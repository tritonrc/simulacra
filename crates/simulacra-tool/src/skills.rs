//! Skill discovery and the model-visible `Skill` tool.

use std::collections::HashMap;
#[cfg(feature = "sandbox")]
use std::future::Future;
#[cfg(feature = "sandbox")]
use std::pin::Pin;
use std::sync::Arc;

#[cfg(feature = "sandbox")]
use serde_json::{Value, json};
#[cfg(feature = "sandbox")]
use simulacra_sandbox::AgentCell;
use simulacra_types::{CapabilityToken, VirtualFs};
#[cfg(feature = "sandbox")]
use simulacra_types::{Tool, ToolDefinition, ToolError};

use crate::SkillError;
#[cfg(feature = "sandbox")]
use crate::sandbox_tools::{map_sandbox_error, require_str};

// ---------------------------------------------------------------------------
// SkillTool (S017)
// ---------------------------------------------------------------------------

/// Metadata for a discovered skill parsed from SKILL.md frontmatter.
///
/// The frontmatter `name` is the canonical identifier used by both
/// `Skill(command=...)` and `/skill-name` invocation. Directory names are not
/// the source of truth. A directory that contains `SKILL.md` but lacks valid
/// frontmatter (with a `name` and `description` field) is not a valid skill.
///
/// The skill registry is keyed by frontmatter `name`, not directory name.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    /// Canonical skill identifier from frontmatter.name.
    pub name: String,
    /// Compact summary used for discovery and model selection. This description
    /// is exposed in the model-visible skill catalog inside the Skill tool
    /// definition. The tool definition includes only skill `name + description`,
    /// not full `SKILL.md` bodies. Full `SKILL.md` bodies are excluded from the
    /// initial tool definition and from the system prompt.
    pub description: String,
    /// Canonical VFS path to this skill's SKILL.md.
    /// The registry stores a canonical VFS path to each discovered skill's SKILL.md.
    /// Project skills resolve from canonical VFS paths under `/skills`.
    pub vfs_path: String,
    /// When true, the skill is excluded from the model-visible Skill tool catalog.
    /// A model-triggered call to a model-disabled skill returns an error tool result
    /// even if the model guessed the name.
    pub disable_model_invocation: bool,
    /// When false, the skill is not available through `/skill-name` invocation.
    /// A skill with `user_invocable: false` may still appear in the model-visible
    /// Skill tool description if model invocation is enabled (i.e. when
    /// `disable_model_invocation` is `false`).
    pub user_invocable: bool,
    /// Tool names pre-approved during interactive approval for the current turn.
    /// `allowed_tools` only affects the interactive approval layer for the current
    /// turn. It does NOT alter `ToolRegistry`, does NOT bypass capabilities, and
    /// does NOT bypass budgets. A skill never grants capabilities the agent does
    /// not already have.
    pub allowed_tools: Vec<String>,
    /// The parsed markdown body of SKILL.md (everything after YAML frontmatter).
    /// Populated at discovery time by `parse_skill_frontmatter` so that
    /// interactive `/skill-name` invocation can inject the body without VFS access.
    pub body: Option<String>,
}

/// S017 — Skills tool.
///
/// Simulacra registers exactly one built-in tool named `Skill` when the current
/// agent has at least one model-visible skill that survives capability filtering
/// and metadata-budget truncation. Simulacra does NOT register one tool per skill.
/// Skills are not first-class tools.
///
/// The `Skill` tool definition contains only compact metadata for
/// model-invocable skills: `name + description`. Full `SKILL.md` bodies are
/// excluded from the initial tool definition and from the system prompt.
///
/// When the provider emits `Skill { "command": "<name>" }`, the tool reads the
/// corresponding SKILL.md through `AgentCell::read_file`, strips YAML
/// frontmatter, and returns only the markdown body as the tool result. The
/// returned skill body becomes part of the conversation only through that tool
/// result. It is not retroactively added to the system prompt.
///
/// `Skill` never auto-loads sibling resources, never executes scripts, and
/// never expands referenced files inline. Supporting materials remain on disk
/// until explicitly accessed with existing tools (file_read, list_dir,
/// shell_exec, js_exec). A supporting skill document requires an explicit
/// `file_read` or `list_dir` call. A supporting skill script requires an
/// explicit `shell_exec` or `js_exec` call.
///
/// Multiple skills may be loaded in the same turn. Each `Skill` call resolves
/// and returns one skill body independently.
///
/// Skills remain prompt text only. Any side effect suggested by a skill body
/// must still execute through existing tools and `AgentCell`. The `Skill` tool
/// is prompt injection only — it is not a new execution surface.
///
/// If the named skill is unknown skill, not in the agent type's configured
/// skill list, or denied by the capability token, `Skill` returns an error
/// tool result. The agent sees the denial reason.
///
/// If the named skill has `disable_model_invocation: true`, a model-triggered
/// call returns an error tool result even if the model guessed the name.
#[cfg(feature = "sandbox")]
pub struct SkillTool {
    cell: Arc<AgentCell>,
    /// The effective skill catalog is the intersection of discovered skills,
    /// `agent_type.skills`, and `skill:<name>` capability patterns.
    /// Capability checks happen at the call site before returning a skill body.
    catalog: Vec<SkillMeta>,
}

