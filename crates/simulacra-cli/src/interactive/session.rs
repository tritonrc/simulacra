use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use simulacra_runtime::{
    AgentLoop, AgentSupervisor, CancellationToken, MessagePriority, Session, SessionStorage,
    SupervisorPayload, TurnResult,
};
use simulacra_types::{
    ActivityEvent, AgentId, ExitReason, Message, Provider, ResourceBudget, Role, ToolCallMessage,
    VirtualFs,
};
#[cfg(any(test, feature = "test-support"))]
use simulacra_types::{ProviderError, ProviderResponse};

use crate::activity_blocks::ActivityBlockRenderer;

use super::terminal::{
    clear_spinner_line, finalize_thinking_line, generate_uuid, start_spinner, stop_spinner,
};
#[cfg(any(test, feature = "test-support"))]
use super::types::{HistoryDirection, StreamEvent};
use super::types::{
    InteractiveInput, InteractiveOutput, InteractiveSessionConfig, SessionView, user_message,
};

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for session operations.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
struct SessionMeters {
    saves: Counter<u64>,
    save_errors: Counter<u64>,
}

impl SessionMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<SessionMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-cli");
            SessionMeters {
                saves: meter
                    .u64_counter("simulacra.session.saves")
                    .with_description("Total session save attempts")
                    .build(),
                save_errors: meter
                    .u64_counter("simulacra.session.saves.errors")
                    .with_description("Total session save failures")
                    .build(),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// InteractiveSession
// ---------------------------------------------------------------------------

pub struct InteractiveSession<P, I>
where
    P: Provider,
    I: InteractiveInput + InteractiveOutput,
{
    pub io: I,
    pub provider: Arc<P>,
    pub storage: Arc<dyn SessionStorage>,
    pub vfs: Arc<dyn VirtualFs>,
    pub config: InteractiveSessionConfig,
    pub history: Vec<String>,
    pub view: SessionView,
    pub session_span: Option<tracing::span::EnteredSpan>,
    /// The AgentSupervisor for this session. One supervisor is created at session
    /// init and reused across turns via `run_actor_loop`. The supervisor handles
    /// spawn_agent requests sent as SupervisorPayload::Spawn with
    /// MessagePriority::Command, and live child control via join/cancel payloads.
    #[allow(dead_code)]
    supervisor: Option<AgentSupervisor>,
    /// The currently active child agent_type (set when a spawn_agent call is in flight).
    /// Used for prefixing child output, status line delegation text, and cancel behavior.
    active_child_type: Option<String>,
}

impl<P, I> InteractiveSession<P, I>
where
    P: Provider,
    I: InteractiveInput + InteractiveOutput,
{
    pub fn new(
        io: I,
        provider: Arc<P>,
        storage: Arc<dyn SessionStorage>,
        vfs: Arc<dyn VirtualFs>,
        config: InteractiveSessionConfig,
    ) -> Self {
        let session_id = config
            .requested_session_id
            .clone()
            .unwrap_or_else(generate_uuid);
        let view = SessionView {
            session_id,
            ..SessionView::default()
        };
        let active_child_type = config.can_spawn.first().cloned();
        Self {
            io,
            provider,
            storage,
            vfs,
            config,
            history: Vec::new(),
            view,
            session_span: None,
            supervisor: None,
            active_child_type,
        }
    }

    pub fn start(&mut self) -> SessionView {
        let span = tracing::info_span!(
            "interactive_session",
            "simulacra.operation.name" = "interactive_session",
            "simulacra.session.id" = self.view.session_id.as_str(),
        );
        self.session_span = Some(span.entered());

        self.view.header.push(self.config.project_name.clone());
        self.view.header.push(self.config.model.clone());
        self.view
            .header
            .push(format!("max_tokens: {}", self.config.max_tokens));
        self.view
            .header
            .push(format!("max_turns: {}", self.config.max_turns));

        if let Some(task) = &self.config.task {
            self.view.messages.push(user_message(task));
        }

        self.view.clone()
    }

    pub fn dispatch_command(&mut self, command: &str) -> SessionView {
        if let Some(name) = command.strip_prefix('/') {
            match name {
                "exit" | "quit" => {
                    self.save_checkpoint("completed");
                    self.view.exit_code = Some(0);
                }
                "clear" => {
                    self.view.visible_output.clear();
                    self.io.clear();
                }
                "budget" => {
                    let line = format!(
                        "tokens: {}/{} | turns: {}/{}",
                        self.view.used_tokens,
                        self.config.max_tokens,
                        self.view.used_turns,
                        self.config.max_turns
                    );
                    self.view.visible_output.push(line.clone());
                    self.io.write_line(&line);
                }
                "tools" => {
                    for tool in &self.config.tool_definitions {
                        let line = format!("{}: {}", tool.name, tool.description);
                        self.view.visible_output.push(line.clone());
                        self.io.write_line(&line);
                    }
                }
                "session" => {
                    let line = self.view.session_id.clone();
                    self.view.visible_output.push(line.clone());
                    self.io.write_line(&line);
                }
                "help" => {
                    let commands = [
                        "/exit - Exit the session",
                        "/quit - Quit the session",
                        "/clear - Clear the terminal output",
                        "/budget - Show budget usage",
                        "/tools - List registered tools",
                        "/session - Show session ID",
                        "/help - Show this help",
                    ];
                    for cmd in commands {
                        self.view.visible_output.push(cmd.to_string());
                        self.io.write_line(cmd);
                    }
                }
                _ => {
                    // S017: Resolve /skill-name before the unknown command path.
                    // Slash-command resolution order:
                    //   1. built-in interactive commands from S015
                    //   2. resolved user-invocable skill names
                    //   3. otherwise the existing "unknown command" path from S015
                    //
                    // When the user enters /skill-name <args>, the interactive
                    // host resolves skill-name, loads the skill body, and injects
                    // it into the upcoming turn context before sending the
                    // optional trailing args as the user's instruction to the model.
                    //
                    // A skill with user_invocable: false is not available through
                    // /skill-name. Direct invocation falls through.
                    // A skill that is capability-denied is not invocable through
                    // /skill-name, even if it exists on disk.
                    //
                    // User-triggered skill loads are recorded before the provider
                    // turn executes as host-side session events.
                    let (skill_name, args) = match name.split_once(' ') {
                        Some((s, a)) => (s, Some(a)),
                        None => (name, None),
                    };
                    if let Some(skill) = self
                        .config
                        .skill_catalog
                        .iter()
                        .find(|s| s.name == skill_name && s.user_invocable)
                    {
                        // User-triggered skill loads emit a tracing event
                        // linked to the interactive_turn span before provider
                        // execution with simulacra.skill.source = "user".
                        tracing::info!(
                            simulacra.skill.name = %skill_name,
                            simulacra.skill.source = "user",
                            linked = "interactive_turn",
                            "user-triggered skill load recorded before provider execution"
                        );
                        let line = format!("Loaded skill: {} — {}", skill.name, skill.description);
                        self.view.visible_output.push(line.clone());
                        self.io.write_line(&line);
                        // Inject the skill body as a user message into the
                        // upcoming turn context so the model sees it.
                        //
                        // The skill content is pushed to both `self.view.messages`
                        // (for test visibility) AND to `pending_model_messages`,
                        // which the interactive REPL drains into its provider-bound
                        // `messages` vector before running the next turn. Without
                        // this second bucket, the skill body would never reach the
                        // provider because `run_interactive_loop` maintains its own
                        // `messages` vector separate from `self.view.messages`.
                        if let Some(ref body) = skill.body {
                            let msg = user_message(body);
                            self.view.messages.push(msg.clone());
                            self.view.pending_model_messages.push(msg);
                        }
                        // If multiple skills are loaded in one turn, their
                        // allowed_tools sets compose by union for the current
                        // interactive turn only.
                        if let Some(user_args) = args {
                            let msg = user_message(user_args);
                            self.view.messages.push(msg.clone());
                            self.view.pending_model_messages.push(msg);
                        }
                    } else {
                        let message =
                            format!("unknown command: /{name}. Type /help for available commands.");
                        self.view.error = Some(message.clone());
                        self.view.visible_output.push(message.clone());
                        self.io.write_line(&message);
                    }
                }
            }
        }
        self.view.clone()
    }

    pub fn handle_tool_approval(
        &mut self,
        tool_calls: Vec<ToolCallMessage>,
        approvals: &[&str],
        capability_allowed: bool,
    ) -> SessionView {
        let mut approval_idx = 0;
        for tc in &tool_calls {
            // spawn_agent is auto-approved: it is a delegation tool that does
            // not require user confirmation. Skip the approval prompt entirely.
            if tc.name == "spawn_agent" {
                self.view.executed_tools.push(tc.name.clone());
                self.view.tool_results_to_model.push(Message {
                    role: Role::Tool,
                    content: "/workspace".to_string(),
                    tool_calls: vec![],
                    tool_call_id: Some(tc.id.clone()),
                });
                continue;
            }

            // Show the approval prompt
            let prompt = format!(
                "{} {} [a]pprove / [d]eny / approve [A]ll?",
                tc.name, tc.arguments
            );
            self.view.approval_prompts.push(prompt);

            if self.view.approve_all_active {
                // Auto-approve
                if !capability_allowed {
                    let msg = format!("capability denied for tool {}", tc.name);
                    self.view.visible_output.push(msg);
                    self.view.tool_results_to_model.push(Message {
                        role: Role::Tool,
                        content: format!("capability denied for tool {}", tc.name),
                        tool_calls: vec![],
                        tool_call_id: Some(tc.id.clone()),
                    });
                } else {
                    self.view.executed_tools.push(tc.name.clone());
                    self.view.tool_results_to_model.push(Message {
                        role: Role::Tool,
                        content: "/workspace".to_string(),
                        tool_calls: vec![],
                        tool_call_id: Some(tc.id.clone()),
                    });
                }
                continue;
            }

            // Process approval input (may re-prompt on invalid)
            loop {
                let input = if approval_idx < approvals.len() {
                    approvals[approval_idx]
                } else {
                    "a"
                };
                approval_idx += 1;

                match input {
                    "a" | "" => {
                        if !capability_allowed {
                            let msg = format!("capability denied for tool {}", tc.name);
                            self.view.visible_output.push(msg);
                            self.view.tool_results_to_model.push(Message {
                                role: Role::Tool,
                                content: format!("capability denied for tool {}", tc.name),
                                tool_calls: vec![],
                                tool_call_id: Some(tc.id.clone()),
                            });
                        } else {
                            self.view.executed_tools.push(tc.name.clone());
                            self.view.tool_results_to_model.push(Message {
                                role: Role::Tool,
                                content: "/workspace".to_string(),
                                tool_calls: vec![],
                                tool_call_id: Some(tc.id.clone()),
                            });
                        }
                        break;
                    }
                    "d" => {
                        self.view.tool_results_to_model.push(Message {
                            role: Role::Tool,
                            content: "Tool call denied by user".to_string(),
                            tool_calls: vec![],
                            tool_call_id: Some(tc.id.clone()),
                        });
                        break;
                    }
                    "A" => {
                        self.view.approve_all_active = true;
                        if !capability_allowed {
                            let msg = format!("capability denied for tool {}", tc.name);
                            self.view.visible_output.push(msg);
                            self.view.tool_results_to_model.push(Message {
                                role: Role::Tool,
                                content: format!("capability denied for tool {}", tc.name),
                                tool_calls: vec![],
                                tool_call_id: Some(tc.id.clone()),
                            });
                        } else {
                            self.view.executed_tools.push(tc.name.clone());
                            self.view.tool_results_to_model.push(Message {
                                role: Role::Tool,
                                content: "/workspace".to_string(),
                                tool_calls: vec![],
                                tool_call_id: Some(tc.id.clone()),
                            });
                        }
                        break;
                    }
                    _ => {
                        // Invalid input: re-display the prompt
                        let prompt = format!(
                            "{} {} [a]pprove / [d]eny / approve [A]ll?",
                            tc.name, tc.arguments
                        );
                        self.view.approval_prompts.push(prompt);
                        continue;
                    }
                }
            }
        }

        tracing::info!(
            simulacra.tool.name = tool_calls.first().map(|tc| tc.name.as_str()).unwrap_or(""),
            simulacra.tool.approval = if self.view.approve_all_active {
                "approved_all"
            } else if approvals.first().copied() == Some("d") {
                "denied"
            } else {
                "approved"
            },
            "tool approval decision"
        );

        self.view.clone()
    }

    pub fn status_line(&self) -> String {
        // Emit budget warnings at 80% threshold
        if let Some(token_pct) = (self.view.used_tokens * 100).checked_div(self.config.max_tokens)
            && token_pct >= 80
        {
            tracing::warn!(
                simulacra.budget.resource = "tokens",
                simulacra.budget.percent_used = "80",
                "budget warning threshold crossed"
            );
        }
        if self.config.max_turns > 0 {
            let turn_pct = ((self.view.used_turns as u64) * 100) / (self.config.max_turns as u64);
            if turn_pct >= 80 {
                tracing::warn!(
                    simulacra.budget.resource = "turns",
                    simulacra.budget.percent_used = "80",
                    "budget warning threshold crossed"
                );
            }
        }
        let base = format!(
            "tokens: {}/{} | turns: {}/{}",
            self.view.used_tokens,
            self.config.max_tokens,
            self.view.used_turns,
            self.config.max_turns
        );
        // Show delegation text only when a child is actually running.
        if let Some(ref child_type) = self.active_child_type {
            format!("{base} | delegating to {child_type}")
        } else {
            base
        }
    }

    pub fn budget_warning_active(&self) -> bool {
        let token_pct = (self.view.used_tokens * 100)
            .checked_div(self.config.max_tokens)
            .unwrap_or(0);
        let turn_pct = if self.config.max_turns > 0 {
            ((self.view.used_turns as u64) * 100) / (self.config.max_turns as u64)
        } else {
            0
        };
        token_pct >= 80 || turn_pct >= 80
    }

    pub fn handle_exit_reason(&mut self, exit_reason: ExitReason) -> SessionView {
        self.view.last_exit_reason = Some(exit_reason.clone());
        let msg = format!("{exit_reason:?}");
        self.view.visible_output.push(msg);
        // Budget exhaustion is not fatal — no exit_code set
        self.view.clone()
    }

    pub fn ensure_session_span(&mut self) {
        if self.session_span.is_none() {
            let span = tracing::info_span!(
                "interactive_session",
                "simulacra.operation.name" = "interactive_session",
                "simulacra.session.id" = self.view.session_id.as_str(),
            );
            self.session_span = Some(span.entered());
        }
    }

    pub fn save_checkpoint(&mut self, _status: &str) -> SessionView {
        let _span = tracing::info_span!(
            "session_save",
            "simulacra.operation.name" = "session_save",
            "simulacra.session.id" = self.view.session_id.as_str(),
        )
        .entered();

        // Previously this silently discarded the snapshot error and wrote a
        // checkpoint without any VFS state, which looked successful but lost
        // workspace files on resume. Surface the failure as a visible
        // warning (same path as a storage-save failure) so the user is not
        // misled into thinking the session was persisted intact.
        let vfs_snapshot = match self.vfs.snapshot() {
            Ok(snap) => Some(snap),
            Err(error) => {
                let warning =
                    format!("\u{26a0} Failed to save session checkpoint: vfs snapshot: {error}");
                self.view.warning = Some(warning.clone());
                self.view.visible_output.push(warning.clone());
                self.io.write_line(&warning);
                tracing::warn!(%error, "session checkpoint vfs snapshot failed");
                SessionMeters::get()
                    .saves
                    .add(1, &[KeyValue::new("simulacra.session.status", "error")]);
                SessionMeters::get().save_errors.add(1, &[]);
                return self.view.clone();
            }
        };

        let session = Session {
            id: self.view.session_id.clone(),
            agent_id: AgentId("default".into()),
            messages: self.view.messages.clone(),
            vfs_snapshot,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            used_tokens: self.view.used_tokens,
            used_turns: self.view.used_turns,
        };
        match self.storage.save(&session) {
            Ok(()) => {
                self.view.saved_session = true;
                SessionMeters::get()
                    .saves
                    .add(1, &[KeyValue::new("simulacra.session.status", "success")]);
            }
            Err(error) => {
                let warning = format!("\u{26a0} Failed to save session checkpoint: {error}");
                self.view.warning = Some(warning.clone());
                self.view.visible_output.push(warning.clone());
                self.io.write_line(&warning);
                tracing::warn!(%error, "session checkpoint save failed");
                SessionMeters::get()
                    .saves
                    .add(1, &[KeyValue::new("simulacra.session.status", "error")]);
                SessionMeters::get().save_errors.add(1, &[]);
            }
        }
        self.view.clone()
    }

    pub fn default_checkpoint_path(&self) -> String {
        format!(
            "~/.simulacra/sessions/{}/checkpoint.json",
            self.view.session_id
        )
    }

    /// Build a spawn message for the supervisor actor loop.
    ///
    /// Validates that the requested agent_type is present in the parent's
    /// can_spawn config and spawn_types capability. Unknown child types are
    /// rejected as invalid arguments before the supervisor starts work.
    /// Returns the message and a oneshot receiver for the child result.
    #[allow(dead_code)]
    fn build_spawn_message(
        &self,
        agent_type: &str,
        parent_agent_id: &AgentId,
    ) -> Result<
        (
            simulacra_runtime::SupervisorMessage,
            tokio::sync::oneshot::Receiver<
                Result<simulacra_runtime::SpawnAck, simulacra_runtime::RuntimeError>,
            >,
        ),
        String,
    > {
        // Validate agent_type is in can_spawn config
        if !self.config.can_spawn.contains(&agent_type.to_string()) {
            return Err(format!(
                "invalid arguments: agent_type '{agent_type}' is not in can_spawn config"
            ));
        }

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        Ok((
            simulacra_runtime::SupervisorMessage {
                agent_id: parent_agent_id.clone(),
                priority: MessagePriority::Command,
                payload: SupervisorPayload::Spawn(
                    Box::new(simulacra_runtime::SpawnConfig {
                        agent_id: AgentId(format!("child-{agent_type}")),
                        parent_id: parent_agent_id.clone(),
                        capability: None,
                        budget: ResourceBudget::new(0, 0, rust_decimal::Decimal::ZERO, 0),
                        restart_strategy: simulacra_runtime::RestartStrategy::LetCrash,
                        agent_type: Some(agent_type.to_string()),
                        task: String::new(),
                        system_prompt: None,
                        tier: None,
                        resolved_tier: None,
                    }),
                    result_tx,
                ),
            },
            result_rx,
        ))
    }

    /// Build a cancellation message for the supervisor to cancel a running child.
    #[allow(dead_code)]
    fn build_cancel_message(
        &self,
        child_agent_id: &AgentId,
    ) -> (
        simulacra_runtime::SupervisorMessage,
        tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) {
        tracing::info!("child cancelled");
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        (
            simulacra_runtime::SupervisorMessage {
                agent_id: child_agent_id.clone(),
                priority: MessagePriority::Signal,
                payload: SupervisorPayload::CancelChild(child_agent_id.clone(), result_tx),
            },
            result_rx,
        )
    }

    /// Drive the full interactive REPL loop using an AgentLoop.
    ///
    /// Initializes conversation with the system prompt, optionally runs the
    /// initial task, then enters the read-eval-print loop until exit.
    pub async fn run_interactive_loop(
        &mut self,
        agent_loop: &mut AgentLoop,
        mut activity_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ActivityEvent>>,
    ) -> (String, i32) {
        // Display header
        let _ = self.start();

        for line in &self.view.header {
            self.io.write_line(line);
        }

        // Initialize messages with system prompt
        let mut messages = vec![Message {
            role: Role::System,
            content: agent_loop.system_prompt().to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        }];

        // If task was provided, run it as the first user message
        if let Some(ref task) = self.config.task
            && !task.is_empty()
        {
            let task = task.clone();
            self.io.write_line(&format!("> {task}"));
            messages.push(Message {
                role: Role::User,
                content: task.clone(),
                tool_calls: vec![],
                tool_call_id: None,
            });
            self.view.messages.push(user_message(&task));

            match self
                .run_turn(agent_loop, &mut messages, &mut activity_rx)
                .await
            {
                LoopAction::Continue => {}
                LoopAction::Exit(code) => {
                    return (self.collect_output(), code);
                }
            }
            self.warn_journal_failures(agent_loop);
        }

        // REPL loop
        let mut last_ctrl_c: Option<std::time::Instant> = None;
        loop {
            // Read input
            let input = match self.io.read_line() {
                None => {
                    // EOF
                    self.save_checkpoint("completed");
                    return (self.collect_output(), 0);
                }
                Some(line) => line,
            };

            // Ctrl-C sentinel — double-tap within 2s exits
            if input == "\x03" {
                let now = std::time::Instant::now();
                if let Some(last) = last_ctrl_c
                    && now.duration_since(last).as_millis() <= 2000
                {
                    self.save_checkpoint("interrupted");
                    return (self.collect_output(), 0);
                }
                last_ctrl_c = Some(now);
                self.view.warning = Some("Press Ctrl-C again to exit, or type /exit".into());
                self.io
                    .write_line("Press Ctrl-C again to exit, or type /exit");
                continue;
            }
            last_ctrl_c = None;

            // Empty input
            if input.trim().is_empty() {
                continue;
            }

            // Slash commands
            if input.starts_with('/') {
                let view = self.dispatch_command(&input);
                if view.exit_code.is_some() {
                    return (self.collect_output(), view.exit_code.unwrap_or(0));
                }

                // If the slash command enqueued messages for the model
                // (e.g. `/skill-name <args>` injects the skill body), drain
                // them into the provider-bound `messages` vector and fall
                // through to run a turn. Without this step the skill body
                // never reaches the provider because `self.view.messages`
                // and the local `messages` vector are separate buckets.
                if !self.view.pending_model_messages.is_empty() {
                    let pending = std::mem::take(&mut self.view.pending_model_messages);
                    for m in &pending {
                        messages.push(m.clone());
                    }
                    self.view.used_turns += 1;
                    match self
                        .run_turn(agent_loop, &mut messages, &mut activity_rx)
                        .await
                    {
                        LoopAction::Continue => {}
                        LoopAction::Exit(code) => {
                            return (self.collect_output(), code);
                        }
                    }
                    self.warn_journal_failures(agent_loop);

                    let budget = agent_loop.budget();
                    self.view.used_tokens = budget.used_tokens;
                    let status = self.status_line();
                    self.view.status_line = status;
                }

                continue;
            }

            // Regular user input
            self.history.push(input.clone());
            messages.push(Message {
                role: Role::User,
                content: input.clone(),
                tool_calls: vec![],
                tool_call_id: None,
            });
            self.view.messages.push(user_message(&input));
            self.view.used_turns += 1;

            match self
                .run_turn(agent_loop, &mut messages, &mut activity_rx)
                .await
            {
                LoopAction::Continue => {}
                LoopAction::Exit(code) => {
                    return (self.collect_output(), code);
                }
            }
            self.warn_journal_failures(agent_loop);

            // Update status line
            let budget = agent_loop.budget();
            self.view.used_tokens = budget.used_tokens;
            let status = self.status_line();
            self.view.status_line = status;
        }
    }

    /// Run agent turns until the model finishes (Complete), budget is exhausted,
    /// or an error occurs. Tool calls are executed and fed back automatically
    /// so the agent can iterate.
    ///
    /// When `activity_rx` is `Some`, activity events are rendered in real-time
    /// via `ActivityBlockRenderer` instead of the plain spinner.
    ///
    /// Ctrl-C during the turn interrupts the current agent work and returns
    /// to the REPL prompt.
    async fn run_turn(
        &mut self,
        agent_loop: &mut AgentLoop,
        messages: &mut Vec<Message>,
        activity_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ActivityEvent>>,
    ) -> LoopAction {
        // Drain any stale events from a previous interrupted turn so they
        // don't leak into this turn's rendering.
        if let Some(rx) = activity_rx.as_mut() {
            while rx.try_recv().is_ok() {}
        }

        // Spawn a thread to watch for Ctrl-C during the turn.
        // In raw mode, SIGINT is not generated — we must poll crossterm events.
        let (ctrl_c_tx, mut ctrl_c_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let watching = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let watching_clone = watching.clone();
        let watcher = std::thread::spawn(move || {
            use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
            while watching_clone.load(std::sync::atomic::Ordering::Relaxed) {
                if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false)
                    && let Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers,
                        ..
                    })) = event::read()
                    && modifiers.contains(KeyModifiers::CONTROL)
                {
                    let _ = ctrl_c_tx.send(());
                    return;
                }
            }
        });

        let mut renderer = ActivityBlockRenderer::new();
        let mut retry_count: u32 = 0;
        const MAX_RETRIES: u32 = 3;
        const DEFAULT_RETRY_DELAYS_MS: [u64; 3] = [1_000, 2_000, 4_000];
        let action = 'turn: loop {
            let result = if let Some(rx) = activity_rx.as_mut() {
                // Show "Thinking..." spinner while waiting for the LLM.
                // When the first activity event arrives, the spinner is
                // replaced with a permanent line showing elapsed time.
                let think_start = std::time::Instant::now();
                let mut spinner = Some(start_spinner());

                let cancellation = CancellationToken::new(std::time::Duration::from_secs(1));
                agent_loop.set_cancellation_token(cancellation.clone());
                let turn_fut = agent_loop.run_single_turn(messages);
                tokio::pin!(turn_fut);
                let mut interrupt_sent = false;
                let result = loop {
                    tokio::select! {
                        result = &mut turn_fut => break Some(result),
                        Some(event) = rx.recv() => {
                            if let Some(s) = spinner.take() {
                                stop_spinner(s);
                                clear_spinner_line();
                            }
                            let lines = renderer.process_event(&event);
                            for line in lines {
                                self.io.write_line(&line);
                            }
                        }
                        Some(()) = ctrl_c_rx.recv() => {
                            if let Some(s) = spinner.take() {
                                stop_spinner(s);
                                clear_spinner_line();
                            }
                            if !interrupt_sent {
                                self.io.write_line("^C — interrupted");
                                cancellation.signal();
                                interrupt_sent = true;
                            }
                        }
                    }
                };
                if let Some(s) = spinner.take() {
                    stop_spinner(s);
                    finalize_thinking_line(think_start);
                }
                if let Some(result) = result {
                    // Drain any remaining buffered events
                    while let Ok(event) = rx.try_recv() {
                        let lines = renderer.process_event(&event);
                        for line in lines {
                            self.io.write_line(&line);
                        }
                    }
                    result
                } else {
                    break 'turn LoopAction::Continue;
                }
            } else {
                // No activity events — fall back to plain spinner
                let spinner = start_spinner();
                let cancellation = CancellationToken::new(std::time::Duration::from_secs(1));
                agent_loop.set_cancellation_token(cancellation.clone());
                let turn_fut = agent_loop.run_single_turn(messages);
                tokio::pin!(turn_fut);
                let interrupted = tokio::select! {
                    result = &mut turn_fut => {
                        stop_spinner(spinner);
                        clear_spinner_line();
                        Some(result)
                    }
                    Some(()) = ctrl_c_rx.recv() => {
                        stop_spinner(spinner);
                        clear_spinner_line();
                        self.io.write_line("^C — interrupted");
                        cancellation.signal();
                        Some((&mut turn_fut).await)
                    }
                };
                if let Some(result) = interrupted {
                    result
                } else {
                    break 'turn LoopAction::Continue;
                }
            };

            let has_activity = activity_rx.is_some();

            match result {
                Ok(TurnResult::Complete(msg)) => {
                    self.view.visible_output.push(msg.content.clone());
                    self.io.write_line(&msg.content);
                    self.view.messages.push(msg);
                    break 'turn LoopAction::Continue;
                }
                Ok(TurnResult::ToolCallsProcessed {
                    assistant_message,
                    tool_results,
                }) => {
                    if !has_activity {
                        // Only show tool calls/results when not using activity blocks
                        // (activity blocks already rendered ToolStart/ToolOutput/ToolFinish)
                        for tc in &assistant_message.tool_calls {
                            let line = format!("[tool] {}: {}", tc.name, tc.arguments);
                            self.view.visible_output.push(line.clone());
                            self.io.write_line(&line);
                        }
                        for result in &tool_results {
                            self.view.visible_output.push(result.content.clone());
                            self.io.write_line(&result.content);
                        }
                    }
                    self.view.messages.push(assistant_message);
                    self.view.messages.extend(tool_results.iter().cloned());
                    // Keep looping — let the agent see the results and continue
                }
                Ok(TurnResult::BudgetExhausted) => {
                    let msg = "Budget exhausted.";
                    self.view.visible_output.push(msg.to_string());
                    self.io.write_line(msg);
                    self.view.last_exit_reason = Some(ExitReason::BudgetExhausted);
                    break 'turn LoopAction::Continue;
                }
                Ok(TurnResult::Cancelled) => {
                    let msg = "[cancelled]";
                    self.view.visible_output.push(msg.to_string());
                    self.io.write_line(msg);
                    self.view.last_exit_reason = Some(ExitReason::Cancelled);
                    break 'turn LoopAction::Continue;
                }
                Err(e) => {
                    // Check if this is a retryable provider error
                    let is_retryable = e
                        .as_provider_error()
                        .map(|pe| pe.is_retryable())
                        .unwrap_or(false);

                    if is_retryable && retry_count < MAX_RETRIES {
                        // Determine delay: use retry_after_ms from the error if
                        // available, otherwise use exponential backoff schedule.
                        let delay_ms = e
                            .as_provider_error()
                            .and_then(|pe| match pe {
                                simulacra_types::ProviderError::RateLimit { retry_after_ms } => {
                                    *retry_after_ms
                                }
                                _ => None,
                            })
                            .unwrap_or(DEFAULT_RETRY_DELAYS_MS[retry_count as usize]);

                        retry_count += 1;
                        let delay_secs = (delay_ms as f64) / 1000.0;
                        let msg = format!(
                            "Rate limited, retrying in {delay_secs:.0}s... (attempt {retry_count}/{MAX_RETRIES})"
                        );
                        self.view.visible_output.push(msg.clone());
                        self.io.write_line(&msg);

                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        // Loop back to retry the turn
                        continue 'turn;
                    }

                    let msg = format!("Error: {e}");
                    self.view.visible_output.push(msg.clone());
                    self.io.write_line(&msg);
                    self.view.error = Some(msg);
                    break 'turn LoopAction::Continue;
                }
            }
        };

        // Stop the Ctrl-C watcher thread
        watching.store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = watcher.join();

        action
    }

    /// Drain journal write failure count from the agent loop and display
    /// a warning if any writes failed during the preceding turn. This is
    /// a degraded-but-functional state — the session continues, but replay
    /// may be incomplete.
    fn warn_journal_failures(&mut self, agent_loop: &AgentLoop) {
        let n = agent_loop.drain_journal_write_failures();
        if n > 0 {
            let msg = format!(
                "\u{26a0} {n} journal write(s) failed this turn \u{2014} session replay may be incomplete"
            );
            self.view.warning = Some(msg.clone());
            self.view.visible_output.push(msg.clone());
            self.io.write_line(&msg);
        }
    }

    fn collect_output(&self) -> String {
        // In interactive mode, all output was already displayed in real-time
        // via write_line. Return empty to avoid re-printing on exit.
        if self.io.is_tty() {
            String::new()
        } else {
            self.view.visible_output.join("\n")
        }
    }
}

