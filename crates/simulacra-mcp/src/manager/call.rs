use opentelemetry::KeyValue;
use simulacra_types::{AgentId, CapabilityToken};

use crate::error::McpError;
use crate::observability::McpMeters;

use super::McpManager;

impl McpManager {
    /// Call a tool on the named MCP server.
    ///
    /// The capability proxy layer checks the `CapabilityToken` before
    /// dispatching the call to ensure `mcp_tools` contains the requested tool.
    ///
    /// Before dispatching, the call appends a Journal entry of kind ToolCall
    /// so the conversation replay log captures every MCP tool invocation
    /// (journal before side effect — the Golden Rule).
    ///
    /// **Agent attribution.** This signature does not carry an agent ID —
    /// it is preserved for callers that don't need per-agent journal
    /// attribution (single-agent CLI processes). For shared-process
    /// deployments where multiple agents share one `McpManager`
    /// (e.g. `simulacra-server`), use [`call_tool_for_agent`] so each
    /// outbound `simulacra:mcp/http.fetch` journal entry carries the calling
    /// agent's ID.
    ///
    /// [`call_tool_for_agent`]: Self::call_tool_for_agent
    pub async fn call_tool(
        &mut self,
        server: &str,
        tool: &str,
        input: serde_json::Value,
        capability: &CapabilityToken,
    ) -> Result<serde_json::Value, McpError> {
        // Empty AgentId means "let the WASM module's bake-in default
        // win, if any" inside the dispatch chain — preserves the
        // existing CLI behavior where `WasmMcpServerDescriptor.agent_id`
        // is the only source.
        self.call_tool_for_agent(&AgentId(String::new()), server, tool, input, capability)
            .await
    }

    /// Like [`call_tool`] but stamps the per-call `agent_id` onto every
    /// downstream journal entry written by the dispatch path (notably
    /// the WASM transport's `simulacra:mcp/http.fetch` audit entries).
    ///
    /// Use this in shared-process deployments (`simulacra-server`) where one
    /// `McpManager` instance is reused across many concurrent agents and
    /// the audit trail needs to attribute each outbound HTTP call to the
    /// agent that made it. A non-empty `agent_id` always overrides any
    /// `WasmMcpModule::with_agent_id` default; an empty `agent_id` falls
    /// back to the module's bake-in (preserving CLI semantics).
    ///
    /// [`call_tool`]: Self::call_tool
    pub async fn call_tool_for_agent(
        &mut self,
        agent_id: &AgentId,
        server: &str,
        tool: &str,
        input: serde_json::Value,
        capability: &CapabilityToken,
    ) -> Result<serde_json::Value, McpError> {
        self.check_capability(server, tool, capability)?;

        // Ensure the server has completed its MCP handshake before dispatching.
        self.ensure_server_connected(server).await;
        if !self.connection_handshake_done(server) {
            return Err(Self::handshake_failed_error(server));
        }

        let source = format!("mcp:{server}");
        let argument_length = input.to_string().len();
        let safe_input = serde_json::json!({"argument_length": argument_length});

        let span = tracing::info_span!(
            "execute_tool",
            gen_ai.operation.name = "execute_tool",
            simulacra.tool.name = tool,
            simulacra.tool.source = %source,
        );

        // Log inside a synchronous span guard that is dropped before awaits.
        {
            let _guard = span.enter();

            tracing::info!(
                counter.simulacra.mcp.calls = 1,
                server = server,
                tool = tool,
                "MCP tool call"
            );

            tracing::info!(
                event = "gen_ai.tool.message",
                simulacra.tool.source = %source,
                server = %server,
                tool = %tool,
                input = %safe_input,
                gen_ai.tool.argument_length = argument_length,
                "MCP tool input metadata"
            );
        }

        // Journal before side effect (Golden Rule).
        // If the journal append fails, abort — DO NOT execute the side effect.
        self.append_journal_tool_call(
            tool,
            &serde_json::json!({
                "server": server,
                "tool": tool,
                "argument_length": argument_length,
            }),
        )?;

        let call_start = std::time::Instant::now();
        let result = self
            .dispatch_with_reconnect(agent_id, server, tool, &input)
            .await;

        // S010: Record OTel meter observations for MCP tool call
        let meters = McpMeters::get();
        let attrs = &[
            KeyValue::new("server", server.to_owned()),
            KeyValue::new("tool", tool.to_owned()),
        ];
        meters
            .tool_duration
            .record(call_start.elapsed().as_secs_f64() * 1000.0, attrs);
        meters.calls.add(1, attrs);
        if result.is_err() {
            meters.tool_errors.add(1, attrs);
        }

        let output = result?;

        {
            let _guard = span.enter();
            // Do NOT log full output content — it may contain sensitive data
            // returned from the MCP server (secrets, tokens, PII). Emit the
            // length only, matching the `gen_ai.tool.result_length` pattern
            // used by simulacra-tool.
            let output_length = output.to_string().len();
            tracing::info!(
                event = "gen_ai.tool.message",
                simulacra.tool.source = %source,
                gen_ai.tool.result_length = output_length,
                "MCP tool output"
            );
        }

        Ok(output)
    }
}
