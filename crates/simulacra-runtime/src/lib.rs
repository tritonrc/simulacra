//! Simulacra runtime crate.
//!
//! Provides session management, journal storage, agent supervision, the agent
//! loop, and guardrail traits. This is the top-level orchestration layer that
//! composes sandbox, provider, context, and MCP capabilities.

mod acp_child;
mod activity_sink;
mod agent_loop;
mod error;
mod exit_reason;
mod guardrail;
mod journal;
mod journal_sqlite;
mod replay;
mod session;
mod session_file;
mod session_sqlite;
#[cfg(feature = "spawn")]
mod spawn_tool;
mod sqlite_util;
mod supervisor;
#[cfg(test)]
mod tests;
#[cfg(feature = "spawn")]
mod vfs_hook;

// Re-export all public types at the crate root.
pub use acp_child::{AcpChildFuture, AcpChildRequest, AcpChildRuntime};
pub use activity_sink::{
    ActivitySink, ChannelActivitySink, ForwardingActivitySink, NoopActivitySink,
};
pub use agent_loop::{
    ActiveTurn, AgentHitlRuntime, AgentHitlSenders, AgentInputQueue, AgentLoop, AgentLoopConfig,
    AgentLoopOutput, ChildInputHandle, REQUEST_INPUT_TOOL_NAME, RequestInputTool, StepContext,
    ToolApprovalResponse, TurnContext, TurnResult, TurnState,
};
pub use error::RuntimeError;
pub use guardrail::{GuardrailDecision, InputGuardrail, OutputGuardrail};
pub use journal::{CountingJournalStorage, InMemoryJournalStorage};
pub use journal_sqlite::SqliteJournalStorage;
pub use replay::JournalReplayIterator;
pub use session::{InMemorySessionStorage, Session, SessionStorage};
pub use session_file::FileSessionStorage;
pub use session_sqlite::SqliteSessionStorage;
#[cfg(feature = "spawn")]
pub use spawn_tool::{
    AgentTaskFactory, CancelChildAgentTool, ChildCellConfigurator, ChildProviderFactory,
    ChildStatusTool, ChildToolRegistrar, CloseChildAgentTool, DEFAULT_SYSTEM_PROMPT,
    JoinChildAgentTool, ListChildAgentTool, NoopContextStrategy, ProviderKind, SpawnAgentTool,
    SteerChildAgentTool, WaitChildAgentTool,
};
pub use supervisor::{
    AgentSupervisor, BoxTaskFuture, CancellationToken, ChildMetadata, ChildRosterEntry,
    ChildStatus, ChildTerminalResult, MessagePriority, RestartStrategy, SpawnAck, SpawnConfig,
    SpawnResult, SupervisorMessage, SupervisorPayload, TaskFactory, WaitChildResult,
    WaitChildrenResult,
};
#[cfg(feature = "spawn")]
pub use vfs_hook::HookedVfsLayer;
