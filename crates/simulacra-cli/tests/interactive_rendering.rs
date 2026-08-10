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
use simulacra_runtime::{
    InMemorySessionStorage, MessagePriority, SessionStorage, SupervisorMessage, SupervisorPayload,
};
use simulacra_types::{
    ActivityEvent, AgentId, FinishReason, Message, Provider, ProviderError, ProviderResponse, Role,
    TokenUsage, ToolCallMessage, ToolDefinition, VirtualFs,
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
allowed_child_placements = ["researcher"]

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]

[child_placements.researcher]
backend = "native"
model = "gpt-5.4"
max_turns = 3
max_tokens = 222

[child_placements.researcher.capabilities]
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

    let allowed_child_placements = boot
        .config
        .agent_types
        .get("default")
        .map(|a| a.allowed_child_placements.clone())
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
            allowed_child_placements,
            skill_catalog: vec![],
        },
    )
}

fn contains_text(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

fn accepted_child(child_id: &str) -> ActivityEvent {
    ActivityEvent::ChildSpawned {
        child_id: child_id.into(),
        placement: "researcher".into(),
        task: "bounded delegated work".into(),
    }
}

fn finished_child(child_id: &str, exit_reason: &str) -> ActivityEvent {
    ActivityEvent::ChildFinished {
        child_id: child_id.into(),
        placement: "researcher".into(),
        exit_reason: exit_reason.into(),
        duration_ms: 1,
        tool_uses: 0,
        token_count: 1,
    }
}

// ---------------------------------------------------------------------------
// Tool definition and result shape
// ---------------------------------------------------------------------------

#[test]
fn interactive_sessions_register_spawn_agent_and_list_it_in_tools_output() {
    let mut session = build_session();

    let view = session.dispatch_command("/tools");

    let stock_spawn_description = "I can start a supervised child for one concrete, bounded, independent task. Choose where I run it with placement and shape how it works with instructions; placement supplies an environment and capabilities, not a role. I return a live handle, not the child's final answer.";
    assert!(
        contains_text(&view.visible_output, stock_spawn_description),
        "CLI bootstrap must register spawn_agent with the complete stock description"
    );

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
        "concrete, bounded, independent task",
        "placement supplies an environment and capabilities, not a role",
        "return a live handle, not the child's final answer",
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
        content: r#"{"child_id":"child-1","placement":"researcher","message":"summary"}"#.into(),
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
fn repl_shows_subagent_work_with_a_generic_child_prefix() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-repl"));

    let view = session.process_streaming_events(vec![
        StreamEvent::Token("delegated output".into()),
        StreamEvent::Done,
    ]);

    assert!(
        contains_text(&view.visible_output, "[agent:Child]")
            && !contains_text(&view.visible_output, "[tool]"),
        "child-visible output should use a generic child prefix distinct from tool blocks"
    );
    assert!(!contains_text(&view.visible_output, "researcher"));
    session.process_activity_event(&finished_child("child-repl", "completed"));
    assert!(!session.status_line().contains("delegating"));
}

#[test]
fn spinner_status_text_indicates_delegation_while_a_child_is_running() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-spinner"));

    assert!(
        session.status_line().contains("delegating to Child")
            && !session.status_line().contains("researcher"),
        "interactive status text should indicate generic child delegation without treating placement as identity"
    );
}

#[test]
fn interactive_session_with_authorized_placements_starts_without_an_active_child() {
    let session = build_session();

    assert!(
        !session.status_line().contains("delegating"),
        "placement authorization alone must not fabricate an active child"
    );
}

#[test]
fn child_failure_output_uses_the_child_prefix_until_its_terminal_lifecycle_event() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-failed"));
    let failed = session.process_streaming_events(vec![
        StreamEvent::Token("error: child failed".into()),
        StreamEvent::Done,
    ]);

    assert!(
        failed.visible_output.iter().any(|frame| {
            frame.starts_with("[agent:Child]") && frame.contains("error: child failed")
        }),
        "child output should retain the generic child prefix while the child is live"
    );
    session.process_activity_event(&finished_child("child-failed", "failed"));
    assert!(!session.status_line().contains("delegating"));
}

#[test]
fn interactive_session_tracks_all_concurrent_children_and_removes_only_exact_terminal_ids() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-a"));
    session.process_activity_event(&accepted_child("child-b"));
    assert!(session.status_line().contains("delegating"));

    session.process_activity_event(&finished_child("child-b", "completed"));
    assert!(
        session.status_line().contains("delegating"),
        "child-a remains live when child-b terminates"
    );
    session.process_activity_event(&finished_child("not-an-accepted-child", "failed"));
    assert!(
        session.status_line().contains("delegating"),
        "a nonmatching terminal id has no effect"
    );
    session.process_activity_event(&finished_child("child-a", "cancelled"));
    assert!(!session.status_line().contains("delegating"));
}

