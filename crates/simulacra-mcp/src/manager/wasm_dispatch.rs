use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opentelemetry::KeyValue;
use simulacra_types::AgentId;

use crate::domain::tool_schema::McpToolSchema;
use crate::error::McpError;
use crate::observability::McpMeters;
use crate::transport::state::{McpConnection, TransportMode};
use crate::wasm::bindings::wit_server;
use crate::wasm::{WasmMcpModule, build_wasm_mcp_linker, build_wasm_mcp_store};

use super::McpManager;

impl McpManager {
    /// Register a WASM MCP server by module path.
    ///
    /// Phase 1c keeps this as a *structural* register stub (no compile, no
    /// instantiate) on purpose: the wasm_mcp_config tests that exercise
    /// capability-ordering need the connection to exist so the call_tool
    /// path runs the cap check at all. The dispatch-time WASM behavior is
    /// still `unimplemented!()` via `WasmMcpModule::dispatch` below.
    #[cfg(feature = "wasm")]
    pub async fn connect_wasm_named(
        &mut self,
        name: &str,
        module_id: &str,
    ) -> Result<(), McpError> {
        self.connections.insert(
            name.to_string(),
            McpConnection {
                server_name: name.to_string(),
                url: "http://127.0.0.1/".to_string(),
                headers: Vec::new(),
                tools: Vec::new(),
                handshake_done: false,
                was_connected: false,
                transport_mode: Some(TransportMode::Wasm {
                    module_id: module_id.to_string(),
                }),
                configured_transport: Some("wasm".to_string()),
            },
        );

        Ok(())
    }

    /// Register a WASM MCP server using a fully-loaded [`WasmMcpModule`].
    ///
    /// The module's `(ToolDefinition, WasmTool)` pairs are installed on the
    /// connection so `list_tools` returns them and `call_tool` can dispatch
    /// through the cached component. The connection is marked
    /// `handshake_done = true` because all the work `list-tools` would have
    /// done over the wire has already happened in `load_wasm_mcp_module`.
    pub async fn connect_wasm_module(
        &mut self,
        name: &str,
        module: WasmMcpModule,
    ) -> Result<(), McpError> {
        #[cfg(feature = "wasm")]
        {
            // S041 §Observability: WASM MCP servers complete their handshake
            // in-process (the module's `list-tools` already ran during
            // `load_wasm_mcp_module`), but we still emit a
            // `simulacra_mcp_handshake` span so o11y consumers can correlate
            // WASM and HTTP/SSE MCP transports under a single span name.
            let handshake_span = tracing::info_span!(
                "simulacra_mcp_handshake",
                simulacra.mcp.transport_mode = "wasm",
                simulacra.mcp.module_id = name,
            );
            let _handshake_guard = handshake_span.enter();

            // Bridge each ToolDefinition into the existing McpToolSchema shape
            // so the rest of the manager (list_tools, etc.) treats wasm
            // transports identically to HTTP/SSE.
            let tool_schemas = module
                .tools
                .iter()
                .map(|def| McpToolSchema {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    input_schema: def.input_schema.clone(),
                })
                .collect();

            self.connections.insert(
                name.to_string(),
                McpConnection {
                    server_name: name.to_string(),
                    url: String::new(),
                    headers: Vec::new(),
                    tools: tool_schemas,
                    handshake_done: true,
                    was_connected: true,
                    transport_mode: Some(TransportMode::Wasm {
                        module_id: name.to_string(),
                    }),
                    configured_transport: Some("wasm".to_string()),
                },
            );

            self.wasm_modules.insert(name.to_string(), module);
            Ok(())
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = (name, module);
            Err(McpError::ConnectionFailed(
                "WASM MCP support is disabled (simulacra-mcp built without `wasm` feature)"
                    .to_string(),
            ))
        }
    }

