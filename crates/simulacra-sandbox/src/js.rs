//! JavaScript execution on the mediated QuickJS runtime.
//!
//! [`AgentCell::execute_js`] and [`AgentCell::execute_js_async`] are the entry
//! points. The runtime, module fetcher, FS proxy, and fetch proxy are all built
//! lazily and cached in [`AgentCell::js_runtime`] so that each `eval` reuses the
//! same mediated host configuration.

use std::sync::Arc;

use opentelemetry::KeyValue;
use simulacra_quickjs::{FsProxy, JsOutput, JsRuntime, ModuleFetcher};
use simulacra_types::{BudgetExhausted, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind};
use tracing::Instrument;

use crate::fetch_proxy::AgentCellFetchProxy;
use crate::fs_proxy::AgentCellFsProxy;
use crate::guards::{check_and_journal_capability, reserve_turn};
use crate::module_fetcher::AgentCellModuleFetcher;
use crate::runtime::SandboxMeters;
use crate::{AgentCell, SandboxError};

impl AgentCell {
    /// Execute JavaScript code, checking javascript capability and turns budget.
    ///
    /// The JS runtime's `fs.readFileSync`/`fs.writeFileSync` and `simulacra:fs`
    /// `readFile`/`writeFile` route through [`read_file`](Self::read_file) and
    /// [`write_file`](Self::write_file) for capability checking. Remote module
    /// imports route through [`fetch_http`](Self::fetch_http).
    pub fn execute_js(&self, code: &str) -> Result<JsOutput, SandboxError> {
        // Rebuild interest cache so the callsite is evaluated against the current
        // thread-local subscriber rather than a stale cached decision from a
        // different thread.
        tracing::callsite::rebuild_interest_cache();
        let _span = tracing::info_span!(
            "sandbox_js_exec",
            simulacra.operation.name = "sandbox_js_exec",
        )
        .entered();

        check_and_journal_capability(
            || self.capability.check_javascript(),
            "execute_js",
            "javascript",
            &self.journal,
            &self.agent_id,
        )?;

        reserve_turn(&self.budget, &self.journal, &self.agent_id)?;

        let _script_permit = self
            .script_executor
            .as_ref()
            .map(|executor| {
                executor
                    .try_acquire_permit()
                    .map_err(|e| SandboxError::Internal(format!("script executor error: {e}")))
            })
            .transpose()?;

        let js_start = std::time::Instant::now();
        let runtime = self.prepare_js_runtime();
        let output =
            runtime.and_then(|rt| rt.eval(code).map_err(|e| self.map_js_execution_error(e)));
        self.finish_js_execution(js_start);
        output
    }

    /// Async entry point for JavaScript execution.
    ///
    /// This awaits script-executor backpressure once at the `AgentCell` boundary,
    /// then executes through the same mediated JS runtime wrapper.
    pub async fn execute_js_async(&self, code: &str) -> Result<JsOutput, SandboxError> {
        tracing::callsite::rebuild_interest_cache();
        async move {
            check_and_journal_capability(
                || self.capability.check_javascript(),
                "execute_js",
                "javascript",
                &self.journal,
                &self.agent_id,
            )?;
            reserve_turn(&self.budget, &self.journal, &self.agent_id)?;

            let _script_permit =
                match self.script_executor.as_ref() {
                    Some(executor) => Some(executor.acquire_permit().await.map_err(|e| {
                        SandboxError::Internal(format!("script executor error: {e}"))
                    })?),
                    None => None,
                };

            let js_start = std::time::Instant::now();
            let runtime = self.prepare_js_runtime();
            let output =
                runtime.and_then(|rt| rt.eval(code).map_err(|e| self.map_js_execution_error(e)));
            self.finish_js_execution(js_start);
            output
        }
        .instrument(tracing::info_span!(
            "sandbox_js_exec",
            simulacra.operation.name = "sandbox_js_exec",
        ))
        .await
    }

