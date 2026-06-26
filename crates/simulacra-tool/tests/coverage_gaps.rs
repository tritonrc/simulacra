#![cfg(feature = "sandbox")]

//! Additional behavioural tests for simulacra-tool coverage gaps.
//!
//! Covers: FT1 (SkillTool surface), FT2 (skill frontmatter parsing),
//! FT6 (file_edit error paths), FT7 (list_dir on file), FT8 (capability denial
//! for file_write, shell_exec, list_dir), GFT3 (list_dir missing path),
//! GFT5 (file_write budget exhaustion via max_turns).

use rust_decimal::Decimal;
use serde_json::{Value, json};
use simulacra_sandbox::AgentCell;
use simulacra_tool::{
    SkillMeta, SkillTool, ToolError, ToolRegistry, parse_skill_frontmatter, register_builtins,
};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage, Tool,
    VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::future::Future;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Shared fakes and helpers (mirrors s012_builtins_red.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
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
            .filter(|entry| entry.agent_id == *agent_id)
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

struct Harness {
    registry: ToolRegistry,
    vfs: Arc<MemoryFs>,
    #[allow(dead_code)]
    cell: Arc<AgentCell>,
}

impl Harness {
    fn new(capability: CapabilityToken, budget: ResourceBudget) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = Arc::new(AgentCell::new(
            vfs_dyn,
            capability,
            Arc::new(Mutex::new(budget)),
            journal,
            http_client,
        ));
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, Arc::clone(&cell));

        Self {
            registry,
            vfs,
            cell,
        }
    }
}

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn full_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn unlimited_budget() -> ResourceBudget {
    ResourceBudget::new(0, 0, Decimal::ZERO, 0)
}

fn call_tool(
    harness: &Harness,
    name: &str,
    arguments: Value,
    capability: &CapabilityToken,
) -> Result<Value, ToolError> {
    run_async(harness.registry.call(name, arguments, capability))
}

fn assert_error_result_contains(value: &Value, expected_substring: &str) {
    assert_eq!(
        value.get("is_error").and_then(Value::as_bool),
        Some(true),
        "expected an error-shaped tool result, got {value:?}"
    );

    let rendered = value.to_string().to_ascii_lowercase();
    assert!(
        rendered.contains(&expected_substring.to_ascii_lowercase()),
        "expected {value:?} to mention {expected_substring:?}"
    );
}

fn assert_invalid_arguments(result: Result<Value, ToolError>) {
    match result {
        Err(ToolError::InvalidArguments(_)) => {}
        other => panic!("expected invalid arguments error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// FT6: file_edit missing old_string / new_string error paths
// ---------------------------------------------------------------------------

#[test]
fn file_edit_without_old_string_returns_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"content")
        .unwrap();

    assert_invalid_arguments(call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "new_string": "replacement"
        }),
        &capability,
    ));
}

#[test]
fn file_edit_without_new_string_returns_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"content")
        .unwrap();

    assert_invalid_arguments(call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "content"
        }),
        &capability,
    ));
}

// ---------------------------------------------------------------------------
// FT7: list_dir on a file path (not a directory)
// ---------------------------------------------------------------------------

#[test]
fn list_dir_on_a_file_returns_error_result() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/readme.md", b"hello").unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace/readme.md" }),
        &capability,
    )
    .expect("list_dir on a file should return a user-facing error result, not a ToolError");

    assert_error_result_contains(&result, "not a directory");
}

// ---------------------------------------------------------------------------
// GFT3: list_dir without path argument
// ---------------------------------------------------------------------------

#[test]
fn list_dir_without_path_argument_returns_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "list_dir", json!({}), &capability));
}

// ---------------------------------------------------------------------------
// FT8: capability-denial tests for file_write, shell_exec, list_dir
// ---------------------------------------------------------------------------

fn no_write_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![], // no write paths
        ..Default::default()
    }
}

fn no_shell_capability() -> CapabilityToken {
    CapabilityToken {
        shell: false, // shell denied
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn no_read_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![], // no read paths
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

#[test]
fn file_write_with_denied_write_capability_returns_capability_denied() {
    let capability = no_write_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/secret.txt",
            "content": "should be denied"
        }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error for file_write, got {other:?}"),
    }
}

#[test]
fn shell_exec_with_denied_shell_capability_returns_capability_denied() {
    let capability = no_shell_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error for shell_exec, got {other:?}"),
    }
}

