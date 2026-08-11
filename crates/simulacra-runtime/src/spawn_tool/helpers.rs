use super::*;

pub(super) fn normalize_spawn_path_scope(path: &str) -> PathPattern {
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
#[cfg(test)]
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

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnBeforeContext {
    placement: String,
    backend: String,
    instructions: Option<String>,
    task: String,
    budget: SpawnHookBudget,
    capabilities: SpawnHookCapabilities,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnHookCapabilities {
    network: Vec<NetworkPermission>,
    mcp_tools: Vec<String>,
    shell: bool,
    javascript: bool,
    python: bool,
    paths_write: Vec<PathPattern>,
    paths_read: Vec<PathPattern>,
    spawn_placements: Vec<String>,
    skill_patterns: Vec<String>,
    memory: SpawnHookMemoryCapability,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnHookMemoryCapability {
    enabled: bool,
    search_scopes: Vec<simulacra_types::MemoryPath>,
    write_scopes: Vec<simulacra_types::MemoryPath>,
}

impl From<CapabilityToken> for SpawnHookCapabilities {
    fn from(capability: CapabilityToken) -> Self {
        Self {
            network: capability.network,
            mcp_tools: capability.mcp_tools,
            shell: capability.shell,
            javascript: capability.javascript,
            python: capability.python,
            paths_write: capability.paths_write,
            paths_read: capability.paths_read,
            spawn_placements: capability.spawn_placements,
            skill_patterns: capability.skill_patterns,
            memory: SpawnHookMemoryCapability {
                enabled: capability.memory.enabled,
                search_scopes: capability.memory.search_scopes,
                write_scopes: capability.memory.write_scopes,
            },
        }
    }
}

impl SpawnHookCapabilities {
    fn into_token(self) -> CapabilityToken {
        CapabilityToken {
            network: self.network,
            mcp_tools: self.mcp_tools,
            shell: self.shell,
            javascript: self.javascript,
            python: self.python,
            paths_write: self.paths_write,
            paths_read: self.paths_read,
            spawn_placements: self.spawn_placements,
            skill_patterns: self.skill_patterns,
            memory: simulacra_types::MemoryCapability {
                enabled: self.memory.enabled,
                search_scopes: self.memory.search_scopes,
                write_scopes: self.memory.write_scopes,
            },
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct SpawnHookBudget {
    max_tokens: u64,
    max_turns: u32,
    max_cost: Decimal,
    max_sub_agents: u32,
}

impl SpawnHookBudget {
    fn from_resource(budget: &ResourceBudget) -> Self {
        Self {
            max_tokens: budget.max_tokens,
            max_turns: budget.max_turns,
            max_cost: budget.max_cost,
            max_sub_agents: budget.max_sub_agents,
        }
    }

    fn apply_to(self, budget: &mut ResourceBudget) {
        budget.max_tokens = self.max_tokens;
        budget.max_turns = self.max_turns;
        budget.max_cost = self.max_cost;
        budget.max_sub_agents = self.max_sub_agents;
    }
}

fn append_spawn_hook_entry(
    journal: &Arc<dyn JournalStorage>,
    parent_id: &AgentId,
    entry: simulacra_types::JournalEntryKind,
) -> Result<(), RuntimeError> {
    journal
        .append(simulacra_types::JournalEntry {
            schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
            agent_id: parent_id.clone(),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
            entry,
        })
        .map_err(|source| RuntimeError::JournalAppendFailed {
            entry_kind: "spawn hook",
            source,
        })
}

fn authoritative_spawn_hook_kill(
    journal: &Arc<dyn JournalStorage>,
    parent_id: &AgentId,
    phase: &'static str,
    hook: String,
    reason: String,
) -> RuntimeError {
    crate::policy_kill::signal(parent_id, journal, hook.clone(), reason.clone());
    if let Err(error) = append_spawn_hook_entry(
        journal,
        parent_id,
        simulacra_types::JournalEntryKind::HookKill {
            hook_name: hook.clone(),
            operation: "spawn".into(),
            reason: reason.clone(),
        },
    ) {
        tracing::error!(
            hook = hook.as_str(),
            phase,
            parent_id = parent_id.0.as_str(),
            error = %error,
            "failed to append authoritative spawn HookKill audit"
        );
    }
    RuntimeError::HookKill { hook, reason }
}

fn hook_limit_is_attenuation<T>(candidate: T, original: T) -> bool
where
    T: Copy + Default + PartialEq + PartialOrd,
{
    original == T::default() || (candidate != T::default() && candidate <= original)
}

pub(super) fn run_spawn_before_hook(
    pipeline: Option<&Arc<simulacra_hooks::pipeline::HookPipeline>>,
    journal: &Arc<dyn JournalStorage>,
    config: &mut SpawnConfig,
    backend: AgentBackend,
    effective_capability: CapabilityToken,
) -> Result<(), RuntimeError> {
    let Some(pipeline) = pipeline else {
        config.capability = Some(effective_capability);
        return Ok(());
    };

    let backend = match backend {
        AgentBackend::Native => "native",
        AgentBackend::Acp => "acp",
    };
    let original_budget = SpawnHookBudget::from_resource(&config.budget);
    let initial = SpawnBeforeContext {
        placement: config.placement.clone(),
        backend: backend.to_string(),
        instructions: config.instructions.clone(),
        task: config.task.clone(),
        budget: SpawnHookBudget::from_resource(&config.budget),
        capabilities: effective_capability.clone().into(),
    };
    let before_ctx = serde_json::to_string(&initial)
        .map_err(|error| RuntimeError::HookError(error.to_string()))?;

    match pipeline.run_before_attributed(simulacra_hooks::verdict::Operation::Spawn, &before_ctx) {
        Ok((simulacra_hooks::Verdict::Continue(_), modified, _)) => {
            let modified: SpawnBeforeContext =
                serde_json::from_str(&modified).map_err(|error| {
                    RuntimeError::HookError(format!("invalid spawn before-hook context: {error}"))
                })?;
            for (field, changed) in [
                ("placement", modified.placement != initial.placement),
                ("backend", modified.backend != initial.backend),
                (
                    "instructions",
                    modified.instructions != initial.instructions,
                ),
                ("task", modified.task != initial.task),
            ] {
                if changed {
                    return Err(RuntimeError::HookError(format!(
                        "spawn before-hook cannot modify {field}"
                    )));
                }
            }
            if !hook_limit_is_attenuation(modified.budget.max_tokens, original_budget.max_tokens)
                || !hook_limit_is_attenuation(modified.budget.max_turns, original_budget.max_turns)
                || !hook_limit_is_attenuation(modified.budget.max_cost, original_budget.max_cost)
                || !hook_limit_is_attenuation(
                    modified.budget.max_sub_agents,
                    original_budget.max_sub_agents,
                )
            {
                return Err(RuntimeError::HookError(
                    "spawn before-hook budget must attenuate the requested budget".into(),
                ));
            }
            let modified_capability = modified.capabilities.into_token();
            if !modified_capability.is_subset_of(&effective_capability) {
                return Err(RuntimeError::HookError(
                    "spawn before-hook capabilities must attenuate the effective capabilities"
                        .into(),
                ));
            }
            modified.budget.apply_to(&mut config.budget);
            config.capability = Some(modified_capability);
            Ok(())
        }
        Ok((simulacra_hooks::Verdict::Deny(reason), _, hook_name)) => {
            let hook_name = hook_name.ok_or_else(|| {
                RuntimeError::HookError("spawn before-hook denied without hook attribution".into())
            })?;
            append_spawn_hook_entry(
                journal,
                &config.parent_id,
                simulacra_types::JournalEntryKind::HookDenial {
                    hook_name,
                    operation: "spawn".into(),
                    reason: reason.clone(),
                },
            )?;
            Err(RuntimeError::HookDenial(reason))
        }
        Ok((simulacra_hooks::Verdict::Kill(reason), _, Some(hook))) => Err(
            authoritative_spawn_hook_kill(journal, &config.parent_id, "before", hook, reason),
        ),
        Ok((simulacra_hooks::Verdict::Kill(_), _, None)) => Err(RuntimeError::HookError(
            "spawn before-hook killed without hook attribution".into(),
        )),
        Err(simulacra_hooks::HookError::Killed { hook, reason }) => Err(
            authoritative_spawn_hook_kill(journal, &config.parent_id, "before", hook, reason),
        ),
        Err(e) => Err(RuntimeError::HookError(e.to_string())),
    }
}

pub(super) fn run_spawn_after_hook(
    pipeline: Option<&Arc<simulacra_hooks::pipeline::HookPipeline>>,
    journal: &Arc<dyn JournalStorage>,
    config: &SpawnConfig,
    backend: AgentBackend,
    result: &Result<AgentLoopOutput, RuntimeError>,
) -> Result<(), RuntimeError> {
    let Some(pipeline) = pipeline else {
        return Ok(());
    };

    let tokens_used = result.as_ref().map(|o| o.token_usage.total()).unwrap_or(0);
    let terminal_status = crate::supervisor::status_from_spawn_result(result);
    let after_ctx = serde_json::json!({
        "child_id": config.agent_id,
        "placement": config.placement,
        "backend": match backend { AgentBackend::Native => "native", AgentBackend::Acp => "acp" },
        "result": terminal_status,
        "tokens_used": tokens_used,
    })
    .to_string();
    match pipeline.run_after(simulacra_hooks::verdict::Operation::Spawn, &after_ctx) {
        Ok(_) => Ok(()),
        Err(simulacra_hooks::HookError::Killed { hook, reason }) => Err(
            authoritative_spawn_hook_kill(journal, &config.parent_id, "after", hook, reason),
        ),
        Err(error) => Err(RuntimeError::HookError(error.to_string())),
    }
}
