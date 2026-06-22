use std::collections::HashMap;

use simulacra_tool::SkillMeta;
use simulacra_types::{ExitReason, Message, Role, ToolCallMessage, ToolDefinition};

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

pub trait InteractiveInput {
    fn read_line(&mut self) -> Option<String>;
    fn read_approval(&mut self) -> Option<String>;
    fn is_tty(&self) -> bool;
}

pub trait InteractiveOutput {
    fn write_line(&mut self, line: &str);
    fn clear(&mut self);
    fn restore_terminal(&mut self);
}

// ---------------------------------------------------------------------------
// StreamEvent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Token(String),
    ToolCall(ToolCallMessage),
    Done,
}

// ---------------------------------------------------------------------------
// SessionView
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct SessionView {
    pub header: Vec<String>,
    pub visible_output: Vec<String>,
    pub status_line: String,
    pub session_id: String,
    pub messages: Vec<Message>,
    pub provider_calls: Vec<Vec<Message>>,
    pub tool_results_to_model: Vec<Message>,
    pub approval_prompts: Vec<String>,
    pub executed_tools: Vec<String>,
    pub stream_frames: Vec<String>,
    pub resumed_summary: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub saved_session: bool,
    pub approve_all_active: bool,
    pub forced_exit_without_save: bool,
    pub terminal_restored: bool,
    pub auto_approved_tools: bool,
    pub used_tokens: u64,
    pub used_turns: u32,
    pub retry_delays_ms: Vec<u64>,
    pub restored_vfs: HashMap<String, String>,
    pub last_exit_reason: Option<ExitReason>,
    /// Messages queued by a slash-command (e.g. `/skill-name <args>`) to be
    /// forwarded to the provider on the next turn. The interactive loop
    /// drains this buffer into its own `messages` vector before invoking
    /// `run_turn`, then clears it.
    pub pending_model_messages: Vec<Message>,
}

// ---------------------------------------------------------------------------
// InteractiveSessionConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct InteractiveSessionConfig {
    pub project_name: String,
    pub model: String,
    pub max_tokens: u64,
    pub max_turns: u32,
    pub task: Option<String>,
    pub requested_session_id: Option<String>,
    pub tool_definitions: Vec<ToolDefinition>,
    pub can_spawn: Vec<String>,
    /// S017: Effective skill catalog for /skill-name interactive invocation.
    /// In interactive mode, `/skill-name` is a reserved slash-command form for
    /// user-invocable skills. Slash-command resolution order is:
    ///   1. built-in interactive commands from S015
    ///   2. resolved user-invocable skill names
    ///   3. otherwise the existing "unknown command" path from S015
    ///
    /// When the user enters `/skill-name <args>`, the interactive host resolves
    /// `skill-name`, loads the same skill body, and injects it into the
    /// upcoming turn context before sending the optional trailing `<args>` text
    /// to the model.
    ///
    /// The trailing `args` text after `/skill-name` is sent to the model as
    /// the user's instruction for that skill invocation. If no args are
    /// provided, the turn still loads the skill and the model may ask a
    /// follow-up question.
    ///
    /// User-triggered skill loading does not require model approval and does
    /// not appear as an LLM-emitted tool call. It is a host-side context
    /// injection path.
    ///
    /// A skill with `user_invocable: false` is not available through
    /// `/skill-name`. Direct invocation falls through to the unknown command
    /// path.
    ///
    /// A skill that is capability-denied is not invocable through `/skill-name`,
    /// even if it exists on disk.
    ///
    /// User-triggered skill loads are recorded as host-side session events
    /// before provider execution so the source of the injected prompt remains
    /// attributable.
    pub skill_catalog: Vec<SkillMeta>,
}

// ---------------------------------------------------------------------------
// HistoryDirection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum HistoryDirection {
    Up,
    Down,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn user_message(content: &str) -> Message {
    Message {
        role: Role::User,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
    }
}