#[test]
fn list_dir_with_denied_read_capability_returns_capability_denied() {
    let capability = no_read_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/file.txt", b"data").unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error for list_dir, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// GFT5: file_write budget exhaustion via max_vfs_bytes
// (file_write checks VFS bytes budget, not turns budget)
// ---------------------------------------------------------------------------

fn budget_with_vfs_bytes_exhausted() -> ResourceBudget {
    ResourceBudget {
        max_vfs_bytes: 1,
        used_vfs_bytes: 1,
        ..ResourceBudget::new(0, 0, Decimal::ZERO, 0)
    }
}

#[test]
fn file_write_with_exhausted_vfs_bytes_budget_returns_execution_failed() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), budget_with_vfs_bytes_exhausted());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/budget.txt",
            "content": "data"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            let lower = message.to_ascii_lowercase();
            assert!(
                lower.contains("vfs_bytes"),
                "expected budget error to mention 'vfs_bytes', got: {message}"
            );
        }
        other => panic!("expected execution failed error for budget exhaustion, got {other:?}"),
    }
}

// shell_exec checks turns budget; verify it surfaces as ExecutionFailed.
fn budget_with_turns_exhausted() -> ResourceBudget {
    ResourceBudget {
        used_turns: 1,
        ..ResourceBudget::new(0, 1, Decimal::ZERO, 0)
    }
}

#[test]
fn shell_exec_with_exhausted_turns_budget_returns_execution_failed() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), budget_with_turns_exhausted());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello" }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            let lower = message.to_ascii_lowercase();
            assert!(
                lower.contains("turns"),
                "expected budget error to mention 'turns', got: {message}"
            );
        }
        other => panic!("expected execution failed error for budget exhaustion, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// FT2: parse_skill_frontmatter unit tests
// ---------------------------------------------------------------------------

#[test]
fn parse_skill_frontmatter_extracts_name_and_description() {
    let content = "\
---
name: code-review
description: Review code for quality
---
# Code Review Skill

Detailed instructions here.
";
    let meta = parse_skill_frontmatter(content, "/skills/cr/SKILL.md").unwrap();
    assert_eq!(meta.name, "code-review");
    assert_eq!(meta.description, "Review code for quality");
    assert_eq!(meta.vfs_path, "/skills/cr/SKILL.md");
    assert!(!meta.disable_model_invocation);
    assert!(meta.user_invocable);
    assert!(meta.allowed_tools.is_empty());
}

#[test]
fn parse_skill_frontmatter_missing_opening_delimiter_returns_error() {
    let content = "name: oops\n---\nBody here.\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("frontmatter"),
        "expected error about frontmatter, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_missing_closing_delimiter_returns_error() {
    let content = "---\nname: oops\ndescription: bad\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("closing"),
        "expected error about closing delimiter, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_missing_name_field_returns_error() {
    let content = "---\ndescription: no name\n---\nBody here.\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("name"),
        "expected error about missing name, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_missing_description_field_returns_error() {
    let content = "---\nname: orphan\n---\nBody here.\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("description"),
        "expected error about missing description, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_empty_body_returns_error() {
    let content = "---\nname: empty\ndescription: no body\n---\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("body"),
        "expected error about missing body, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_reads_disable_model_invocation() {
    let content = "\
---
name: internal
description: Internal only
disable_model_invocation: true
---
# Internal Skill

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/internal/SKILL.md").unwrap();
    assert!(meta.disable_model_invocation);
}

#[test]
fn parse_skill_frontmatter_reads_user_invocable_false() {
    let content = "\
---
name: hidden
description: Not user-invocable
user_invocable: false
---
# Hidden Skill

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/hidden/SKILL.md").unwrap();
    assert!(!meta.user_invocable);
}

#[test]
fn parse_skill_frontmatter_reads_allowed_tools_list() {
    let content = "\
---
name: builder
description: Build things
allowed_tools:
- shell_exec
- file_write
---
# Builder Skill

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/builder/SKILL.md").unwrap();
    assert_eq!(meta.allowed_tools, vec!["shell_exec", "file_write"]);
}

#[test]
fn parse_skill_frontmatter_populates_body_field() {
    let content = "\
---
name: test
description: Test skill
---
# Test Skill

This is the body.
";
    let meta = parse_skill_frontmatter(content, "/skills/test/SKILL.md").unwrap();
    assert!(meta.body.is_some());
    let body = meta.body.unwrap();
    assert!(
        body.contains("This is the body"),
        "expected body to contain content, got: {body}"
    );
}

// ---------------------------------------------------------------------------
// FT1: SkillTool surface tests
// ---------------------------------------------------------------------------

fn make_skill_tool(
    vfs: &Arc<MemoryFs>,
    catalog: Vec<SkillMeta>,
) -> (SkillTool, Arc<AgentCell>, Arc<MemoryFs>) {
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let capability = full_capability();
    let budget = unlimited_budget();
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs_dyn,
        capability,
        Arc::new(Mutex::new(budget)),
        journal,
        http_client,
    ));
    let tool = SkillTool::new(Arc::clone(&cell), catalog);
    (tool, cell, vfs.clone())
}