#[cfg(feature = "sandbox")]
impl SkillTool {
    pub fn new(cell: Arc<AgentCell>, catalog: Vec<SkillMeta>) -> Self {
        Self { cell, catalog }
    }

    /// Build the model-visible skill catalog description (name + description
    /// pairs) for inclusion in the Skill tool definition. Applies the metadata
    /// budget to limit context consumption.
    ///
    /// Only model-invocable skills count against the metadata budget. A skill
    /// with `disable_model_invocation: true` is excluded from the model-visible
    /// `Skill` tool description even if it is otherwise available to the agent.
    ///
    /// Metadata entries are considered in the order listed by
    /// `agent_type.skills` (the order in which they appear in the catalog).
    ///
    /// If one or more model-invocable skills are omitted due to the metadata
    /// budget, the Skill tool description indicates that the catalog is partial.
    ///
    /// Omitted skills remain resolvable for user-triggered invocation if they
    /// are `user_invocable: true` and otherwise allowed.
    fn build_catalog_description(&self, metadata_budget_chars: usize) -> String {
        let model_visible: Vec<&SkillMeta> = self
            .catalog
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .collect();

        let mut desc = String::from("Available skills:\n");
        let mut included = 0;
        let mut omitted = 0;

        for skill in &model_visible {
            let entry = format!("- {}: {}\n", skill.name, skill.description);
            if desc.len() + entry.len() <= metadata_budget_chars {
                desc.push_str(&entry);
                included += 1;
            } else {
                omitted += 1;
            }
        }

        if omitted > 0 {
            desc.push_str(&format!(
                "\n(catalog is partial — {omitted} additional skill(s) omitted due to metadata budget)\n"
            ));
        }

        if included == 0 && omitted == 0 {
            desc.push_str("(no skills available)\n");
        }

        desc
    }
}

