//! Shell command execution and the mediated external-command dispatcher.
//!
//! [`AgentCell::execute_shell`] runs a command through [`simulacra_shell`], but
//! the VFS, HTTP, and external-command boundaries all route back through the
//! `AgentCell` Golden Rule chain. Persistent shell `cwd` and `env` survive
//! across calls.

use std::sync::Arc;

use opentelemetry::KeyValue;
use simulacra_types::{JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, VfsError};

use crate::fs_proxy::AgentCellFsProxy;
use crate::guards::{check_and_journal_capability, reserve_turn};
use crate::runtime::SandboxMeters;
use crate::shell_http_proxy::AgentCellShellHttpProxy;
use crate::{AgentCell, SandboxError};

impl AgentCell {
    /// Execute a shell command, checking shell capability and turns budget.
    pub fn execute_shell(
        &self,
        command: &str,
    ) -> Result<simulacra_shell::CommandResult, SandboxError> {
        self.execute_shell_with_workdir(command, None)
    }

    /// Execute a shell command with an optional one-call working directory.
    pub fn execute_shell_with_workdir(
        &self,
        command: &str,
        workdir: Option<&str>,
    ) -> Result<simulacra_shell::CommandResult, SandboxError> {
        // Rebuild interest cache so the callsite is evaluated against the current
        // thread-local subscriber rather than a stale cached decision from a
        // different thread.
        tracing::callsite::rebuild_interest_cache();
        let _span = tracing::info_span!(
            "sandbox_shell_exec",
            simulacra.operation.name = "sandbox_shell_exec",
            simulacra.shell.command = command,
        )
        .entered();

        check_and_journal_capability(
            || self.capability.check_shell(),
            "execute_shell",
            "shell",
            &self.journal,
            &self.agent_id,
        )?;

        if let Some(path) = workdir {
            let metadata = self.metadata(path)?;
            if !metadata.is_dir {
                return Err(SandboxError::Vfs(VfsError::NotADirectory(path.to_string())));
            }
        }

        // Atomically reserve the turn before execution.
        reserve_turn(&self.budget, &self.journal, &self.agent_id)?;

        let shell_start = std::time::Instant::now();

        let env = self
            .shell_env
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_env mutex poisoned: {e}")))?
            .clone();
        let shell_http_proxy = AgentCellShellHttpProxy {
            capability: self.capability.clone(),
            budget: Arc::clone(&self.budget),
            journal: Arc::clone(&self.journal),
            agent_id: self.agent_id.clone(),
            http_client: Arc::clone(&self.http_client),
        };
        let (cwd, previous_cwd) = resolve_cwd(self, workdir)?;
        let shell_vfs = AgentCellFsProxy {
            vfs: Arc::clone(&self.vfs),
            capability: self.capability.clone(),
            budget: Arc::clone(&self.budget),
            journal: Arc::clone(&self.journal),
            agent_id: self.agent_id.clone(),
        };
        let shell_external = AgentCellShellExternal { cell: self };
        let executor =
            simulacra_shell::ShellExecutor::new(&shell_vfs, env, Some(&shell_http_proxy));
        let executor = if workdir.is_some() {
            executor.try_with_cwd(cwd).map_err(SandboxError::Vfs)?
        } else {
            executor.with_cwd(cwd)
        };
        let mut executor = executor.with_external(&shell_external);
        let result = executor.run(command);
        // Persist the environment + cwd for subsequent calls so that
        // `cd /tmp` in one call leaves the next call rooted at /tmp.
        let new_cwd = executor.cwd().to_string();
        let new_env = executor.into_env();
        *self
            .shell_env
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_env mutex poisoned: {e}")))? =
            new_env;
        let cwd_to_store = if workdir.is_some() {
            previous_cwd
        } else {
            new_cwd
        };
        *self
            .shell_cwd
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_cwd mutex poisoned: {e}")))? =
            cwd_to_store;

        self.finish_shell_command(command, shell_start, result)
    }

    fn record_shell_command(&self, command: &str, exit_code: i32) {
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::ShellCommand {
                command: command.to_string(),
                exit_code,
            },
        }) {
            tracing::error!(error = %err, "journal append failed for execute_shell");
        }
    }

    fn record_shell_meters(&self, shell_start: std::time::Instant) {
        let meters = SandboxMeters::get();
        let attrs = &[KeyValue::new("simulacra.agent.id", self.agent_id.0.clone())];
        meters
            .shell_duration
            .record(shell_start.elapsed().as_secs_f64() * 1000.0, attrs);
        meters.shell_requests.add(1, attrs);
    }

    fn finish_shell_command(
        &self,
        command: &str,
        shell_start: std::time::Instant,
        result: simulacra_shell::CommandResult,
    ) -> Result<simulacra_shell::CommandResult, SandboxError> {
        self.record_shell_command(command, result.exit_code);
        self.record_shell_meters(shell_start);
        Ok(result)
    }
}