fn sample_skill_content() -> &'static str {
    "\
---
name: code-review
description: Review code for quality
---
# Code Review

Review the code carefully.
"
}

#[test]
fn skill_tool_definition_name_is_skill() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let def = tool.definition();
    assert_eq!(def.name, "Skill");
}

#[test]
fn skill_tool_definition_schema_requires_command() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let def = tool.definition();
    let required = def
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .unwrap();
    assert!(required.contains(&json!("command")));
}

#[test]
fn skill_tool_definition_includes_catalog_description_for_model_visible_skills() {
    let vfs = Arc::new(MemoryFs::new());
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/cr/SKILL.md".into(),
        disable_model_invocation: false,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let def = tool.definition();
    assert!(
        def.description.contains("code-review"),
        "expected skill catalog in description, got: {}",
        def.description
    );
    assert!(
        def.description.contains("Review code for quality"),
        "expected skill description in definition, got: {}",
        def.description
    );
}

#[test]
fn skill_tool_definition_excludes_model_disabled_skills_from_catalog() {
    let vfs = Arc::new(MemoryFs::new());
    let catalog = vec![SkillMeta {
        name: "internal-only".into(),
        description: "Should not appear".into(),
        vfs_path: "/skills/internal/SKILL.md".into(),
        disable_model_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let def = tool.definition();
    assert!(
        !def.description.contains("internal-only"),
        "model-disabled skill should be excluded from catalog, got: {}",
        def.description
    );
}

#[test]
fn skill_tool_call_with_unknown_command_returns_error_result() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let capability = full_capability();

    let result = run_async(tool.call(json!({ "command": "nonexistent" }), &capability))
        .expect("unknown skill should return error result, not ToolError");

    assert_error_result_contains(&result, "unknown skill");
}

#[test]
fn skill_tool_call_without_command_returns_invalid_arguments() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let capability = full_capability();

    let result = run_async(tool.call(json!({}), &capability));
    assert_invalid_arguments(result);
}

#[test]
fn skill_tool_call_with_model_disabled_skill_returns_error_result() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write(
        "/skills/internal/SKILL.md",
        sample_skill_content().as_bytes(),
    )
    .unwrap();
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/internal/SKILL.md".into(),
        disable_model_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let capability = full_capability();

    let result = run_async(tool.call(json!({ "command": "code-review" }), &capability))
        .expect("model-disabled skill should return error result");

    assert_error_result_contains(&result, "disable_model_invocation");
}

#[test]
fn skill_tool_call_with_capability_denied_skill_returns_error_result() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/skills/cr/SKILL.md", sample_skill_content().as_bytes())
        .unwrap();
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/cr/SKILL.md".into(),
        disable_model_invocation: false,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    // Restrict capability to deny the skill
    let capability = CapabilityToken {
        skill_patterns: vec!["skill:other-skill".into()],
        paths_read: vec![PathPattern("/**".into())],
        ..Default::default()
    };

    let result = run_async(tool.call(json!({ "command": "code-review" }), &capability))
        .expect("denied skill should return error result");

    assert_error_result_contains(&result, "not allowed");
}

#[test]
fn skill_tool_call_with_valid_skill_returns_markdown_body() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/skills/cr/SKILL.md", sample_skill_content().as_bytes())
        .unwrap();
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/cr/SKILL.md".into(),
        disable_model_invocation: false,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let capability = full_capability();

    let result = run_async(tool.call(json!({ "command": "code-review" }), &capability))
        .expect("valid skill call should succeed");

    let body = result.as_str().expect("result should be a string");
    assert!(
        body.contains("Review the code carefully"),
        "expected skill body content, got: {body}"
    );
    // Should NOT contain frontmatter
    assert!(
        !body.contains("---"),
        "frontmatter should be stripped, got: {body}"
    );
}

#[test]
fn skill_tool_catalog_truncation_indicates_partial_when_budget_exceeded() {
    let vfs = Arc::new(MemoryFs::new());
    // Create a catalog with many skills that exceed a small budget
    let mut catalog = Vec::new();
    for i in 0..100 {
        catalog.push(SkillMeta {
            name: format!("skill-with-a-very-long-name-number-{i}"),
            description: format!("This is a very long description for skill number {i} that should consume budget quickly"),
            vfs_path: format!("/skills/s{i}/SKILL.md"),
            disable_model_invocation: false,
            user_invocable: true,
            allowed_tools: vec![],
            body: Some("body".into()),
        });
    }
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let def = tool.definition();

    // The default budget is 4096 chars; 100 skills with long names should exceed it
    assert!(
        def.description.contains("partial"),
        "expected partial catalog indication when budget exceeded, got: {}",
        def.description
    );
}