#[cfg(feature = "sandbox")]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        // The Skill tool definition is built from the current agent's effective
        // skill catalog after agent-type config and capability filtering. The
        // definition includes only `name + description`, not the full SKILL.md
        // body. The `"command"` field is required. `additionalProperties` is false.
        //
        // The metadata budget for skill descriptions is derived as a configured
        // percentage of the active model's context window. For now we use a
        // reasonable default of 4096 characters.
        let metadata_budget_chars = 4096;
        let catalog_desc = self.build_catalog_description(metadata_budget_chars);

        ToolDefinition {
            name: "Skill".into(),
            description: format!(
                "Load the body of a registered skill on demand. \
                 Returns the full skill prompt text as a tool result.\n\n{catalog_desc}"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Skill identifier from SKILL.md frontmatter.name"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        let command = match require_str(&args, "command") {
            Ok(c) => c,
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        // Resolve the skill from the effective skill catalog.
        let skill = match self.catalog.iter().find(|s| s.name == command) {
            Some(s) => s.clone(),
            None => {
                return Box::pin(async move {
                    Ok(json!({
                        "is_error": true,
                        "content": format!("unknown skill: {command:?}")
                    }))
                });
            }
        };

        // If the named skill has disable_model_invocation: true, a
        // model-triggered call returns an error tool result even if the model
        // guessed the name.
        if skill.disable_model_invocation {
            return Box::pin(async move {
                Ok(json!({
                    "is_error": true,
                    "content": format!(
                        "skill {command:?} has disable_model_invocation=true and cannot be invoked by the model"
                    )
                }))
            });
        }

        // Capability checks happen at the call site: before returning a
        // skill body, Simulacra verifies that the requested skill is allowed by
        // the current capability token.
        if let Err(denied) = capability.check_skill(&command) {
            return Box::pin(async move {
                Ok(json!({
                    "is_error": true,
                    "content": denied.reason
                }))
            });
        }

        let vfs_path = skill.vfs_path.clone();
        let cell = Arc::clone(&self.cell);

        Box::pin(async move {
            // Load the SKILL.md body via AgentCell::read_file (Golden Rule).
            // The tool reads the corresponding SKILL.md through
            // AgentCell::read_file, strips YAML frontmatter, and returns only
            // the markdown body as the tool result.
            let data = cell.read_file(&vfs_path).map_err(map_sandbox_error)?;

            let content = String::from_utf8_lossy(&data).into_owned();

            // Parse and strip YAML frontmatter, returning only the markdown body.
            let body = strip_yaml_frontmatter(&content);

            // OTel: tool span with gen_ai.tool.name = "Skill" is created by
            // ToolRegistry::call. Skill invocation spans include
            // simulacra.skill.name and simulacra.skill.source ("model" or "user").
            // Skill resolution spans include the canonical VFS path of the
            // loaded SKILL.md (simulacra.vfs.path).
            tracing::info!(
                simulacra.skill.name = %command,
                simulacra.skill.source = "model",
                simulacra.vfs.path = %vfs_path,
                "skill loaded"
            );

            Ok(json!(body))
        })
    }
}

/// Strip YAML frontmatter (delimited by `---`) from a SKILL.md string,
/// returning only the markdown body after the closing `---`.
#[cfg(feature = "sandbox")]
fn strip_yaml_frontmatter(content: &str) -> String {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content.to_string();
    }
    // Find the closing `---` after the opening one.
    if let Some(end) = trimmed[3..].find("\n---") {
        let after_close = &trimmed[3 + end + 4..]; // skip past "\n---"
        after_close.trim_start_matches('\n').to_string()
    } else {
        content.to_string()
    }
}

/// Parse SKILL.md YAML frontmatter into a SkillMeta.
///
/// A valid skill directory requires `SKILL.md` with YAML frontmatter plus a
/// markdown body. The `name` field is the canonical identifier used by both
/// `Skill(command=...)` and `/skill-name`. The `description` field is exposed
/// in the model-visible skill catalog.
///
/// `disable_model_invocation: true` blocks model-triggered invocation.
/// `user_invocable: false` blocks `/skill-name` invocation but the skill may
/// still appear in the model-visible catalog if disable_model_invocation is
/// false. When `user_invocable: false`, `/skill-name` falls through to the
/// unknown command path.
///
/// `allowed_tools` narrows interactive pre-approval only and does NOT widen
/// capability policy.
pub fn parse_skill_frontmatter(content: &str, vfs_path: &str) -> Result<SkillMeta, String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err("SKILL.md must begin with YAML frontmatter (---)".into());
    }
    let end = trimmed[3..]
        .find("\n---")
        .ok_or("SKILL.md frontmatter missing closing ---")?;

    let yaml_str = &trimmed[3..3 + end + 1]; // include trailing newline

    // Parse YAML fields manually (avoid adding a yaml dependency).
    let mut name = None;
    let mut description = None;
    let mut disable_model_invocation = false;
    let mut user_invocable = true;
    let mut allowed_tools = Vec::new();
    let mut in_allowed_tools = false;

    for line in yaml_str.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if in_allowed_tools {
            if let Some(item) = line.strip_prefix("- ") {
                allowed_tools.push(item.trim().to_string());
                continue;
            } else {
                in_allowed_tools = false;
            }
        }

        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "name" => name = Some(val.to_string()),
                "description" => description = Some(val.to_string()),
                "disable_model_invocation" => {
                    disable_model_invocation = val == "true";
                }
                "user_invocable" => {
                    user_invocable = val != "false";
                }
                "allowed_tools" if val.is_empty() => {
                    in_allowed_tools = true;
                }
                _ => {}
            }
        }
    }

    let name = name.ok_or("SKILL.md frontmatter missing required field: name")?;
    let description =
        description.ok_or("SKILL.md frontmatter missing required field: description")?;

    // Validate that there is a non-empty markdown body after the frontmatter.
    let body_start = 3 + end + 4; // skip opening "---", yaml, "\n---"
    let body = trimmed[body_start..].trim();
    if body.is_empty() {
        return Err("SKILL.md requires a markdown body after the YAML frontmatter".into());
    }

    Ok(SkillMeta {
        name,
        description,
        vfs_path: vfs_path.to_string(),
        disable_model_invocation,
        user_invocable,
        allowed_tools,
        body: Some(body.to_string()),
    })
}

