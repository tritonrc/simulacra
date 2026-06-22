use rust_decimal::Decimal;
use serde_json::Value;
use simulacra_sandbox::AgentCell;
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, NetworkPermission, PathPattern, ResourceBudget,
    TokenUsage, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl FakeJournalStorage {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl JournalStorage for FakeJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| &entry.agent_id == agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|error| JournalError::Storage(error.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
    current_span: Option<String>,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan { fields });
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            level: event.metadata().level().to_string(),
            current_span: ctx.lookup_current().map(|span| span.name().to_string()),
            fields,
        });
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

#[allow(clippy::type_complexity)]
fn capture_operation<R>(
    operation: impl FnOnce() -> R,
) -> (R, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

struct Harness {
    vfs: Arc<MemoryFs>,
    cell: AgentCell,
}

impl Harness {
    fn new(
        capability: CapabilityToken,
        budget: Arc<Mutex<ResourceBudget>>,
        journal: Arc<FakeJournalStorage>,
    ) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal_dyn: Arc<dyn JournalStorage> = journal;
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = AgentCell::new(vfs_dyn, capability, budget, journal_dyn, http_client);

        // Register well-known module stubs for integration tests that import
        // from .invalid domains (which have no real HTTP server).
        cell.register_module_stub(
            "https://modules.invalid/write-secret.js",
            r#"
            import { writeFile } from "simulacra:fs";
            export function writeSecret() { writeFile("/workspace/secret.txt", "secret data"); }
            "#,
        );

        Self { vfs, cell }
    }
}

fn capability_with_network(
    reads: &[&str],
    writes: &[&str],
    network: &[&str],
    javascript: bool,
) -> CapabilityToken {
    CapabilityToken {
        network: network
            .iter()
            .map(|permission| NetworkPermission((*permission).to_string()))
            .collect(),
        javascript,
        paths_read: reads
            .iter()
            .map(|pattern| PathPattern((*pattern).to_string()))
            .collect(),
        paths_write: writes
            .iter()
            .map(|pattern| PathPattern((*pattern).to_string()))
            .collect(),
        ..Default::default()
    }
}

fn unlimited_budget() -> Arc<Mutex<ResourceBudget>> {
    Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)))
}

fn budget_counter(budget: &Arc<Mutex<ResourceBudget>>, field: &str) -> u64 {
    serde_json::to_value(&*budget.lock().unwrap())
        .expect("budget should serialize")
        .get(field)
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[test]
fn remote_module_fetch_uses_agent_cell_proxy_for_budget_journal_and_span_emission() {
    let url = "https://modules.invalid/entry.js";
    let journal = Arc::new(FakeJournalStorage::default());
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], true),
        Arc::clone(&budget),
        Arc::clone(&journal),
    );
    let before = budget_counter(&budget, "used_turns");

    let (_result, spans, _events) = capture_operation(|| {
        let _ = harness.cell.execute_js(&format!(
            r#"
            import value from "{url}";
            value;
            "#
        ));
    });

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "remote module fetches should consume one turns-budget unit through AgentCell::fetch_http"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::HttpRequest { method, url: entry_url, .. }
                    if method == "GET" && entry_url == url
            )
        }),
        "expected remote module fetches to write an HttpRequest journal entry via the AgentCell proxy"
    );
    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"module_fetch".to_string())
                && span.fields.get("simulacra.module.url") == Some(&url.to_string())
        }),
        "expected remote module fetches to emit a module_fetch span with the requested URL"
    );
}

#[test]
fn remote_module_code_calling_simulacra_fs_still_hits_agent_cell_capability_checks() {
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .execute_js(
            r#"
            import { writeSecret } from "https://modules.invalid/write-secret.js";
            writeSecret();
            "#,
        )
        .expect_err("remote module fs access should be denied by the AgentCell fs proxy")
        .to_string();

    assert!(
        error.contains("/workspace/secret.txt") && error.contains("denied"),
        "expected remote module fs access to surface the denied path, got {error}"
    );
    assert!(
        !harness.vfs.exists("/workspace/secret.txt"),
        "denied remote-module fs writes must not mutate the VFS"
    );
}

#[test]
fn remote_module_url_capability_denials_emit_warn_events_with_module_fetch_metadata() {
    let (_result, _spans, events) = capture_operation(|| {
        let harness = Harness::new(
            capability_with_network(&[], &[], &[], true),
            unlimited_budget(),
            Arc::new(FakeJournalStorage::default()),
        );

        let _ = harness.cell.execute_js(
            r#"
            import denied from "https://denied.invalid/pkg.js";
            denied;
            "#,
        );
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && matches!(
                    event.current_span.as_deref(),
                    Some("module_fetch" | "sandbox_http_fetch")
                )
                && event.fields.get("simulacra.capability.operation")
                    == Some(&"module_fetch".to_string())
                && event.fields.contains_key("simulacra.capability.reason")
        }),
        "expected WARN capability-denial event for remote module fetches, got {events:#?}"
    );
}