/// Resolve the cwd for this call. When `workdir` is `Some`, it overrides the
/// persistent cwd for a single call (and the previous persistent cwd is kept).
fn resolve_cwd(cell: &AgentCell, workdir: Option<&str>) -> Result<(String, String), SandboxError> {
    let persistent = || {
        cell.shell_cwd
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_cwd mutex poisoned: {e}")))
    };
    match workdir {
        Some(path) => {
            let previous = persistent()?.clone();
            Ok((path.to_string(), previous))
        }
        None => {
            let cwd = persistent()?.clone();
            Ok((cwd.clone(), cwd))
        }
    }
}

// ── External command dispatch (node / python) ────────────────────

struct AgentCellShellExternal<'a> {
    cell: &'a AgentCell,
}

impl simulacra_shell::ShellExternalCommand for AgentCellShellExternal<'_> {
    fn run_external(
        &self,
        program: &str,
        args: &[String],
        stdin: &str,
        cwd: &str,
    ) -> Option<simulacra_shell::CommandResult> {
        match program {
            "node" | "nodejs" => Some(self.run_node(args, stdin, cwd)),
            #[cfg(feature = "python")]
            "python" | "python3" => Some(self.run_python(args, stdin, cwd)),
            _ => None,
        }
    }
}

impl AgentCellShellExternal<'_> {
    fn run_node(&self, args: &[String], stdin: &str, cwd: &str) -> simulacra_shell::CommandResult {
        if args.is_empty() {
            return usage_error("node <script.js>");
        }

        if args[0] == "-e" {
            return match require_flag_arg(args, "node", "-e") {
                Ok(code) => self.cell.js_shell_result(&code),
                Err(result) => result,
            };
        }

        if args[0] == "-" {
            return self.cell.js_shell_result(stdin);
        }

        let script = resolve_shell_path(&args[0], cwd);
        match self.cell.read_file(&script) {
            Ok(bytes) => {
                let code = String::from_utf8_lossy(&bytes);
                self.cell.js_shell_result(&code)
            }
            Err(e) => open_error("node", &args[0], &e),
        }
    }

    #[cfg(feature = "python")]
    fn run_python(
        &self,
        args: &[String],
        stdin: &str,
        cwd: &str,
    ) -> simulacra_shell::CommandResult {
        if check_and_journal_capability(
            || self.cell.capability.check_python(),
            "execute_python",
            "python",
            &self.cell.journal,
            &self.cell.agent_id,
        )
        .is_err()
        {
            return simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: "python: capability not granted\n".into(),
                exit_code: 1,
            };
        }

        if args.is_empty() {
            return usage_error("python3 <script.py>");
        }

        if args[0] == "-c" {
            return match require_flag_arg(args, "python", "-c") {
                Ok(code) => self.cell.python_shell_result(&code),
                Err(result) => result,
            };
        }

        if args[0] == "-" {
            return self.cell.python_shell_result(stdin);
        }

        let script = resolve_shell_path(&args[0], cwd);
        match self.cell.read_file(&script) {
            Ok(bytes) => {
                let code = String::from_utf8_lossy(&bytes);
                self.cell.python_shell_result(&code)
            }
            Err(e) => open_error("python3", &args[0], &e),
        }
    }
}

/// Resolve a `-e`/`-c` flag argument: `program -e <code>`.
///
/// The caller has already confirmed `args[0] == flag`. Returns `Ok(code)` when
/// the argument is present, or `Err(CommandResult)` when it is missing.
fn require_flag_arg(
    args: &[String],
    program: &str,
    flag: &str,
) -> Result<String, simulacra_shell::CommandResult> {
    if args.len() < 2 {
        return Err(simulacra_shell::CommandResult {
            stdout: String::new(),
            stderr: format!("{program}: option {flag} requires an argument\n"),
            exit_code: 1,
        });
    }
    Ok(args[1..].join(" "))
}

fn usage_error(usage: &str) -> simulacra_shell::CommandResult {
    simulacra_shell::CommandResult {
        stdout: String::new(),
        stderr: format!("Usage: {usage}\n"),
        exit_code: 1,
    }
}

/// Build a "cannot open '<file>': <err>" error result.
fn open_error(program: &str, file: &str, err: &SandboxError) -> simulacra_shell::CommandResult {
    simulacra_shell::CommandResult {
        stdout: String::new(),
        stderr: format!("{program}: cannot open '{file}': {err}\n"),
        exit_code: 1,
    }
}

/// Normalize a path against a working directory into an absolute VFS path.
fn resolve_shell_path(path: &str, cwd: &str) -> String {
    let combined = if path.starts_with('/') {
        path.to_string()
    } else if cwd == "/" {
        format!("/{path}")
    } else {
        format!("{cwd}/{path}")
    };

    let mut parts = Vec::new();
    for part in combined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}