// ---------------------------------------------------------------------------
// discover_and_filter_skills
// ---------------------------------------------------------------------------

/// Walk the VFS `/skills` directory, parse `SKILL.md` frontmatter for each
/// subdirectory, and filter the result by the agent type's configured skill
/// list and the capability token's `skill:<name>` patterns.
///
/// User-triggered skill loads are recorded as host-side session events before
/// provider execution so the source of the injected prompt remains attributable.
///
/// Bootstrap discovery emits an INFO-level event with discovered skill count
/// and mounted skill-root count.
pub fn discover_and_filter_skills(
    vfs: &Arc<dyn VirtualFs>,
    agent_skills: &[String],
    capability: &CapabilityToken,
    agent_type_name: &str,
) -> Result<Vec<SkillMeta>, SkillError> {
    // If the agent type has no skills configured, nothing to discover.
    if agent_skills.is_empty() {
        return Ok(Vec::new());
    }

    // Discover skills from /skills/<dir>/SKILL.md in the VFS.
    let mut discovered: HashMap<String, SkillMeta> = HashMap::new();
    let mut invalid_names: Vec<String> = Vec::new();

    if let Ok(entries) = vfs.list_dir("/skills") {
        for dir_name in &entries {
            let skill_path = format!("/skills/{dir_name}/SKILL.md");
            if !vfs.exists(&skill_path) {
                continue;
            }
            match vfs.read(&skill_path) {
                Ok(data) => {
                    let content = String::from_utf8_lossy(&data).into_owned();
                    match parse_skill_frontmatter(&content, &skill_path) {
                        Ok(meta) => {
                            // Duplicate skill names across discovery roots
                            // fail startup instead of shadowing.
                            if discovered.contains_key(&meta.name) {
                                return Err(SkillError::DuplicateSkillName {
                                    name: meta.name.clone(),
                                    first_path: discovered[&meta.name].vfs_path.clone(),
                                    second_path: skill_path,
                                });
                            }
                            discovered.insert(meta.name.clone(), meta);
                        }
                        Err(e) => {
                            // Invalid or missing SKILL.md frontmatter is
                            // skipped with a warning when unreferenced.
                            tracing::warn!(
                                path = %skill_path,
                                error = %e,
                                "skip invalid SKILL.md frontmatter"
                            );
                            invalid_names.push(dir_name.clone());
                        }
                    }
                }
                Err(_) => continue,
            }
        }
    }

    tracing::info!(
        discovered_skill_count = discovered.len(),
        mounted_skill_root_count = 0_usize,
        "skill discovery complete"
    );

    // Filter by agent_type.skills (the allow-list) and build the effective
    // skill catalog.
    let mut catalog = Vec::new();
    for skill_name in agent_skills {
        if let Some(meta) = discovered.get(skill_name) {
            // Capability check: skill:<name> patterns.
            if capability.check_skill(skill_name).is_ok() {
                catalog.push(meta.clone());
            } else {
                // Skill capability denials emit a WARN-level event with the
                // requested skill name and denial reason.
                tracing::warn!(
                    skill_name = %skill_name,
                    denial_reason = "skill not allowed by capability token",
                    "skill capability denied"
                );
            }
        } else {
            // An agent type that references an undiscoverable skill fails
            // startup with an error naming the agent type and missing skill.
            return Err(SkillError::UndiscoverableSkill {
                agent_type: agent_type_name.to_string(),
                skill: skill_name.clone(),
            });
        }
    }

    Ok(catalog)
}
