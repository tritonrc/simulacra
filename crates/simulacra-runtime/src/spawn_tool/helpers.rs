use super::*;

/// Parse an optional `capabilities` JSON object into a `CapabilityToken`.
///
/// **Note on memory:** the `capabilities` JSON object does not currently
/// expose a `memory` field — there is no way for an LLM to ask for or
/// narrow memory grants at spawn time. Per W1 from the S037 capability
/// sandbox review, the factory call sites that intersect this override
/// against the parent's capabilities MUST inherit `parent.memory` rather
/// than using the parsed override's default-empty `MemoryCapability`,
/// otherwise children would silently lose memory access whenever a
/// capability override is supplied. See `inherit_memory_when_override_unset`.
pub(super) fn parse_capability_override(value: &serde_json::Value) -> CapabilityToken {
    let network = value
        .get("network")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| NetworkPermission(s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let mcp_tools = value
        .get("mcp_tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let shell = value
        .get("shell")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let javascript = value
        .get("javascript")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let python = value
        .get("python")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let paths_write = value
        .get("paths_write")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| PathPattern(s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let paths_read = value
        .get("paths_read")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| PathPattern(s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let spawn_types = value
        .get("spawn_types")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    CapabilityToken {
        network,
        mcp_tools,
        shell,
        javascript,
        python,
        paths_write,
        paths_read,
        spawn_types,
        skill_patterns: vec![],
        // Memory is intentionally left at default here. The factory call
        // sites use `inherit_memory_when_override_unset` below to copy
        // parent.memory into the override before intersecting, so an
        // unmentioned memory grant inherits rather than being stripped.
        memory: simulacra_types::MemoryCapability::default(),
    }
}

/// W1 fix: an override parsed from spawn_agent JSON has no way to specify
/// `memory`, so the parsed token always carries `MemoryCapability::default()`
/// (disabled, empty scopes). Intersecting that against the parent would
/// silently strip the parent's memory grants from the child, which is the
/// opposite of what "the LLM did not mention memory" should mean.
///
/// This helper detects "the override's memory is the unset default" and, in
/// that case, copies the parent's memory into the override before intersect.
/// When the JSON capabilities object grows a `memory` field in the future,
/// this helper should be replaced with explicit tracking of whether the
/// override authored memory.
pub(super) fn inherit_memory_when_override_unset(
    override_cap: &CapabilityToken,
    parent: &CapabilityToken,
) -> CapabilityToken {
    let mut out = override_cap.clone();
    if out.memory == simulacra_types::MemoryCapability::default() {
        out.memory = parent.memory.clone();
    }
    out
}

// ---------------------------------------------------------------------------
// resolve_tier_model
// ---------------------------------------------------------------------------

/// Resolve a model name from a tier name, falling back to the parent's model.
pub(super) fn resolve_tier_model(
    tier: Option<&str>,
    tiers_config: &TierMap,
    parent_model: &str,
) -> String {
    match tier {
        Some(t) => tiers_config
            .get(t)
            .cloned()
            .unwrap_or_else(|| parent_model.to_string()),
        None => parent_model.to_string(),
    }
}

pub(super) fn parent_tier_name(tiers_config: &TierMap, parent_model: &str) -> String {
    tiers_config
        .iter()
        .find_map(|(tier, model)| {
            if model == parent_model {
                Some(tier.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "balanced".to_string())
}
