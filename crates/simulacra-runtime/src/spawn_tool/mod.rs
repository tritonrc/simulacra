//! SpawnAgentTool and AgentTaskFactory — moved from simulacra-cli.
//!
//! `SpawnAgentTool` is a `Tool` implementation that sends spawn requests to the
//! supervisor via an mpsc channel. `AgentTaskFactory` is the `TaskFactory`
//! implementation that constructs child `AgentLoop` instances.

mod factory;
mod helpers;
mod proc_runtime;
mod prompt;
mod tool;
mod types;

#[cfg(test)]
mod tests;

pub use factory::{AgentTaskFactory, ChildCellConfigurator, ChildToolRegistrar};
pub use prompt::DEFAULT_SYSTEM_PROMPT;
pub use tool::SpawnAgentTool;
pub use types::{NoopContextStrategy, ProviderKind};

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_config::{SimulacraConfig, TierMap, build_capability_token};
use simulacra_provider::{AnthropicProvider, OpenAiProvider};
use simulacra_types::{
    ActivityEvent, AgentId, CapabilityToken, ContextStrategy, JournalStorage, Message,
    NetworkPermission, PathPattern, Provider, ResourceBudget, ToolDefinition, VirtualFs,
};
use simulacra_vfs::{HookLister, ProcFs, ProcState, ToolLister};

use crate::exit_reason::exit_reason_to_snake_case;
use crate::{
    ActivitySink, AgentLoop, AgentLoopConfig, BoxTaskFuture, CancellationToken,
    CountingJournalStorage, ForwardingActivitySink, MessagePriority, RuntimeError, SpawnConfig,
    SupervisorMessage, SupervisorPayload,
};

use helpers::{
    inherit_memory_when_override_unset, parent_tier_name, parse_capability_override,
    resolve_tier_model,
};
use proc_runtime::{ChildProcSpec, child_proc_runtime};
