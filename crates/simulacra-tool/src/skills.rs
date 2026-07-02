//! Skill discovery and the model-visible `Skill` tool.

mod frontmatter;

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
pub use frontmatter::parse_skill_frontmatter;
#[cfg(feature = "sandbox")]
use frontmatter::strip_yaml_frontmatter;

#[cfg(feature = "sandbox")]
const DEFAULT_SKILL_METADATA_BUDGET_CHARS: usize = 8_000;

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
    /// When false, the skill is not surfaced in model-visible skill metadata and
    /// model-triggered `Skill` calls for it are rejected. User-triggered loading
    /// is still controlled by `user_invocable`.
    pub allow_implicit_invocation: bool,
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
/// If the named skill has `disable_model_invocation: true` or
/// `allow_implicit_invocation: false`, a model-triggered call returns an error
/// tool result even if the model guessed the name.
#[cfg(feature = "sandbox")]
pub struct SkillTool {
    cell: Arc<AgentCell>,
    /// The effective skill catalog is the intersection of discovered skills,
    /// `agent_type.skills`, and `skill:<name>` capability patterns.
    /// Capability checks happen at the call site before returning a skill body.
    catalog: Vec<SkillMeta>,
    metadata_budget_chars: usize,
}

#[cfg(feature = "sandbox")]
impl SkillTool {
    pub fn new(cell: Arc<AgentCell>, catalog: Vec<SkillMeta>) -> Self {
        Self {
            cell,
            catalog,
            metadata_budget_chars: DEFAULT_SKILL_METADATA_BUDGET_CHARS,
        }
    }

    pub fn new_with_metadata_budget(
        cell: Arc<AgentCell>,
        catalog: Vec<SkillMeta>,
        metadata_budget_chars: usize,
    ) -> Self {
        Self {
            cell,
            catalog,
            metadata_budget_chars,
        }
    }

    /// Build the model-visible skill catalog description (name + description
    /// pairs) for inclusion in the Skill tool definition. Applies the metadata
    /// budget to limit context consumption.
    ///
    /// Only model-invocable skills count against the metadata budget. A skill
    /// with `disable_model_invocation: true` or `allow_implicit_invocation: false`
    /// is excluded from the model-visible `Skill` tool description even if it is
    /// otherwise available to the agent.
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
            .filter(|s| !s.disable_model_invocation && s.allow_implicit_invocation)
            .collect();

        let mut desc = String::from("Available skills:\n");
        let mut included = 0;
        let mut omitted = 0;

        for skill in &model_visible {
            let normalized_description = skill
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let entry = format!("- {}: {normalized_description}\n", skill.name);
            if desc.len() + entry.len() <= metadata_budget_chars {
                desc.push_str(&entry);
                included += 1;
            } else {
                let prefix = format!("- {}: ", skill.name);
                let suffix = "...\n";
                let available = metadata_budget_chars
                    .saturating_sub(desc.len())
                    .saturating_sub(prefix.len())
                    .saturating_sub(suffix.len());
                if available > 0 {
                    desc.push_str(&prefix);
                    desc.push_str(truncate_to_char_boundary(
                        &normalized_description,
                        available,
                    ));
                    desc.push_str(suffix);
                    included += 1;
                } else {
                    omitted += 1;
                }
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
        // The metadata budget can be provided by the caller when model context
        // is known. Otherwise SkillTool::new uses the default fallback budget.
        let catalog_desc = self.build_catalog_description(self.metadata_budget_chars);

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

        if !skill.allow_implicit_invocation {
            return Box::pin(async move {
                Ok(json!({
                    "is_error": true,
                    "content": format!(
                        "skill {command:?} has allow_implicit_invocation=false and cannot be invoked by the model"
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

// ---------------------------------------------------------------------------
// discover_and_filter_skills
// ---------------------------------------------------------------------------

/// Walk the VFS `/skills` directory tree, parse discovered `SKILL.md`
/// frontmatter, and filter the result by the agent type's configured skill
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

    // Discover skills below the mounted VFS /skills root. This recurses
    // downward to support grouped skill layouts, but never walks upward or
    // searches for other skills directories elsewhere in the VFS.
    let mut discovered: HashMap<String, SkillMeta> = HashMap::new();

    for skill_path in discover_skill_paths(vfs.as_ref(), "/skills") {
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
                        // Invalid or missing SKILL.md frontmatter is skipped
                        // with a warning when unreferenced.
                        tracing::warn!(
                            path = %skill_path,
                            error = %e,
                            "skip invalid SKILL.md frontmatter"
                        );
                    }
                }
            }
            Err(_) => continue,
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

fn discover_skill_paths(vfs: &dyn VirtualFs, root: &str) -> Vec<String> {
    let mut paths = Vec::new();
    collect_skill_paths(vfs, root, &mut paths);
    paths.sort();
    paths
}

fn collect_skill_paths(vfs: &dyn VirtualFs, dir: &str, paths: &mut Vec<String>) {
    let Ok(entries) = vfs.list_dir(dir) else {
        return;
    };

    for entry in entries {
        let child = format!("{dir}/{entry}");
        let skill_path = format!("{child}/SKILL.md");
        if vfs.exists(&skill_path) {
            paths.push(skill_path);
        }

        if vfs.metadata(&child).is_ok_and(|meta| meta.is_dir) {
            collect_skill_paths(vfs, &child, paths);
        }
    }
}

#[cfg(feature = "sandbox")]
fn truncate_to_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
