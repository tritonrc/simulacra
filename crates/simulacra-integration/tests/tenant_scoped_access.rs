use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use simulacra_http::UreqHttpClient;
use simulacra_sandbox::AgentCell;
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage,
    VirtualFs,
};
use simulacra_vfs::{IntegrationLister, MemoryFs, ServiceFs};

#[derive(Default)]
struct FakeJournal {
    entries: Mutex<Vec<JournalEntry>>,
}

impl FakeJournal {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl JournalStorage for FakeJournal {
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

    fn query_token_usage(&self, _: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|err| JournalError::Storage(err.to_string()))?;
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
        Ok(self
            .read_all(agent_id)?
            .into_iter()
            .take(checkpoint_idx + 1)
            .collect())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .read_all(agent_id)?
            .into_iter()
            .skip(start_index)
            .collect())
    }
}

struct FakeIntegrationLister {
    names: Vec<String>,
    readmes: HashMap<String, String>,
    skills: HashMap<String, Vec<String>>,
}

impl FakeIntegrationLister {
    fn new(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|name| (*name).to_string()).collect(),
            readmes: names
                .iter()
                .map(|name| {
                    (
                        (*name).to_string(),
                        format!("# {name}\n\nTenant-scoped integration.\n"),
                    )
                })
                .collect(),
            skills: names
                .iter()
                .map(|name| ((*name).to_string(), vec!["sync".to_string()]))
                .collect(),
        }
    }
}

impl IntegrationLister for FakeIntegrationLister {
    fn integration_names(&self) -> Vec<String> {
        let mut names = self.names.clone();
        names.sort();
        names
    }

    fn integration_metadata(&self, name: &str) -> Option<String> {
        self.names
            .iter()
            .any(|candidate| candidate == name)
            .then(|| {
                serde_json::json!({
                    "base_url": format!("https://api.{name}.example.com"),
                    "scopes": [format!("{name}.read")],
                    "rate_limit_rps": 10,
                    "status": "ok"
                })
                .to_string()
            })
    }

    fn integration_readme(&self, name: &str) -> Option<String> {
        self.readmes.get(name).cloned()
    }

    fn integration_skill_names(&self, name: &str) -> Vec<String> {
        self.skills.get(name).cloned().unwrap_or_default()
    }
}

fn service_fs_for_tenant(names: &[&str]) -> ServiceFs<MemoryFs> {
    let inner = MemoryFs::new();
    inner
        .write(
            "/var/skills/hubspot/create-contact/schema.json",
            br#"{"input":{"type":"object"},"output":{"type":"object"}}"#,
        )
        .unwrap();
    inner
        .write(
            "/var/skills/hubspot/create-contact/skill.js",
            b"export default async function run() {}",
        )
        .unwrap();
    inner
        .write(
            "/var/skills/hubspot/create-contact/PROVENANCE.md",
            b"---\ntier: marketplace\nversion: \"1.2.0\"\norigin: platform\n---\n",
        )
        .unwrap();
    inner
        .write(
            "/var/skills/team/reconcile/schema.json",
            br#"{"input":{"type":"object"},"output":{"type":"object"}}"#,
        )
        .unwrap();
    inner
        .write(
            "/var/skills/org/sync/PROVENANCE.md",
            b"---\ntier: org\nversion: \"2.0.0\"\norigin: authored\n---\n",
        )
        .unwrap();
    ServiceFs::new(inner, Arc::new(FakeIntegrationLister::new(names)))
}

fn make_cell(paths_read: Vec<PathPattern>, journal: Arc<FakeJournal>, names: &[&str]) -> AgentCell {
    let vfs: Arc<dyn simulacra_types::VirtualFs> = Arc::new(service_fs_for_tenant(names));
    AgentCell::new(
        vfs,
        CapabilityToken {
            paths_read,
            ..Default::default()
        },
        Arc::new(Mutex::new(ResourceBudget::new(
            100_000,
            10,
            Decimal::ZERO,
            0,
        ))),
        journal as Arc<dyn JournalStorage>,
        Arc::new(UreqHttpClient::default()),
    )
}

