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
                .filter_map(|v| v.as_str().map(normalize_spawn_path_scope))
                .collect()
        })
        .unwrap_or_default();
    let paths_read = value
        .get("paths_read")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(normalize_spawn_path_scope))
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

fn normalize_spawn_path_scope(path: &str) -> PathPattern {
    let had_trailing_slash = path != "/" && path.ends_with('/');
    let trimmed = if path == "/" {
        path
    } else {
        path.trim_end_matches('/')
    };
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return PathPattern(path.to_string());
    }

    if let Some(prefix) = trimmed.strip_suffix("/**") {
        let normalized = normalize_absolute_spawn_path(prefix);
        return if normalized == "/" {
            PathPattern("/**".to_string())
        } else {
            PathPattern(format!("{normalized}/**"))
        };
    }

    if path.contains('*') {
        return PathPattern(path.to_string());
    }

    let normalized = normalize_absolute_spawn_path(trimmed);
    if normalized == "/" {
        return PathPattern("/**".to_string());
    }

    let leaf = normalized.rsplit('/').next().unwrap_or(&normalized);
    if had_trailing_slash || is_common_workspace_directory(leaf) {
        PathPattern(format!("{normalized}/**"))
    } else {
        PathPattern(normalized)
    }
}

fn normalize_absolute_spawn_path(path: &str) -> String {
    let mut components = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            segment => components.push(segment),
        }
    }
    format!("/{}", components.join("/"))
}

fn is_common_workspace_directory(leaf: &str) -> bool {
    matches!(
        leaf,
        ".github"
            | "bench"
            | "benches"
            | "crate"
            | "crates"
            | "demo"
            | "demos"
            | "doc"
            | "docs"
            | "example"
            | "examples"
            | "fixture"
            | "fixtures"
            | "rule"
            | "rules"
            | "script"
            | "scripts"
            | "spec"
            | "specs"
            | "src"
            | "test"
            | "tests"
            | "workspace"
    )
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

pub(super) fn run_spawn_before_hook(
    pipeline: Option<&Arc<simulacra_hooks::pipeline::HookPipeline>>,
    agent_type: &str,
    system_prompt: &str,
    budget: &ResourceBudget,
) -> Result<(), RuntimeError> {
    let Some(pipeline) = pipeline else {
        return Ok(());
    };

    let before_ctx = serde_json::json!({
        "agent_type": agent_type,
        "system_prompt": system_prompt,
        "budget": {
            "max_tokens": budget.max_tokens,
            "max_turns": budget.max_turns,
        },
    })
    .to_string();

    match pipeline.run_before(simulacra_hooks::verdict::Operation::Spawn, &before_ctx) {
        Ok((simulacra_hooks::Verdict::Continue(_), _)) => Ok(()),
        Ok((simulacra_hooks::Verdict::Deny(reason), _)) => Err(RuntimeError::HookDenial(reason)),
        Ok((simulacra_hooks::Verdict::Kill(_), _)) => {
            unreachable!("Kill is returned as Err from run_before")
        }
        Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
            Err(RuntimeError::HookKill { hook, reason })
        }
        Err(e) => Err(RuntimeError::HookError(e.to_string())),
    }
}

pub(super) fn run_spawn_after_hook(
    pipeline: Option<&Arc<simulacra_hooks::pipeline::HookPipeline>>,
    agent_type: &str,
    result: &Result<AgentLoopOutput, RuntimeError>,
) {
    let Some(pipeline) = pipeline else {
        return;
    };

    let tokens_used = result.as_ref().map(|o| o.token_usage.total()).unwrap_or(0);
    let after_ctx = serde_json::json!({
        "agent_type": agent_type,
        "result": result.as_ref().map(|o| format!("{:?}", o.exit_reason)).unwrap_or_else(|e| format!("{e}")),
        "tokens_used": tokens_used,
    })
    .to_string();
    let _ = pipeline.run_after(simulacra_hooks::verdict::Operation::Spawn, &after_ctx);
}
