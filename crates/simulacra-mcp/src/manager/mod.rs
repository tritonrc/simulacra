mod call;
mod capability;
mod dispatch;
mod handshake;
mod journal;
mod reconnect;
mod registry;
#[cfg(feature = "wasm")]
mod wasm_dispatch;

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(feature = "wasm")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};

use simulacra_types::{AgentId, JournalStorage};

use crate::transport::state::McpConnection;
#[cfg(feature = "wasm")]
use crate::wasm::WasmMcpModule;

/// Manager for MCP server connections.
///
/// Holds active connections and aggregates tool definitions from
/// all connected servers.
pub struct McpManager {
    pub(crate) connections: HashMap<String, McpConnection>,
    /// Optional Journal storage for recording ToolCall entries.
    #[allow(dead_code)]
    pub(crate) journal: Option<Arc<dyn JournalStorage>>,
    /// Agent ID for journal entries.
    #[allow(dead_code)]
    pub(crate) agent_id: AgentId,
    /// Base delay in milliseconds for reconnection exponential backoff.
    pub(crate) reconnect_base_delay_ms: u64,
    pub(crate) agent_fuel_remaining: Option<Arc<AtomicU64>>,
    #[cfg(feature = "wasm")]
    pub(crate) instantiation_recorder: Option<Arc<AtomicUsize>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_modules: HashMap<String, WasmMcpModule>,
}

impl McpManager {
    /// Create a new MCP manager with no connections.
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            journal: None,
            agent_id: AgentId(String::new()),
            reconnect_base_delay_ms: 1000,
            agent_fuel_remaining: None,
            #[cfg(feature = "wasm")]
            instantiation_recorder: None,
            #[cfg(feature = "wasm")]
            wasm_modules: HashMap::new(),
        }
    }

    /// Create a new MCP manager with journal support.
    #[allow(dead_code)]
    pub fn with_journal(journal: Arc<dyn JournalStorage>, agent_id: AgentId) -> Self {
        Self {
            connections: HashMap::new(),
            journal: Some(journal),
            agent_id,
            reconnect_base_delay_ms: 1000,
            agent_fuel_remaining: None,
            #[cfg(feature = "wasm")]
            instantiation_recorder: None,
            #[cfg(feature = "wasm")]
            wasm_modules: HashMap::new(),
        }
    }

    /// Override the base delay for reconnection backoff (milliseconds).
    pub fn set_reconnect_base_delay_ms(&mut self, ms: u64) {
        self.reconnect_base_delay_ms = ms;
    }

    /// Set the agent-level fuel budget for WASM MCP calls.
    pub fn set_agent_fuel_budget(&mut self, fuel: u64) {
        self.agent_fuel_remaining = Some(Arc::new(AtomicU64::new(fuel)));
    }

    /// Inspect the agent-level fuel budget remaining for WASM MCP calls.
    pub fn agent_fuel_budget_remaining(&self) -> Option<u64> {
        self.agent_fuel_remaining
            .as_ref()
            .map(|arc| arc.load(Ordering::Acquire))
    }

    pub(crate) fn discard_server(&mut self, server: &str) {
        self.connections.remove(server);
        #[cfg(feature = "wasm")]
        self.wasm_modules.remove(server);
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}
