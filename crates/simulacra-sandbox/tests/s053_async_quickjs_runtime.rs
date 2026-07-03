use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use simulacra_sandbox::AgentCell;
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, NetworkPermission, PathPattern, ResourceBudget,
    TokenUsage, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl FakeJournalStorage {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries
            .lock()
            .expect("journal lock should not be poisoned")
            .clone()
    }
}

impl JournalStorage for FakeJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries
            .lock()
            .expect("journal lock should not be poisoned")
            .push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .expect("journal lock should not be poisoned")
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

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
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
        self.spans
            .lock()
            .expect("span lock should not be poisoned")
            .push(CapturedSpan { fields });
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
}

fn capture_spans<R>(operation: impl FnOnce() -> R) -> (R, Vec<CapturedSpan>) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
    });
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans
        .lock()
        .expect("span lock should not be poisoned")
        .clone();
    (result, spans)
}

struct Harness {
    cell: AgentCell,
    journal: Arc<FakeJournalStorage>,
    budget: Arc<Mutex<ResourceBudget>>,
}

impl Harness {
    fn new(capability: CapabilityToken) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs;
        let journal = Arc::new(FakeJournalStorage::default());
        let journal_dyn: Arc<dyn JournalStorage> = journal.clone();
        let budget = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = AgentCell::new(
            vfs_dyn,
            capability,
            Arc::clone(&budget),
            journal_dyn,
            http_client,
        );
        Self {
            cell,
            journal,
            budget,
        }
    }

    fn used_turns(&self) -> u64 {
        serde_json::to_value(
            &*self
                .budget
                .lock()
                .expect("budget lock should not be poisoned"),
        )
        .expect("budget should serialize")
        .get("used_turns")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
    }
}

fn capability(network: &[&str]) -> CapabilityToken {
    CapabilityToken {
        javascript: true,
        network: network
            .iter()
            .map(|permission| NetworkPermission((*permission).to_string()))
            .collect(),
        paths_read: vec![PathPattern("/workspace/**".to_string())],
        paths_write: vec![PathPattern("/workspace/**".to_string())],
        ..Default::default()
    }
}

fn has_http_request(entries: &[JournalEntry], url: &str) -> bool {
    entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::HttpRequest { method, url: entry_url, status }
                if method == "GET" && entry_url == url && *status == 200
        )
    })
}

#[test]
fn js_exec_success_output_and_javascript_exceptions_remain_compatible() {
    let harness = Harness::new(capability(&[]));

    let output = harness
        .cell
        .execute_js(
            r#"
            console.log("s053 stdout");
            40 + 2;
            "#,
        )
        .expect("successful js_exec behavior should remain compatible");
    assert_eq!(output.stdout, "s053 stdout\n");
    assert_eq!(output.result.as_deref(), Some("42"));

    let error = harness
        .cell
        .execute_js(r#"throw new Error("s053 js exception")"#)
        .expect_err("JS exceptions should still surface as sandbox JS errors")
        .to_string();
    assert!(
        error.contains("s053 js exception"),
        "expected JS exception message to survive async substrate refactor, got {error:?}"
    );

    let code_execution_entries = harness
        .journal
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        })
        .count();
    assert_eq!(
        code_execution_entries, 2,
        "successful and failing js_exec calls should both journal CodeExecution"
    );
}

#[test]
fn static_remote_module_prefetch_uses_agent_cell_module_fetcher_for_transitive_imports() {
    let harness = Harness::new(capability(&["net:modules.invalid"]));
    let entry_url = "https://modules.invalid/entry.js";
    let leaf_url = "https://modules.invalid/leaf.js";
    harness.cell.register_module_stub(
        entry_url,
        r#"
        import leaf from "https://modules.invalid/leaf.js";
        export default `entry:${leaf}`;
        "#,
    );
    harness
        .cell
        .register_module_stub(leaf_url, r#"export default "leaf";"#);
    let turns_before = harness.used_turns();

    let (output, spans) = capture_spans(|| {
        harness.cell.execute_js(
            r#"
            import value from "https://modules.invalid/entry.js";
            value;
            "#,
        )
    });
    let output = output.expect("static remote import graph should execute through AgentCell");

    assert_eq!(output.result.as_deref(), Some("entry:leaf"));
    assert_eq!(
        harness.used_turns(),
        turns_before + 1,
        "remote static module prefetch should share the enclosing execute_js turn"
    );
    let entries = harness.journal.entries();
    assert!(
        has_http_request(&entries, entry_url),
        "entry remote module should produce an HttpRequest journal entry through AgentCellModuleFetcher"
    );
    assert!(
        has_http_request(&entries, leaf_url),
        "transitive remote module should produce an HttpRequest journal entry through AgentCellModuleFetcher"
    );
    for url in [entry_url, leaf_url] {
        assert!(
            spans.iter().any(|span| {
                span.fields.get("simulacra.operation.name") == Some(&"module_fetch".to_string())
                    && span.fields.get("simulacra.module.url") == Some(&url.to_string())
            }),
            "expected module_fetch span for {url}, got {spans:#?}"
        );
    }
}

#[test]
fn dynamic_remote_imports_fail_closed_before_agent_cell_module_fetch() {
    let harness = Harness::new(capability(&["net:modules.invalid"]));
    let url = "https://modules.invalid/dynamic.js";
    harness
        .cell
        .register_module_stub(url, r#"export default "dynamic-loaded";"#);

    let error = harness
        .cell
        .execute_js(
            r#"
            export {};
            const url = "https://modules.invalid/dynamic.js";
            const module = await import(url);
            module.default;
            "#,
        )
        .expect_err("unprefetched dynamic remote import should fail closed")
        .to_string();

    let lower = error.to_lowercase();
    assert!(
        lower.contains("dynamic") && lower.contains("prefetch"),
        "dynamic remote import should explain the fail-closed prefetch policy, got {error:?}"
    );
    assert!(
        error.contains(url),
        "dynamic remote import error should include the rejected URL, got {error:?}"
    );
    assert!(
        !has_http_request(&harness.journal.entries(), url),
        "unprefetched dynamic remote imports must fail before AgentCellModuleFetcher journals HttpRequest"
    );
}
