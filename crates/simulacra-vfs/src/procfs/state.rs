use std::sync::{Arc, Mutex, atomic::AtomicU64};
use std::time::Instant;

use simulacra_types::{CapabilityToken, ResourceBudget, VirtualFs};

/// Provides tool names and compact-JSON definitions for `/proc/tools/`.
pub trait ToolLister: Send + Sync + 'static {
    /// All tool names, in any order. ProcFs will sort them.
    fn tool_names(&self) -> Vec<String>;
    /// Compact JSON object for the given tool, or `None` if not found.
    fn tool_json(&self, name: &str) -> Option<String>;
}

/// Provides hook names per operation type for `/proc/hooks/`.
pub trait HookLister: Send + Sync + 'static {
    /// Hook names registered for `operation` (e.g. `"tool_call"`), in order.
    fn hook_names(&self, operation: &str) -> Vec<String>;
}

/// All the live runtime state that `/proc` files expose.
pub struct ProcState {
    pub agent_id: String,
    pub agent_name: String,
    pub model: String,
    pub parent_id: Option<String>,
    pub budget: Arc<Mutex<ResourceBudget>>,
    pub capabilities: CapabilityToken,
    pub tools: Arc<dyn ToolLister>,
    pub session_id: String,
    pub session_start: Instant,
    pub journal_entries: Arc<AtomicU64>,
    pub hooks: Arc<dyn HookLister>,
    pub turn: Arc<AtomicU64>,
}

/// A [`VirtualFs`] layer that adds a virtual `/proc` directory.
pub struct ProcFs<V: VirtualFs> {
    pub(super) inner: V,
    pub(super) state: Arc<ProcState>,
}

impl<V: VirtualFs> ProcFs<V> {
    /// Wrap `inner` with a `/proc` layer backed by `state`.
    pub fn new(inner: V, state: Arc<ProcState>) -> Self {
        Self { inner, state }
    }
}