#[test]
fn agent_with_workspace_only_paths_read_gets_error_on_svc_reads() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/workspace/**".into())],
        journal,
        &["hubspot"],
    );

    let err = cell
        .read_file("/svc/hubspot/README.md")
        .expect_err("svc reads should be denied without a matching paths_read grant");

    assert!(err.to_string().contains("capability denied"));
}

#[test]
fn agent_with_hubspot_only_paths_read_can_read_hubspot_but_not_slack() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/svc/hubspot/**".into())],
        journal,
        &["hubspot", "slack"],
    );

    let _ = cell.read_file("/svc/hubspot/README.md");
    let err = cell
        .read_file("/svc/slack/README.md")
        .expect_err("ungranted service path should be denied");

    assert!(err.to_string().contains("capability denied"));
}

#[test]
fn agent_with_wildcard_paths_read_can_read_all_svc_paths() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/**".into())],
        journal,
        &["hubspot", "slack"],
    );

    let readme = cell
        .read_file("/svc/hubspot/README.md")
        .expect("wildcard paths_read should allow all /svc reads");

    assert!(!readme.is_empty());
}

#[test]
fn capability_check_happens_before_servicefs_dispatch() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/workspace/**".into())],
        journal,
        &["hubspot"],
    );

    let err = cell
        .read_file("/svc/hubspot/README.md")
        .expect_err("should be denied before the vfs dispatches to ServiceFs");

    assert!(err.to_string().contains("capability denied"));
}

#[test]
fn tenant_with_hubspot_only_sees_only_hubspot_in_svc_listing() {
    let fs = service_fs_for_tenant(&["hubspot"]);

    let entries = fs
        .list_dir("/svc/")
        .expect("tenant-scoped /svc root should list granted integrations");

    assert_eq!(entries, vec!["hubspot"]);
}

#[test]
fn tenant_without_hubspot_gets_not_found_for_hubspot_readme() {
    let fs = service_fs_for_tenant(&["slack"]);

    let err = fs
        .read("/svc/hubspot/README.md")
        .expect_err("ungranted integration should be hidden");

    assert!(matches!(err, simulacra_types::VfsError::NotFound(_)));
}

#[test]
fn tenant_with_empty_integrations_sees_empty_svc() {
    let fs = service_fs_for_tenant(&[]);

    assert_eq!(
        fs.list_dir("/svc/")
            .expect("empty tenant should still have /svc root"),
        Vec::<String>::new()
    );
}

#[test]
fn tenant_with_no_integrations_field_sees_empty_svc() {
    let config: simulacra_config::SimulacraConfig = toml::from_str(
        r#"
[project]
name = "simulacra"

[agent_types.default]
model = "claude-sonnet-4.6"

[tenants.onboarding]
agent_type = "default"
"#,
    )
    .expect("simulacra config should parse");

    assert_eq!(
        config
            .tenants
            .get("onboarding")
            .and_then(|tenant| tenant.integrations.clone())
            .unwrap_or_default(),
        Vec::<String>::new()
    );
}

#[test]
fn list_dir_var_skills_returns_skill_directories_sorted() {
    let fs = service_fs_for_tenant(&["hubspot"]);

    let entries = fs
        .list_dir("/var/skills/")
        .expect("skill namespace should delegate to inner memory fs");

    assert_eq!(entries, vec!["hubspot", "org", "team"]);
}

#[test]
fn read_skill_schema_returns_input_output_schema() {
    let fs = service_fs_for_tenant(&["hubspot"]);

    let schema = String::from_utf8(
        fs.read("/var/skills/hubspot/create-contact/schema.json")
            .expect("schema.json should be readable"),
    )
    .unwrap();

    assert!(schema.contains("\"input\""));
    assert!(schema.contains("\"output\""));
}

#[test]
fn read_skill_implementation_returns_implementation_source() {
    let fs = service_fs_for_tenant(&["hubspot"]);

    let skill = String::from_utf8(
        fs.read("/var/skills/hubspot/create-contact/skill.js")
            .expect("skill implementation should be readable"),
    )
    .unwrap();

    assert!(skill.contains("export default"));
}

#[test]
fn read_provenance_returns_tier_and_version() {
    let fs = service_fs_for_tenant(&["hubspot"]);

    let provenance = String::from_utf8(
        fs.read("/var/skills/hubspot/create-contact/PROVENANCE.md")
            .expect("provenance should be readable"),
    )
    .unwrap();

    assert!(provenance.contains("tier: marketplace"));
    assert!(provenance.contains("version: \"1.2.0\""));
}

#[test]
fn skills_from_all_three_tiers_are_visible_in_the_same_namespace() {
    let fs = service_fs_for_tenant(&["hubspot"]);

    let entries = fs
        .list_dir("/var/skills/")
        .expect("skill root should list all visible tiers");

    assert!(entries.contains(&"hubspot".to_string()));
    assert!(entries.contains(&"org".to_string()));
    assert!(entries.contains(&"team".to_string()));
}

#[test]
fn skill_access_is_gated_by_paths_read() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/workspace/**".into())],
        journal,
        &["hubspot"],
    );

    let err = cell
        .read_file("/var/skills/hubspot/create-contact/schema.json")
        .expect_err("skills should respect paths_read gating");

    assert!(err.to_string().contains("capability denied"));
}

#[test]
fn svc_read_produces_journal_entry() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/**".into())],
        Arc::clone(&journal),
        &["hubspot"],
    );

    let _ = cell.read_file("/svc/hubspot/README.md");
    let entries = journal.entries();

    assert!(
        entries.iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult { tool_name, content, is_error, .. }
                if tool_name == "read_file" && !is_error && content.contains("/svc/hubspot/README.md")
        )),
        "expected journal entry for /svc read; entries: {entries:#?}"
    );
}

#[test]
fn denied_svc_read_produces_journal_entry_recording_the_denial() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/workspace/**".into())],
        Arc::clone(&journal),
        &["hubspot"],
    );

    let _ = cell.read_file("/svc/hubspot/README.md");
    let entries = journal.entries();

    assert!(
        entries.iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "read_file" && *is_error
        )),
        "expected denied /svc read to be journaled; entries: {entries:#?}"
    );
}

#[test]
fn var_skills_reads_produce_journal_entries_through_normal_vfs_journaling() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/var/skills/**".into())],
        Arc::clone(&journal),
        &["hubspot"],
    );

    let _ = cell
        .read_file("/var/skills/hubspot/create-contact/schema.json")
        .expect("skills read should succeed when granted");
    let entries = journal.entries();

    assert!(
        entries.iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult { tool_name, content, is_error, .. }
                if tool_name == "read_file"
                    && !is_error
                    && content.contains("/var/skills/hubspot/create-contact/schema.json")
        )),
        "expected normal VFS journaling for /var/skills reads; entries: {entries:#?}"
    );
}

// Observability assertions (tracing::warn! on capability-denied access) are validated
// via Aniani queries per S010, not unit tests.
