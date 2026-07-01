use std::sync::Arc;

use simulacra_types::{AgentId, JournalStorage};
use tokio::sync::Mutex;

use crate::manager::McpManager;
use crate::tool::McpTool;
use crate::wasm::load_wasm_mcp_module;

/// Connect to MCP servers and return `Tool` wrappers for all discovered tools.
///
/// This is the main integration point for the CLI bootstrap. It takes server
/// descriptors as `(name, url, transport)` tuples to avoid a dependency on
/// `simulacra-config`.
///
/// Each server is connected using its config `name` as the routing key (not
/// the URL hostname), so `call_tool` dispatches correctly even when multiple
/// servers share a hostname.
///
/// Servers that fail to connect are logged as warnings and skipped — they do
/// not prevent other servers from registering their tools.
pub async fn create_mcp_tools(
    servers: &[(String, Option<String>, Option<String>)],
) -> Vec<McpTool> {
    create_mcp_tools_with_wasm(servers, &[]).await
}

/// MCP server descriptor for the WASM transport. Carries the per-server
/// `host:port` allowlist that `simulacra:mcp/http.fetch` consults before any
/// outbound HTTP, plus the hook pipeline and journal that govern the
/// fetch path in production. Field shapes mirror `simulacra_config::McpServerConfig`
/// but are repeated here so this crate stays free of a `simulacra-config`
/// dependency.
///
/// Available regardless of the `wasm` feature so consumers (e.g.
/// `simulacra-cli`) can build their bootstrap without re-gating on the
/// feature flag — when `wasm` is disabled, `create_mcp_tools_with_wasm`
/// logs a warning and skips each WASM descriptor.
#[derive(Clone)]
pub struct WasmMcpServerDescriptor {
    pub name: String,
    pub module_path: std::path::PathBuf,
    pub network_allowlist: Vec<String>,
    /// Governance hook pipeline. When set, every `simulacra:mcp/http.fetch`
    /// invocation runs the `Operation::HttpRequest` chain at
    /// `Phase::Before` (before wire dispatch) and `Phase::After`
    /// (before returning to the module).
    pub hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    /// Journal storage. When set, every fetch attempt writes one
    /// `JournalEntryKind::HttpRequest` entry BEFORE wire dispatch
    /// (Golden Rule).
    pub journal: Option<Arc<dyn JournalStorage>>,
    /// Agent ID used when journaling fetches from this server. Empty
    /// AgentId means "unattributed" (acceptable for shared CLI
    /// bootstrap; agent-scoped journaling is a future spec).
    pub agent_id: AgentId,
}

impl std::fmt::Debug for WasmMcpServerDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmMcpServerDescriptor")
            .field("name", &self.name)
            .field("module_path", &self.module_path)
            .field("network_allowlist", &self.network_allowlist)
            .field("hooks", &self.hooks.is_some())
            .field("journal", &self.journal.is_some())
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

/// Like `create_mcp_tools` but additionally compiles + connects WASM MCP
/// servers (`transport = "wasm"`). All servers — HTTP/SSE plus WASM —
/// share the same `McpManager`, so capability enforcement, tool routing,
/// and observability stay uniform across transports.
///
/// Always callable. When the `wasm` feature is disabled, WASM descriptors
/// are logged at WARN and skipped so the call still returns the network
/// servers' tools.
pub async fn create_mcp_tools_with_wasm(
    network_servers: &[(String, Option<String>, Option<String>)],
    wasm_servers: &[WasmMcpServerDescriptor],
) -> Vec<McpTool> {
    let manager = Arc::new(Mutex::new(McpManager::new()));

    // Connect HTTP / SSE servers first.
    for (name, url, transport) in network_servers {
        let url = match url {
            Some(u) => u.as_str(),
            None => continue,
        };
        let transport = transport.as_deref();
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.connect_named(name, url, transport).await {
            tracing::warn!(
                server = %name,
                error = %e,
                "failed to connect MCP server"
            );
        }
    }

    // Compile + connect WASM modules. Failure to load any one module is
    // logged and the server is skipped — the rest of the registry still
    // boots, mirroring the network-server fallthrough behavior.
    //
    // Each descriptor's hooks + journal flow into the WasmMcpModule so
    // `simulacra:mcp/http.fetch` runs through the same governance pipeline
    // and journal that govern host-side fetches. The seam is fully
    // wired in production, not just in tests.
    #[cfg(feature = "wasm")]
    for descriptor in wasm_servers {
        match load_wasm_mcp_module(&descriptor.module_path) {
            Ok(mut module) => {
                module = module.with_network_allowlist(descriptor.network_allowlist.clone());
                module = module.with_agent_id(descriptor.agent_id.clone());
                if let Some(ref hooks) = descriptor.hooks {
                    module = module.with_hooks(Arc::clone(hooks));
                }
                if let Some(ref journal) = descriptor.journal {
                    module = module.with_journal(Arc::clone(journal));
                }
                let mut mgr = manager.lock().await;
                if let Err(e) = mgr.connect_wasm_module(&descriptor.name, module).await {
                    tracing::warn!(
                        server = %descriptor.name,
                        error = %e,
                        "failed to connect WASM MCP server"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    server = %descriptor.name,
                    module = %descriptor.module_path.display(),
                    error = %e,
                    "failed to load WASM MCP module"
                );
            }
        }
    }
    #[cfg(not(feature = "wasm"))]
    for descriptor in wasm_servers {
        tracing::warn!(
            server = %descriptor.name,
            "WASM MCP server skipped — simulacra-mcp built without `wasm` feature"
        );
    }

    // Trigger handshakes and collect tools with server attribution.
    let tools_by_server = {
        let mut mgr = manager.lock().await;
        mgr.list_tools_by_server().await
    };

    tools_by_server
        .into_iter()
        .map(|(server_name, tool_def)| McpTool::new(Arc::clone(&manager), server_name, tool_def))
        .collect()
}
