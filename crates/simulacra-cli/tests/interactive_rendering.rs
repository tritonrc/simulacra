use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use simulacra_cli::interactive::{
    InteractiveInput, InteractiveOutput, InteractiveSession, InteractiveSessionConfig, StreamEvent,
};
use simulacra_cli::{CliArgs, CliMode, bootstrap};
use simulacra_runtime::{InMemorySessionStorage, SessionStorage};
use simulacra_types::{
    FinishReason, Message, Provider, ProviderError, ProviderResponse, Role, TokenUsage,
    ToolCallMessage, ToolDefinition, VirtualFs,
};
use simulacra_vfs::MemoryFs;

#[derive(Debug)]
struct FakeProvider;

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut simulacra_types::ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async {
            Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    content: "ok".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_content: vec![],
                },
                token_usage: TokenUsage::default(),
                finish_reason: FinishReason::EndTurn,
                provider_response_id: Some("resp-1".into()),
                model: "claude-sonnet-4-20250514".into(),
            })
        })
    }
}

#[derive(Debug, Default, Clone)]
struct TestIo {
    tty: bool,
    writes: Vec<String>,
    approvals: VecDeque<String>,
}

impl TestIo {
    fn tty() -> Self {
        Self {
            tty: true,
            ..Self::default()
        }
    }
}

impl InteractiveInput for TestIo {
    fn read_line(&mut self) -> Option<String> {
        None
    }

    fn read_approval(&mut self) -> Option<String> {
        self.approvals.pop_front()
    }

    fn is_tty(&self) -> bool {
        self.tty
    }
}

impl InteractiveOutput for TestIo {
    fn write_line(&mut self, line: &str) {
        self.writes.push(line.to_string());
    }

    fn clear(&mut self) {}

    fn restore_terminal(&mut self) {}
}

fn unique_path(name: &str) -> PathBuf {
    static NEXT_PATH_ID: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "simulacra-cli-s018-{name}-{stamp}-{}-{}.toml",
        std::process::id(),
        NEXT_PATH_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn write(contents: &str) -> Self {
        let path = unique_path("config");
        fs::write(&path, contents).expect("temp config should be written");
        Self { path }
    }

    fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn interactive_config_toml() -> String {
    r#"[project]
name = "simulacra-s018"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 7
max_tokens = 4321
can_spawn = ["researcher"]

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]

[agent_types.researcher]
model = "gpt-5.4"
system_prompt = "You are the child researcher."
max_turns = 3
max_tokens = 222

[agent_types.researcher.capabilities]
paths_read = ["/workspace/**"]

[task]
entry_agent = "default"
task = "interactive parent task"
"#
    .into()
}

fn build_session() -> InteractiveSession<FakeProvider, TestIo> {
    let config = TempConfig::write(&interactive_config_toml());
    let boot = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: Some("interactive parent task".into()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
        output_format: simulacra_cli::OutputFormat::Text,
    })
    .expect("bootstrap should succeed");
    let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());

    let can_spawn = boot
        .config
        .agent_types
        .get("default")
        .map(|a| a.can_spawn.clone())
        .unwrap_or_default();
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    InteractiveSession::new(
        TestIo::tty(),
        Arc::new(FakeProvider),
        storage,
        vfs,
        InteractiveSessionConfig {
            project_name: boot.config.project.name.clone(),
            model: boot.model.clone(),
            max_tokens: boot.resource_budget.max_tokens,
            max_turns: boot.resource_budget.max_turns,
            task: Some("interactive parent task".into()),
            requested_session_id: None,
            tool_definitions: boot.tool_definitions.clone(),
            can_spawn,
            skill_catalog: vec![],
        },
    )
}

