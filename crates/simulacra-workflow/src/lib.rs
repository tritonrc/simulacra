//! Restricted workflow runtime for Simulacra-authored ESM workflow scripts.
//!
//! Workflow scripts execute in QuickJS with a restricted host API profile. The
//! only side-effecting operation exposed by this crate is `agent()`, which
//! delegates to a caller-provided [`WorkflowWorker`].

mod error;
mod runtime;
mod store;
mod tool;
mod types;

pub use error::WorkflowError;
pub use runtime::{WorkflowRunHandle, WorkflowRunOptions, WorkflowRuntime};
pub use store::WorkflowStore;
pub use tool::WorkflowTool;
pub use types::{
    WorkflowAgentCall, WorkflowAgentResult, WorkflowEvent, WorkflowRun, WorkflowScript,
    WorkflowScriptMeta, WorkflowStatus, WorkflowWorker, WorkflowWorkerFuture,
};