#[test]
fn every_child_finished_outcome_removes_its_exact_accepted_id() {
    for (index, exit_reason) in [
        "completed",
        "max_turns",
        "budget_exhausted",
        "failed",
        "guardrail_tripped",
        "policy_kill",
        "cancelled",
    ]
    .into_iter()
    .enumerate()
    {
        let child_id = format!("child-terminal-{index}");
        let mut session = build_session();
        session.process_activity_event(&accepted_child(&child_id));
        session.process_activity_event(&finished_child(&child_id, exit_reason));
        assert!(
            !session.status_line().contains("delegating"),
            "terminal outcome {exit_reason} must remove exactly {child_id}"
        );
    }
}

#[tokio::test]
async fn cancellation_uses_the_explicit_join_child_signal_and_waits_for_ok_acknowledgement() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-a"));
    session.process_activity_event(&accepted_child("child-b"));
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(1);
    session.set_supervisor_control(control_tx, AgentId("interactive-root".into()));
    session.process_activity_event(&ActivityEvent::ToolStart {
        tool_call_id: "join-selected-a".into(),
        name: "join_child_agent".into(),
        arguments: serde_json::json!({"child_id": "child-a"}),
    });

    let acknowledgement = async {
        let control = control_rx
            .recv()
            .await
            .expect("selected join cancellation should reach the supervisor");
        assert_eq!(control.agent_id.0, "interactive-root");
        assert_eq!(control.priority, MessagePriority::Signal);
        match control.payload {
            SupervisorPayload::CancelChild(child_id, acknowledgement) => {
                assert_eq!(
                    child_id.0, "child-a",
                    "only the join-selected child is cancellable"
                );
                acknowledgement
                    .send(Ok(()))
                    .expect("interactive cancellation task should await the acknowledgement");
            }
            payload => panic!("expected signal-priority CancelChild, got {payload:?}"),
        }
    };
    let (requested, ()) = tokio::join!(session.cancel_selected_child(), acknowledgement);

    assert!(
        contains_text(
            &requested.visible_output,
            "cancellation requested (child-a)"
        ),
        "only an Ok acknowledgement may render cancellation-requested output"
    );
    assert!(
        requested.tool_results_to_model.iter().any(|message| {
            message.role == Role::Tool
                && message.content.contains("child-a")
                && message.content.contains("cancellation_requested")
        }),
        "an Ok acknowledgement should be the sole basis for the model-facing request result"
    );

    session.process_activity_event(&finished_child("child-b", "completed"));
    assert!(
        session.status_line().contains("delegating"),
        "the selected child remains live until its own matching ChildFinished event"
    );
    session.process_activity_event(&finished_child("child-a", "cancelled"));
    assert!(
        !session.status_line().contains("delegating"),
        "the matching terminal ChildFinished event, not the acknowledgement, ends the child lifecycle"
    );
}

#[tokio::test]
async fn cancellation_without_an_explicit_join_sends_no_control_or_output() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-a"));
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<SupervisorMessage>(1);
    session.set_supervisor_control(control_tx, AgentId("interactive-root".into()));
    let before = session.snapshot();

    let after = session.cancel_selected_child().await;

    assert!(
        control_rx.try_recv().is_err(),
        "activity order must not select a child or send CancelChild without a join"
    );
    assert_eq!(after.visible_output, before.visible_output);
    assert_eq!(
        after.tool_results_to_model.len(),
        before.tool_results_to_model.len()
    );
}

#[tokio::test]
async fn cancellation_error_acknowledgement_reports_an_error_without_claiming_success() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-a"));
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(1);
    session.set_supervisor_control(control_tx, AgentId("interactive-root".into()));
    session.process_activity_event(&ActivityEvent::ToolStart {
        tool_call_id: "join-selected-a".into(),
        name: "join_child_agent".into(),
        arguments: serde_json::json!({"child_id": "child-a"}),
    });

    let acknowledgement = async {
        let control = control_rx
            .recv()
            .await
            .expect("selected join cancellation should reach the supervisor");
        match control.payload {
            SupervisorPayload::CancelChild(child_id, acknowledgement) => {
                assert_eq!(child_id.0, "child-a");
                acknowledgement
                    .send(Err("child is already terminal".into()))
                    .expect("interactive cancellation task should await the acknowledgement");
            }
            payload => panic!("expected CancelChild, got {payload:?}"),
        }
    };
    let (view, ()) = tokio::join!(session.cancel_selected_child(), acknowledgement);
    assert!(
        view.error
            .as_deref()
            .is_some_and(|error| error.contains("child is already terminal")),
        "an Err acknowledgement must be surfaced as an interactive error"
    );
    assert!(
        !contains_text(&view.visible_output, "cancellation requested"),
        "an Err acknowledgement must not render a successful cancellation request"
    );
    assert!(
        view.tool_results_to_model
            .iter()
            .all(|message| !message.content.contains("cancellation_requested")),
        "an Err acknowledgement must not fabricate a model-facing success"
    );
    session.process_activity_event(&finished_child("child-a", "failed"));
    assert!(
        !session.status_line().contains("delegating"),
        "a child remains live after cancellation acknowledgement failure until its matching terminal event"
    );
}