fn contains_text(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

// ---------------------------------------------------------------------------
// Tool definition and result shape
// ---------------------------------------------------------------------------

#[test]
fn interactive_sessions_register_spawn_agent_and_list_it_in_tools_output() {
    let mut session = build_session();

    let view = session.dispatch_command("/tools");

    for tool_name in [
        "spawn_agent",
        "join_child_agent",
        "cancel_child_agent",
        "steer_child_agent",
        "child_status",
        "wait_child_agent",
        "close_child_agent",
    ] {
        assert!(
            contains_text(&view.visible_output, tool_name),
            "interactive /tools output should include {tool_name}"
        );
    }
    for expected in [
        "concrete, bounded, independent subtask",
        "returns a live child handle, not a final answer",
        "join_child_agent when the terminal result is needed",
    ] {
        assert!(
            contains_text(&view.visible_output, expected),
            "interactive /tools output should include spawn_agent guidance {expected:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Child result flow back to parent
// ---------------------------------------------------------------------------

#[test]
fn parent_spawn_tool_results_are_the_only_child_visible_messages_added_to_parent_history() {
    let mut session = build_session();
    session.start();
    session.view.messages.push(Message {
        role: Role::Tool,
        content: r#"{"child_id":"child-1","agent_type":"researcher","message":"summary"}"#.into(),
        tool_calls: vec![],
        tool_call_id: Some("call-1".into()),
        provider_content: vec![],
    });

    assert_eq!(
        session
            .snapshot()
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .count(),
        1,
        "the parent transcript should only contain the final spawn_agent tool result"
    );
}

// ---------------------------------------------------------------------------
// Interactive UX
// ---------------------------------------------------------------------------

#[test]
fn repl_shows_subagent_work_with_a_child_specific_prefix() {
    let mut session = build_session();

    let view = session.process_streaming_events(vec![
        StreamEvent::Token("delegated output".into()),
        StreamEvent::Done,
    ]);

    assert!(
        contains_text(&view.visible_output, "[agent:researcher/")
            && !contains_text(&view.visible_output, "[tool]"),
        "child-visible output should use a stable child prefix distinct from tool blocks"
    );
}

#[test]
fn spinner_status_text_indicates_delegation_while_a_child_is_running() {
    let session = build_session();

    assert!(
        session.status_line().contains("delegating to researcher"),
        "interactive status text should indicate delegation while a child runs"
    );
}

#[test]
fn child_failures_and_cancellations_are_shown_to_the_user_before_the_parent_turn_resumes() {
    let mut session = build_session();
    let cancelled = session.cancel_tool_execution();
    let failed = session.process_streaming_events(vec![
        StreamEvent::ToolCall(ToolCallMessage {
            id: "call-1".into(),
            name: "spawn_agent".into(),
            arguments: serde_json::json!({"agent_type":"researcher"}),
        }),
        StreamEvent::Done,
    ]);

    assert!(
        contains_text(&cancelled.visible_output, "[agent:researcher/")
            && contains_text(&failed.visible_output, "[agent:researcher/")
            && contains_text(&failed.visible_output, "error"),
        "child failures and cancellations should be rendered with the child prefix before control returns to the parent"
    );
}

// ---------------------------------------------------------------------------
// Capability attenuation and config
// ---------------------------------------------------------------------------

#[test]
fn can_spawn_is_reflected_into_the_effective_capability_token_spawn_types() {
    let config = TempConfig::write(&interactive_config_toml());
    let boot = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: Some("interactive parent task".into()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
        output_format: simulacra_cli::OutputFormat::Text,
    })
    .expect("bootstrap should succeed");

    assert_eq!(boot.capability_token.spawn_types, vec!["researcher"]);
}

// ---------------------------------------------------------------------------
// Spawn auto-approval (S018 assertion: spawn_agent is auto-approved)
// ---------------------------------------------------------------------------

#[test]
fn spawn_agent_tool_call_is_auto_approved_without_user_confirmation() {
    let mut session = build_session();
    session.start();

    let spawn_call = ToolCallMessage {
        id: "call-spawn-1".into(),
        name: "spawn_agent".into(),
        arguments: serde_json::json!({"agent_type":"researcher","task":"do research"}),
    };
    let view = session.handle_tool_approval(vec![spawn_call], &[], true);

    // spawn_agent should be auto-approved: no approval_prompts generated,
    // and the tool should appear in executed_tools
    assert!(
        view.approval_prompts.is_empty(),
        "spawn_agent should not generate any approval prompts"
    );
    assert!(
        view.executed_tools.contains(&"spawn_agent".to_string()),
        "spawn_agent should be auto-approved and appear in executed_tools"
    );
}

#[test]
fn spawn_agent_auto_approval_generates_tool_result_message() {
    let mut session = build_session();
    session.start();

    let spawn_call = ToolCallMessage {
        id: "call-spawn-2".into(),
        name: "spawn_agent".into(),
        arguments: serde_json::json!({"agent_type":"researcher","task":"investigate"}),
    };
    let view = session.handle_tool_approval(vec![spawn_call], &[], true);

    // The auto-approved spawn should produce a tool result message
    assert_eq!(
        view.tool_results_to_model.len(),
        1,
        "spawn_agent auto-approval should produce exactly one tool result"
    );
    let result = &view.tool_results_to_model[0];
    assert_eq!(result.role, Role::Tool);
    assert_eq!(
        result.tool_call_id.as_deref(),
        Some("call-spawn-2"),
        "tool result should reference the spawn_agent tool call id"
    );
}

#[test]
fn non_spawn_tools_still_require_approval_when_mixed_with_spawn_agent() {
    let mut session = build_session();
    session.start();

    let spawn_call = ToolCallMessage {
        id: "call-spawn-3".into(),
        name: "spawn_agent".into(),
        arguments: serde_json::json!({"agent_type":"researcher","task":"research"}),
    };
    let shell_call = ToolCallMessage {
        id: "call-shell-1".into(),
        name: "shell_exec".into(),
        arguments: serde_json::json!({"command":"ls"}),
    };
    let view = session.handle_tool_approval(vec![spawn_call, shell_call], &["a"], true);

    // spawn_agent is auto-approved, shell_exec still shows an approval prompt
    assert_eq!(
        view.approval_prompts.len(),
        1,
        "only non-spawn tools should produce approval prompts"
    );
    assert!(
        view.approval_prompts[0].contains("shell_exec"),
        "the approval prompt should be for shell_exec, not spawn_agent"
    );
    assert_eq!(
        view.executed_tools.len(),
        2,
        "both tools should be executed after approval"
    );
}

// ---------------------------------------------------------------------------
// can_spawn config is reflected in session
// ---------------------------------------------------------------------------

#[test]
fn session_config_can_spawn_matches_agent_type_config() {
    let session = build_session();

    assert_eq!(
        session.config.can_spawn,
        vec!["researcher".to_string()],
        "can_spawn from the default agent type should be reflected in the session config"
    );
}

#[test]
fn empty_can_spawn_config_produces_empty_spawn_types() {
    let toml = r#"[project]
name = "simulacra-s018-no-spawn"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 5
max_tokens = 1000

[agent_types.default.capabilities]
shell = true

[task]
entry_agent = "default"
task = "no spawn task"
"#;
    let config = TempConfig::write(toml);
    let boot = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: Some("no spawn task".into()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
        output_format: simulacra_cli::OutputFormat::Text,
    })
    .expect("bootstrap should succeed");

    assert!(
        boot.capability_token.spawn_types.is_empty(),
        "omitting can_spawn should produce empty spawn_types in the capability token"
    );

    let can_spawn = boot
        .config
        .agent_types
        .get("default")
        .map(|a| a.can_spawn.clone())
        .unwrap_or_default();
    assert!(
        can_spawn.is_empty(),
        "omitting can_spawn in config should produce an empty can_spawn list"
    );
}

// ---------------------------------------------------------------------------
// Status line delegation text
// ---------------------------------------------------------------------------

#[test]
fn status_line_without_active_child_shows_budget_only() {
    let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let session: InteractiveSession<FakeProvider, TestIo> = InteractiveSession::new(
        TestIo::tty(),
        Arc::new(FakeProvider),
        storage,
        vfs,
        InteractiveSessionConfig {
            project_name: "test".into(),
            model: "test-model".into(),
            max_tokens: 1000,
            max_turns: 10,
            task: None,
            requested_session_id: None,
            tool_definitions: vec![],
            can_spawn: vec![], // no can_spawn => no active_child_type
            skill_catalog: vec![],
        },
    );

    let status = session.status_line();
    assert!(
        !status.contains("delegating"),
        "status line should not mention delegation when there is no active child"
    );
    assert!(
        status.contains("tokens:") && status.contains("turns:"),
        "status line should show budget info"
    );
}

// ---------------------------------------------------------------------------
// Streaming events with spawn_agent tool call
// ---------------------------------------------------------------------------

#[test]
fn spawn_agent_tool_call_in_stream_sets_active_child_type_for_subsequent_tokens() {
    let mut session = build_session();

    // First send a spawn_agent tool call, then a token
    let view = session.process_streaming_events(vec![
        StreamEvent::ToolCall(ToolCallMessage {
            id: "call-1".into(),
            name: "spawn_agent".into(),
            arguments: serde_json::json!({"agent_type":"researcher"}),
        }),
        StreamEvent::Token("child working...".into()),
        StreamEvent::Done,
    ]);

    // The token after spawn_agent should be prefixed with the child identity
    assert!(
        view.visible_output
            .iter()
            .any(|line| line.contains("[agent:researcher/") && line.contains("child working...")),
        "tokens after spawn_agent tool call should be prefixed with the child agent identity"
    );
}

// ---------------------------------------------------------------------------
// Cancel tool execution produces child-prefixed cancellation
// ---------------------------------------------------------------------------

#[test]
fn cancel_tool_execution_produces_error_tool_result_with_cancellation_content() {
    let mut session = build_session();

    let view = session.cancel_tool_execution();

    // Should produce a tool result for the model indicating cancellation
    assert!(
        !view.tool_results_to_model.is_empty(),
        "cancelling tool execution should produce a tool result message"
    );
    let result = &view.tool_results_to_model[0];
    assert_eq!(result.role, Role::Tool);
    assert!(
        result.content.contains("cancelled"),
        "cancellation tool result should contain 'cancelled'"
    );
}

#[test]
fn cancel_tool_execution_shows_child_prefix_when_child_is_active() {
    let mut session = build_session();

    let view = session.cancel_tool_execution();

    assert!(
        view.visible_output
            .iter()
            .any(|line| line.contains("[agent:researcher/") && line.contains("cancelled")),
        "cancellation output should use the child prefix when a child type is active"
    );
}

// ---------------------------------------------------------------------------
// CapabilityToken spawn_types attenuation
// ---------------------------------------------------------------------------

#[test]
fn capability_token_spawn_types_subset_check_rejects_wider_child() {
    use simulacra_types::CapabilityToken;

    let parent = CapabilityToken {
        spawn_types: vec!["researcher".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        spawn_types: vec!["researcher".into(), "reviewer".into()],
        ..Default::default()
    };

    assert!(
        !child.is_subset_of(&parent),
        "a child with more spawn_types than the parent must be rejected"
    );
}

#[test]
fn capability_token_spawn_types_subset_check_accepts_narrower_child() {
    use simulacra_types::CapabilityToken;

    let parent = CapabilityToken {
        spawn_types: vec!["researcher".into(), "reviewer".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        spawn_types: vec!["researcher".into()],
        ..Default::default()
    };

    assert!(
        child.is_subset_of(&parent),
        "a child with fewer spawn_types than the parent should be accepted"
    );
}

#[test]
fn capability_token_empty_spawn_types_is_subset_of_any_parent() {
    use simulacra_types::CapabilityToken;

    let parent = CapabilityToken {
        spawn_types: vec!["researcher".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        spawn_types: vec![],
        ..Default::default()
    };

    assert!(
        child.is_subset_of(&parent),
        "a child with empty spawn_types should be a subset of any parent"
    );
}