    /// Install an `AtomicUsize` that the runtime increments every time it
    /// instantiates a wasmtime component for a WASM MCP call. Used by
    /// `wasm_mcp_transport.rs` to verify the agent-fuel short-circuit
    /// happens before any wasmtime work.
    pub fn set_instantiation_recorder(&mut self, recorder: Arc<AtomicUsize>) {
        self.instantiation_recorder = Some(recorder);
    }

    /// Dispatch the actual JSON-RPC `tools/call` request to the MCP server.
    ///
    /// Dispatch a tool call to a wasm-transport MCP server.
    ///
    /// Order of operations matches the spec § Tool dispatch:
    ///   1. Look up the loaded module (must exist post-handshake).
    ///   2. Resolve the tool by name on the module's discovered tool list;
    ///      unknown tools surface as `ProtocolError("execution failed: ...")`
    ///      so the agent-facing `ToolError::ExecutionFailed` round-trip works.
    ///   3. Pre-flight the agent fuel budget: `Some(0)` short-circuits
    ///      WITHOUT instantiating the component. The instantiation
    ///      recorder (if installed) ticks only AFTER this check passes.
    ///   4. On a blocking pool, build a fresh `Linker` + `Store`, instantiate
    ///      the module's compiled `simulacra:mcp/server` Component, and call
    ///      `call-tool`. The store carries the per-call WASI ctx, the
    ///      module's allowlist/hooks/journal, and the captured runtime
    ///      handle so the `simulacra:mcp/http.fetch` host import bridges sync
    ///      → async cleanly.
    ///   5. Read the post-call fuel residual via `store.get_fuel()` so
    ///      the span carries `simulacra.wasm.fuel_consumed`. Map
    ///      `tool-error::invalid-arguments`/`execution-failed` to
    ///      `McpError::ProtocolError` so the existing reconnect/retry
    ///      plumbing treats them uniformly.
    #[cfg(feature = "wasm")]
    pub(crate) async fn dispatch_wasm_tool_call(
        &self,
        agent_id: &AgentId,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        // S041 §Observability: dedicated `simulacra_mcp_tool_call` span carries
        // `simulacra.wasm.fuel_consumed` so the WASM transport's fuel
        // accounting surfaces alongside the standard MCP span hierarchy.
        let tool_call_span = tracing::info_span!(
            "simulacra_mcp_tool_call",
            simulacra.mcp.transport_mode = "wasm",
            simulacra.mcp.module_id = server,
            simulacra.tool.name = tool,
            simulacra.wasm.fuel_consumed = tracing::field::Empty,
        );
        let _tool_call_guard = tool_call_span.enter();

        let module = self.wasm_modules.get(server).ok_or_else(|| {
            McpError::ConnectionFailed(format!("no wasm module loaded for server {server}"))
        })?;

        if !module.tools.iter().any(|def| def.name == tool) {
            let msg =
                format!("execution failed: tool '{tool}' not found on wasm MCP server '{server}'");
            tracing::error!(
                server = server,
                tool = tool,
                error = %msg,
                "WASM trap during call_tool: tool not found"
            );
            return Err(McpError::ProtocolError(msg));
        }

        // Agent fuel pre-check: a budget seeded at 0 means **exhausted**
        // (matching the historical `simulacra_wasm::WasmTool` semantics).
        // Short-circuit BEFORE any wasmtime work so the instantiation
        // recorder stays at zero.
        if let Some(ref counter) = self.agent_fuel_remaining
            && counter.load(Ordering::SeqCst) == 0
        {
            return Err(McpError::ProtocolError(
                "execution failed: agent fuel budget exhausted".to_string(),
            ));
        }

        if let Some(ref recorder) = self.instantiation_recorder {
            recorder.fetch_add(1, Ordering::SeqCst);
        }

        let engine = module.engine.clone();
        let component = module.component.clone();
        let allowlist = module.allowlist.clone();
        let hooks = module.hooks.clone();
        let journal = module.journal.clone();
        // Per-call agent_id wins when non-empty so a shared `McpManager`
        // (e.g. simulacra-server) can attribute each fetch to the agent
        // that triggered it. An empty per-call agent_id falls back to
        // the module's bake-in default (CLI back-compat).
        let agent_id = if agent_id.0.is_empty() {
            module.agent_id.clone()
        } else {
            agent_id.clone()
        };
        let http_client = module.http_client.clone();
        // Per-call fuel ceiling = min(server_fuel, agent_remaining_fuel)
        // per spec § Tool dispatch. `0` on either side means "unlimited
        // from that side", so the cap collapses to whichever is finite.
        // Both unlimited stays unlimited (fuel_limit = 0 → store gets
        // u64::MAX in `build_wasm_mcp_store`).
        let server_fuel = module.fuel_limit;
        let agent_remaining = self
            .agent_fuel_remaining
            .as_ref()
            .map(|c| c.load(Ordering::SeqCst));
        let fuel_limit = match (server_fuel, agent_remaining) {
            (0, None) => 0,
            (0, Some(0)) => unreachable!("agent fuel == 0 short-circuited above"),
            (0, Some(remaining)) => remaining,
            (server, None) => server,
            (server, Some(0)) => {
                let _ = server;
                unreachable!("agent fuel == 0 short-circuited above")
            }
            (server, Some(remaining)) => server.min(remaining),
        };
        let server_name = server.to_string();
        let tool_name = tool.to_string();
        let args_json = serde_json::to_string(input).map_err(|e| {
            McpError::ProtocolError(format!("execution failed: invalid arguments: {e}"))
        })?;

        // Capture the current runtime handle so the sync host import
        // (`simulacra:mcp/http.fetch`) can bridge into async fetch from
        // inside spawn_blocking.
        let runtime_handle = tokio::runtime::Handle::current();

        // The blocking closure returns the raw call outcome paired with
        // the post-call fuel consumption. The outcome carries the
        // module's own `tool-error` payload (`Ok` | `Err(ToolError)`)
        // OR a wasmtime trap that we pre-classify as `Err(McpError)`
        // here so the caller sees recognizable messages (notably
        // "fuel exhausted" for `Trap::OutOfFuel`).
        let blocking_handle = runtime_handle.clone();
        type ToolCallResult =
            Result<Result<String, wit_server::simulacra::mcp::types::ToolError>, McpError>;
        let blocking_result =
            tokio::task::spawn_blocking(move || -> Result<(ToolCallResult, u64), McpError> {
                let mut store = build_wasm_mcp_store(
                    &engine,
                    fuel_limit,
                    &server_name,
                    allowlist,
                    hooks,
                    journal,
                    agent_id,
                    http_client,
                    blocking_handle,
                )?;
                let linker = build_wasm_mcp_linker(&engine)?;
                let server = wit_server::Server::instantiate(&mut store, &component, &linker)
                    .map_err(|e| {
                        McpError::ConnectionFailed(format!("wasm instantiation failed: {e}"))
                    })?;
                let call_result = server.call_call_tool(&mut store, &tool_name, &args_json);
                // On `get_fuel` error (engine misconfigured / fuel
                // disabled), fall back to "consumed = 0" rather than
                // "consumed = fuel_limit" so we don't double-charge an
                // agent for a runtime bug. `unwrap_or(0)` here means
                // "unknown consumption → don't deduct"; the engine
                // bug surfaces elsewhere.
                //
                // When `fuel_limit == 0` (unlimited per spec), the store
                // was seeded with `u64::MAX` in `build_wasm_mcp_store` —
                // residual subtraction still yields actual consumption,
                // so the agent's `ResourceBudget` (when present) and the
                // `simulacra.wasm.fuel_consumed` span field reflect real
                // usage even for uncapped modules.
                let fuel_remaining = store.get_fuel().unwrap_or(0);
                let initial_fuel = if fuel_limit == 0 {
                    u64::MAX
                } else {
                    fuel_limit
                };
                let consumed = initial_fuel.saturating_sub(fuel_remaining);
                let outcome: ToolCallResult = match call_result {
                    Ok(inner) => Ok(inner),
                    Err(e) => {
                        // Out-of-fuel traps must surface a recognizable
                        // "fuel exhausted" message so reconnect/retry
                        // plumbing can distinguish budget exhaustion
                        // from other execution failures.
                        let msg = if e
                            .downcast_ref::<wasmtime::Trap>()
                            .is_some_and(|t| matches!(t, wasmtime::Trap::OutOfFuel))
                        {
                            "execution failed: fuel exhausted".to_string()
                        } else {
                            format!("execution failed: {e}")
                        };
                        Err(McpError::ProtocolError(msg))
                    }
                };
                Ok((outcome, consumed))
            })
            .await
            .map_err(|e| McpError::ProtocolError(format!("execution failed: {e}")));

        let (call_result, consumed) = blocking_result??;

        // Decrement the agent-level fuel budget by whatever this call
        // consumed (success and trap paths alike) so a long-running
        // agent monotonically draws down its budget.
        if let Some(ref counter) = self.agent_fuel_remaining {
            let prev = counter.load(Ordering::SeqCst);
            let next = prev.saturating_sub(consumed);
            counter.store(next, Ordering::SeqCst);
        }

        // S041 §Observability: surface the per-call fuel accounting on
        // the span. `simulacra.wasm.fuel_consumed` is computed from the
        // store's residual fuel (initial budget minus what's left after
        // the call returns).
        tool_call_span.record("simulacra.wasm.fuel_consumed", consumed);

        // S041 §Observability: record `simulacra.wasm.fuel_consumed` via
        // the OTel meter directly (the tracing-field histogram
        // convention isn't picked up by every downstream bridge —
        // notably the local Aniani instance — so we use the
        // explicit `Histogram::record` path that already works for
        // `simulacra.mcp.tool.duration`).
        McpMeters::get().wasm_fuel_consumed.record(
            consumed,
            &[
                KeyValue::new("module", server.to_owned()),
                KeyValue::new("tool", tool.to_owned()),
            ],
        );
        // Mirror the metric as a structured log so log-only consumers
        // can still see per-call fuel consumption.
        tracing::info!(
            module = server,
            tool = tool,
            value = consumed,
            "WASM MCP fuel consumed"
        );

        match call_result {
            Ok(Ok(s)) => serde_json::from_str(&s).map_err(|e| {
                McpError::ProtocolError(format!(
                    "execution failed: tool returned non-JSON output: {e}"
                ))
            }),
            Ok(Err(wit_server::simulacra::mcp::types::ToolError::InvalidArguments(msg))) => Err(
                McpError::ProtocolError(format!("execution failed: invalid arguments: {msg}")),
            ),
            Ok(Err(wit_server::simulacra::mcp::types::ToolError::ExecutionFailed(msg))) => {
                // Module-reported execution failure (e.g. invalid input,
                // upstream error). NOT a wasmtime trap — the module ran
                // to completion and signalled failure via its own error
                // channel. Spec § Observability reserves `tracing::error!`
                // for WASM traps; module-level failures are a `warn!`.
                tracing::warn!(
                    server = server,
                    tool = tool,
                    error = %msg,
                    "WASM MCP tool reported execution failure"
                );
                Err(McpError::ProtocolError(format!("execution failed: {msg}")))
            }
            Err(err) => {
                // Real wasmtime trap (out-of-fuel, divide-by-zero, host
                // import bridge failure, etc.) — keep `tracing::error!`
                // per spec line 414.
                tracing::error!(
                    server = server,
                    tool = tool,
                    error = %err,
                    "WASM trap during call_tool"
                );
                Err(err)
            }
        }
    }
}
