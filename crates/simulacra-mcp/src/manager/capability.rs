use crate::domain::capability::glob_match;
use crate::error::McpError;
use simulacra_types::CapabilityToken;

use super::McpManager;

impl McpManager {
    /// Verify the capability token grants access to the requested MCP tool.
    ///
    /// MCP tool capabilities use the fully-qualified `mcp:{server}:{tool}`
    /// namespace so that grants are scoped to a specific server. Bare tool
    /// names in `mcp_tools` do NOT authorize tools across every server —
    /// every pattern MUST be in the `mcp:{server}:{tool}` form (with glob
    /// wildcards, e.g. `mcp:github:*` or `mcp:*:*`).
    ///
    /// Patterns that do not start with `mcp:` are ignored for MCP dispatch
    /// and treated as non-matches.
    pub(crate) fn check_capability(
        &self,
        server: &str,
        tool: &str,
        capability: &CapabilityToken,
    ) -> Result<(), McpError> {
        let qualified = format!("mcp:{server}:{tool}");
        if !capability
            .mcp_tools
            .iter()
            .any(|pattern| pattern.starts_with("mcp:") && glob_match(pattern, &qualified))
        {
            return Err(McpError::CapabilityDenied(format!(
                "tool {tool} on server {server} not in granted mcp_tools \
                 (patterns must be in the form mcp:{{server}}:{{tool}})"
            )));
        }
        Ok(())
    }
}