    pub(crate) fn js_shell_result(&self, code: &str) -> simulacra_shell::CommandResult {
        match self.execute_js(code) {
            Ok(js_out) => {
                let mut stdout = js_out.stdout;
                if let Some(ref result) = js_out.result
                    && !result.is_empty()
                    && result != "undefined"
                {
                    stdout.push_str(result);
                    stdout.push('\n');
                }
                simulacra_shell::CommandResult {
                    stdout,
                    stderr: String::new(),
                    exit_code: js_out.exit_code.unwrap_or(0),
                }
            }
            Err(e) => simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: format!("{e}\n"),
                exit_code: 1,
            },
        }
    }

    /// Lazily build (and cache) the mediated QuickJS runtime.
    pub(crate) fn prepare_js_runtime(&self) -> Result<JsRuntime, SandboxError> {
        let mut rt_slot = self.js_runtime.lock()?;
        if rt_slot.is_none() {
            let fs_proxy: Arc<dyn FsProxy> = Arc::new(AgentCellFsProxy {
                vfs: Arc::clone(&self.vfs),
                capability: self.capability.clone(),
                budget: Arc::clone(&self.budget),
                journal: Arc::clone(&self.journal),
                agent_id: self.agent_id.clone(),
            });

            let stubs = self
                .module_stubs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let fetcher: Box<dyn ModuleFetcher> = Box::new(AgentCellModuleFetcher {
                capability: self.capability.clone(),
                budget: Arc::clone(&self.budget),
                journal: Arc::clone(&self.journal),
                agent_id: self.agent_id.clone(),
                http_client: Arc::clone(&self.http_client),
                stubs,
            });

            let fetch_proxy: Arc<dyn simulacra_fetch::FetchProxy> = Arc::new(AgentCellFetchProxy {
                capability: self.capability.clone(),
                budget: Arc::clone(&self.budget),
                journal: Arc::clone(&self.journal),
                agent_id: self.agent_id.clone(),
                http_client: Arc::clone(&self.http_client),
                integration_registry: self.integration_registry.clone(),
                tenant_integrations: self.tenant_integrations.clone(),
            });

            let runtime = JsRuntime::with_all_options(
                Arc::clone(&self.vfs),
                std::time::Duration::from_secs(5),
                Some(fetcher),
                Some(fs_proxy),
                Some(fetch_proxy),
            )
            .map_err(|e| SandboxError::Js(e.to_string()))?;
            *rt_slot = Some(runtime);
        }

        rt_slot
            .as_ref()
            .cloned()
            .ok_or_else(|| SandboxError::Internal("JS runtime not initialized".into()))
    }

    fn map_js_execution_error(&self, error: simulacra_quickjs::JsError) -> SandboxError {
        // If execution failed due to budget exhaustion (e.g. a module fetch hit
        // the turns limit), surface it as BudgetExhausted so callers get a
        // structured error instead of a generic JS error string.
        let budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
        if budget.max_turns > 0 && budget.used_turns >= budget.max_turns {
            SandboxError::BudgetExhausted(BudgetExhausted {
                resource: "turns".into(),
                used: budget.used_turns.to_string(),
                limit: budget.max_turns.to_string(),
            })
        } else {
            SandboxError::Js(error.to_string())
        }
    }

    fn finish_js_execution(&self, js_start: std::time::Instant) {
        // Journal the code execution BEFORE returning (even on failure).
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::CodeExecution {
                language: "javascript".to_string(),
            },
        }) {
            tracing::error!(error = %err, "journal append failed for execute_js");
        }

        // S010: Record OTel meter observations for JS execution.
        let meters = SandboxMeters::get();
        let attrs = &[KeyValue::new("simulacra.agent.id", self.agent_id.0.clone())];
        meters
            .js_duration
            .record(js_start.elapsed().as_secs_f64() * 1000.0, attrs);
        meters.js_requests.add(1, attrs);
    }
}