// ---------------------------------------------------------------------------
// Capability attenuation and config
// ---------------------------------------------------------------------------

#[test]
fn allowed_child_placements_are_reflected_into_the_effective_capability_token() {
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

    assert_eq!(boot.capability_token.spawn_placements, vec!["researcher"]);
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
        arguments: serde_json::json!({"placement":"researcher","task":"do research"}),
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
        arguments: serde_json::json!({"placement":"researcher","task":"investigate"}),
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
        arguments: serde_json::json!({"placement":"researcher","task":"research"}),
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
// Child placement config is reflected in the session.
// ---------------------------------------------------------------------------

#[test]
fn session_allowed_child_placements_match_agent_type_config() {
    let session = build_session();

    assert_eq!(
        session.config.allowed_child_placements,
        vec!["researcher".to_string()],
        "allowed placements from the default agent type should be reflected in the session config"
    );
}

#[test]
fn omitted_allowed_child_placements_produce_empty_spawn_authorization() {
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
        boot.capability_token.spawn_placements.is_empty(),
        "omitting allowed child placements should produce empty spawn authorization"
    );

    let allowed_child_placements = boot
        .config
        .agent_types
        .get("default")
        .map(|a| a.allowed_child_placements.clone())
        .unwrap_or_default();
    assert!(
        allowed_child_placements.is_empty(),
        "omitting allowed child placements should produce an empty authorization list"
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
            allowed_child_placements: vec![],
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
fn spawn_agent_tool_call_in_stream_sets_active_child_for_subsequent_tokens() {
    let mut session = build_session();
    session.process_activity_event(&accepted_child("child-stream"));

    // The accepted lifecycle establishes identity; the tool call is display only.
    let view = session.process_streaming_events(vec![
        StreamEvent::ToolCall(ToolCallMessage {
            id: "call-1".into(),
            name: "spawn_agent".into(),
            arguments: serde_json::json!({"placement":"researcher"}),
        }),
        StreamEvent::Token("child working...".into()),
        StreamEvent::Done,
    ]);

    // The token after spawn_agent should be prefixed with the child identity
    assert!(
        view.visible_output
            .iter()
            .any(|line| line.contains("[agent:Child]") && line.contains("child working...")),
        "tokens after spawn_agent tool call should use the generic child prefix"
    );
    session.process_activity_event(&finished_child("child-stream", "completed"));
    assert!(!session.status_line().contains("delegating"));
}

#[test]
fn spawn_agent_attempt_alone_does_not_attribute_following_parent_tokens_to_a_child() {
    let mut session = build_session();

    let view = session.process_streaming_events(vec![
        StreamEvent::ToolCall(ToolCallMessage {
            id: "rejected-spawn".into(),
            name: "spawn_agent".into(),
            arguments: serde_json::json!({"placement":"researcher"}),
        }),
        StreamEvent::Token("parent continues after rejected spawn".into()),
        StreamEvent::Done,
    ]);

    assert!(contains_text(
        &view.visible_output,
        "parent continues after rejected spawn"
    ));
    assert!(
        !view.visible_output.iter().any(|line| {
            line.contains("[agent:Child]") && line.contains("parent continues after rejected spawn")
        }),
        "a tool-call attempt is not an accepted ChildSpawned lifecycle event"
    );
}

// ---------------------------------------------------------------------------
// CapabilityToken placement attenuation
// ---------------------------------------------------------------------------

#[test]
fn capability_token_spawn_placements_subset_check_rejects_wider_child() {
    use simulacra_types::CapabilityToken;

    let parent = CapabilityToken {
        spawn_placements: vec!["researcher".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        spawn_placements: vec!["researcher".into(), "reviewer".into()],
        ..Default::default()
    };

    assert!(
        !child.is_subset_of(&parent),
        "a child with more spawn placements than the parent must be rejected"
    );
}

#[test]
fn capability_token_spawn_placements_subset_check_accepts_narrower_child() {
    use simulacra_types::CapabilityToken;

    let parent = CapabilityToken {
        spawn_placements: vec!["researcher".into(), "reviewer".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        spawn_placements: vec!["researcher".into()],
        ..Default::default()
    };

    assert!(
        child.is_subset_of(&parent),
        "a child with fewer spawn placements than the parent should be accepted"
    );
}

#[test]
fn capability_token_empty_spawn_placements_is_subset_of_any_parent() {
    use simulacra_types::CapabilityToken;

    let parent = CapabilityToken {
        spawn_placements: vec!["researcher".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        spawn_placements: vec![],
        ..Default::default()
    };

    assert!(
        child.is_subset_of(&parent),
        "a child with empty spawn placements should be a subset of any parent"
    );
}
