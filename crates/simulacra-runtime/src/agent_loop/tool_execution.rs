use super::*;

/// Execute a tool call live (not from replay).
pub(super) async fn execute_tool_live(
    tools: &ToolRegistry,
    tc: &simulacra_types::ToolCallMessage,
    capability: &CapabilityToken,
    agent_name: &str,
) -> (String, bool) {
    let result = tools.call(&tc.name, tc.arguments.clone(), capability).await;
    match result {
        Ok(val) => {
            // If the tool returned JSON with an "error" field, treat it as
            // an error so the agent loop surfaces it with the ERROR: prefix.
            let is_error = val.is_object() && val.get("error").is_some();
            (val.to_string(), is_error)
        }
        Err(ref e @ simulacra_types::ToolError::CapabilityDenied(ref denied)) => {
            tracing::warn!(
                simulacra.capability.operation = %denied.operation,
                simulacra.capability.reason = %denied.reason,
                simulacra.capability.denials = "1",
                gen_ai.agent.name = agent_name,
                "capability denied"
            );
            (e.to_string(), true)
        }
        Err(e) => (e.to_string(), true),
    }
}