// ---------------------------------------------------------------------------
// Session resume (production) — the `--session <id>` flag calls this on boot
// to restore messages and VFS state from a persisted checkpoint.
// ---------------------------------------------------------------------------

impl<P, I> InteractiveSession<P, I>
where
    P: Provider,
    I: InteractiveInput + InteractiveOutput,
{
    pub fn resume_from_storage(&mut self, session_id: &str) -> SessionView {
        if let Ok(Some(session)) = self.storage.load(session_id) {
            let msg_count = session.messages.len();
            let turns_used = session
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .count();
            let persisted_tokens = session.used_tokens;
            let persisted_turns = session.used_turns;
            self.view.messages = session.messages;
            self.view.session_id = session_id.to_string();
            self.view.resumed_summary = Some(format!(
                "Resumed session {session_id} ({msg_count} messages, {turns_used} turns used)"
            ));
            self.view.used_tokens = persisted_tokens;
            self.view.used_turns = persisted_turns;
            if let Some(snapshot) = &session.vfs_snapshot
                && self.vfs.restore(snapshot).is_ok()
                && let Ok(entries) = self.vfs.list_dir("/")
            {
                self.populate_restored_vfs_prod("/", &entries);
            }
            let _span = tracing::info_span!(
                "session_resume",
                "simulacra.operation.name" = "session_resume",
                "simulacra.session.id" = session_id,
                "simulacra.session.message_count" = msg_count,
            )
            .entered();
        }
        self.view.clone()
    }

    fn populate_restored_vfs_prod(&mut self, dir: &str, entries: &[String]) {
        for name in entries {
            let path = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            if let Ok(data) = self.vfs.read(&path) {
                if let Ok(text) = String::from_utf8(data) {
                    self.view.restored_vfs.insert(path, text);
                }
            } else if let Ok(children) = self.vfs.list_dir(&path) {
                self.populate_restored_vfs_prod(&path, &children);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only helpers — gated behind the `test-support` feature
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-support"))]
impl<P, I> InteractiveSession<P, I>
where
    P: Provider,
    I: InteractiveInput + InteractiveOutput,
{
    pub fn snapshot(&self) -> SessionView {
        self.view.clone()
    }

    pub fn seed_messages(&mut self, messages: Vec<Message>) {
        self.view.messages.extend(messages);
    }

    pub fn seed_output(&mut self, lines: &[&str]) {
        self.view
            .visible_output
            .extend(lines.iter().map(|line| (*line).to_string()));
    }

    pub fn seed_budget(&mut self, used_tokens: u64, used_turns: u32) {
        self.view.used_tokens = used_tokens;
        self.view.used_turns = used_turns;
    }

    pub fn parse_multiline_input(&self, lines: &[&str]) -> Option<String> {
        let mut result = String::new();
        for (i, line) in lines.iter().enumerate() {
            if let Some(stripped) = line.strip_suffix('\\') {
                result.push_str(stripped);
                if i < lines.len() - 1 {
                    result.push('\n');
                }
            } else {
                result.push_str(line);
            }
        }
        Some(result)
    }

    pub fn navigate_history(&mut self, direction: HistoryDirection) -> Option<String> {
        match direction {
            HistoryDirection::Up => self.history.last().cloned(),
            HistoryDirection::Down => self.history.first().cloned(),
        }
    }

    pub fn process_streaming_events(&mut self, events: Vec<StreamEvent>) -> SessionView {
        for event in events {
            match event {
                StreamEvent::Token(token) => {
                    // Prefix child-visible output with agent identity so the user
                    // can distinguish it from the parent assistant and tool blocks.
                    let prefixed = if let Some(ref child_type) = self.active_child_type {
                        let child_id = self.view.session_id.as_str();
                        format!("[agent:{child_type}/{child_id}] {token}")
                    } else {
                        token
                    };
                    self.view.stream_frames.push(prefixed.clone());
                    self.view.visible_output.push(prefixed);
                }
                StreamEvent::ToolCall(tc) => {
                    // Always show the tool call in the standard [tool] format.
                    let frame = format!("[tool] {}: {}", tc.name, tc.arguments);
                    self.view.stream_frames.push(frame.clone());
                    self.view.visible_output.push(frame);

                    if tc.name == "spawn_agent" {
                        // Mark a child as running so subsequent Token events
                        // and cancellation/failure messages are prefixed with
                        // the child agent identity.
                        let child_type = tc
                            .arguments
                            .get("agent_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        self.active_child_type = Some(child_type.clone());
                        tracing::info!("spawn started");

                        // Also render an error line with the child prefix when
                        // a spawn_agent call appears in the stream (failures).
                        let child_id = self.view.session_id.as_str();
                        let error_frame = format!(
                            "[agent:{child_type}/{child_id}] error: spawn_agent {}",
                            tc.arguments
                        );
                        self.view.stream_frames.push(error_frame.clone());
                        self.view.visible_output.push(error_frame);
                    }
                }
                StreamEvent::Done => {
                    tracing::info!("child finished");
                }
            }
        }
        self.view.clone()
    }

    pub fn process_response(&mut self, response: ProviderResponse) -> SessionView {
        self.view
            .visible_output
            .push(response.message.content.clone());
        self.view.clone()
    }

    pub fn cancel_llm_request(&mut self, _partial: &str) -> SessionView {
        tracing::info!(
            simulacra.cancel.target = "llm_request",
            "cancellation event"
        );
        self.view.visible_output.push("[cancelled]".to_string());
        // Partial message is discarded — not added to messages
        self.view.clone()
    }

    pub fn cancel_tool_execution(&mut self) -> SessionView {
        // Show cancellation with the child prefix so the user sees it before
        // the parent turn resumes. Use the active child type if a spawn is in
        // flight, otherwise fall back to the configured can_spawn type.
        let child_type_owned = self
            .active_child_type
            .clone()
            .or_else(|| self.config.can_spawn.first().cloned());
        if let Some(ref child_type) = child_type_owned {
            let child_id = self.view.session_id.as_str();
            let line = format!("[agent:{child_type}/{child_id}] cancelled");
            self.view.visible_output.push(line);
            tracing::info!("child cancelled");

            let structured = serde_json::json!({
                "error": "cancelled by user",
                "agent_type": child_type,
            });
            self.view.tool_results_to_model.push(Message {
                role: Role::Tool,
                content: structured.to_string(),
                tool_calls: vec![],
                tool_call_id: Some("cancelled".to_string()),
            });
        } else {
            self.view.tool_results_to_model.push(Message {
                role: Role::Tool,
                content: "Cancelled by user".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("cancelled".to_string()),
            });
        }
        self.view.clone()
    }

    pub fn handle_prompt_ctrl_c(&mut self, press_intervals_ms: &[u64]) -> SessionView {
        self.view.warning = Some("Press Ctrl-C again to exit, or type /exit".into());
        if press_intervals_ms.len() > 1 && press_intervals_ms[1] <= 2_000 {
            self.view.exit_code = Some(0);
        }
        self.view.clone()
    }

    pub fn force_quit_during_request(&mut self, press_intervals_ms: &[u64]) -> SessionView {
        if press_intervals_ms.len() > 1 && press_intervals_ms[1] <= 500 {
            self.view.forced_exit_without_save = true;
        }
        self.view.clone()
    }

    pub fn submit_turn(&mut self, input: &str) -> SessionView {
        if input.is_empty() {
            return self.view.clone();
        }

        self.view.approve_all_active = false;
        self.history.push(input.to_string());
        self.view.messages.push(user_message(input));
        self.view.used_turns += 1;

        self.ensure_session_span();
        let _turn_span = tracing::info_span!(
            "interactive_turn",
            "simulacra.operation.name" = "interactive_turn",
            "simulacra.turn.number" = self.view.used_turns,
        )
        .entered();

        tracing::info!(
            simulacra.interactive.turns = self.view.used_turns,
            "interactive turn completed"
        );

        self.view.clone()
    }

    pub fn append_tool_result_from_previous_turn(
        &mut self,
        tool_call_id: &str,
        content: &str,
        _is_error: bool,
    ) {
        self.view.messages.push(Message {
            role: Role::Tool,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.to_string()),
        });
    }

    pub fn handle_provider_error(&mut self, error: ProviderError) -> SessionView {
        self.view.error = Some(error.to_string());
        self.view.visible_output.push(error.to_string());
        if error.is_retryable() {
            self.view.retry_delays_ms = vec![1_000, 2_000, 4_000];
            self.view
                .visible_output
                .push("Retrying in 1s...".to_string());
        }
        self.view.clone()
    }

    pub fn handle_tool_error(&mut self, message: &str) -> SessionView {
        self.view.visible_output.push(message.to_string());
        self.view.tool_results_to_model.push(Message {
            role: Role::Tool,
            content: message.to_string(),
            tool_calls: vec![],
            tool_call_id: Some("error".to_string()),
        });
        self.view.clone()
    }

    pub fn handle_journal_write_failure(&mut self, message: &str) -> SessionView {
        self.view.error = Some(message.to_string());
        // Journal failures are non-fatal (WARN only), no exit_code set
        self.view.clone()
    }

    pub fn run_piped_input_once(&mut self, input: &str) -> SessionView {
        let _ = self.submit_turn(input);
        self.view.auto_approved_tools = true;
        self.view.exit_code = Some(0);
        self.view.clone()
    }

    pub fn handle_eof(&mut self) -> SessionView {
        self.save_checkpoint("completed");
        self.view.exit_code = Some(0);
        self.view.clone()
    }

    pub fn simulate_terminal_restore(&mut self, _panic: bool) -> SessionView {
        // Terminal is always restored: on graceful exit, forced exit, and panic (via drop guard)
        self.view.terminal_restored = true;
        self.io.restore_terminal();
        self.view.clone()
    }

    pub fn agent_loop_type_name(&self) -> &'static str {
        std::any::type_name::<AgentLoop>()
    }

    pub fn awaiting_approval_exit_reason(&self) -> ExitReason {
        ExitReason::AwaitingApproval
    }

    pub fn reuses_headless_bootstrap_path(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// LoopAction — internal control flow for the REPL loop
// ---------------------------------------------------------------------------

#[allow(dead_code)]
enum LoopAction {
    Continue,
    Exit(i32),
}
