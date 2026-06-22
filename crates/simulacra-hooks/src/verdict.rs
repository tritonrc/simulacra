use std::fmt;

use serde::{Deserialize, Serialize};

/// Phase of hook execution relative to the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Before,
    After,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::Before => write!(f, "before"),
            Phase::After => write!(f, "after"),
        }
    }
}

/// The type of operation a hook intercepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    ToolCall,
    Llm,
    Spawn,
    HttpRequest,
    VfsWrite,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operation::ToolCall => write!(f, "tool_call"),
            Operation::Llm => write!(f, "llm"),
            Operation::Spawn => write!(f, "spawn"),
            Operation::HttpRequest => write!(f, "http_request"),
            Operation::VfsWrite => write!(f, "vfs_write"),
        }
    }
}

/// The verdict returned by a hook invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Continue processing. Optionally contains modified context JSON.
    Continue(Option<String>),
    /// Deny the operation with a reason.
    Deny(String),
    /// Kill the agent entirely with a reason.
    Kill(String),
}

impl Verdict {
    /// Create a Continue verdict with no modifications.
    pub fn continue_unchanged() -> Self {
        Verdict::Continue(None)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Continue(None) => write!(f, "continue"),
            Verdict::Continue(Some(_)) => write!(f, "continue(modified)"),
            Verdict::Deny(reason) => write!(f, "deny: {reason}"),
            Verdict::Kill(reason) => write!(f, "kill: {reason}"),
        }
    }
}
