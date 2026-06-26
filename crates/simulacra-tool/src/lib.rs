//! Simulacra tool crate.
//!
//! Registry for tools that can be offered to an LLM and invoked
//! when the model returns a tool-use response.

use std::collections::HashMap;
#[cfg(feature = "sandbox")]
use std::future::Future;
#[cfg(feature = "sandbox")]
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_hooks::pipeline::HookPipeline;
use simulacra_hooks::verdict::Operation;
#[cfg(feature = "sandbox")]
use simulacra_sandbox::{AgentCell, SandboxError};
use simulacra_types::VirtualFs;

pub use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError};

pub mod memory;
pub use memory::{
    MemoryReadChunkTool, MemoryToolHandles, SemanticSearchTool, register_memory_tools,
};

const MAX_TOOL_MESSAGE_EVENT_CHARS: usize = 4096;

fn tool_message_event_payload(result: &Value) -> (String, usize, bool) {
    let full = result.to_string();
    let full_len = full.len();
    let mut truncated = false;
    let message = if full.chars().count() > MAX_TOOL_MESSAGE_EVENT_CHARS {
        truncated = true;
        full.chars()
            .take(MAX_TOOL_MESSAGE_EVENT_CHARS)
            .collect::<String>()
    } else {
        full
    };
    (message, full_len, truncated)
}

fn emit_tool_result_event(name: &str, result: &Value) {
    let (result_message, result_len, truncated) = tool_message_event_payload(result);
    tracing::info!(
        gen_ai.tool.name = name,
        gen_ai.tool.message = result_message,
        gen_ai.tool.result_length = result_len,
        gen_ai.tool.message_truncated = truncated,
        "tool result"
    );
}

/// Errors that can occur during skill discovery and filtering.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// Two skills resolved to the same frontmatter `name`.
    #[error("duplicate skill name {name:?} discovered at {first_path} and {second_path}")]
    DuplicateSkillName {
        name: String,
        first_path: String,
        second_path: String,
    },

    /// An agent type references a skill that was not found in the VFS.
    #[error(
        "agent type {agent_type:?} references undiscoverable skill {skill:?}: \
         no valid /skills/{skill}/SKILL.md found"
    )]
    UndiscoverableSkill { agent_type: String, skill: String },
}

/// Registry holding available tools.
///
/// Provides lookup by name and capability-checked invocation.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    pipeline: Option<Arc<HookPipeline>>,
}

impl ToolRegistry {
    /// Create an empty tool registry.
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            pipeline: None,
        }
    }

    /// Set the governance hook pipeline for tool call interception.
    pub fn set_pipeline(&mut self, pipeline: Arc<HookPipeline>) {
        self.pipeline = Some(pipeline);
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Return definitions for all registered tools.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    /// Look up a tool by name, check capabilities, and call it.
    ///
    /// Creates an OTel span with `gen_ai.tool.name` and emits result/error events.
    /// When a governance hook pipeline is set, tool calls are wrapped with
    /// before/after hooks that can modify arguments/results or deny/kill.
    pub fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
        capability: &'a CapabilityToken,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, ToolError>> + 'a {
        // Create the span eagerly (before the future is polled) so it is
        // registered with whatever subscriber is active at construction time.
        let span = tracing::info_span!("tool_invoke", gen_ai.tool.name = name);

        async move {
            let _guard = span.enter();

            let tool = self
                .tools
                .iter()
                .find(|t| t.definition().name == name)
                .ok_or_else(|| ToolError::ExecutionFailed(format!("unknown tool: {name}")))?;

            // Tools that own their own hook lifecycle (e.g. memory tools that
            // translate deny verdicts into graceful payload shapes) opt out of
            // the generic before/after wrapping so hooks do not fire twice.
            let tool_owns_hooks = tool.handles_own_hooks();

            // --- BEFORE hook ---
            let effective_args = if let Some(ref pipeline) = self.pipeline
                && !tool_owns_hooks
            {
                let before_ctx = json!({
                    "tool": name,
                    "arguments": &arguments,
                })
                .to_string();
                match pipeline.run_before(Operation::ToolCall, &before_ctx) {
                    Ok((verdict, modified_ctx)) => match verdict {
                        simulacra_hooks::Verdict::Continue(_) => {
                            // If the hook modified the context, extract the
                            // updated arguments from it.
                            if let Ok(parsed) = serde_json::from_str::<Value>(&modified_ctx) {
                                if let Some(args) = parsed.get("arguments") {
                                    args.clone()
                                } else {
                                    arguments
                                }
                            } else {
                                arguments
                            }
                        }
                        simulacra_hooks::Verdict::Deny(reason) => {
                            return Err(ToolError::ExecutionFailed(format!(
                                "hook denied tool call: {reason}"
                            )));
                        }
                        simulacra_hooks::Verdict::Kill(_) => {
                            unreachable!("Kill is returned as Err from run_before")
                        }
                    },
                    Err(e) => {
                        return Err(ToolError::ExecutionFailed(format!("hook error: {e}")));
                    }
                }
            } else {
                arguments
            };

            // --- EXECUTE ---
            let result = tool.call(effective_args.clone(), capability).await;

            // --- AFTER hook ---
            match result {
                Ok(result_value) => {
                    if let Some(ref pipeline) = self.pipeline
                        && !tool_owns_hooks
                    {
                        let after_ctx = json!({
                            "tool": name,
                            "arguments": &effective_args,
                            "result": &result_value,
                        })
                        .to_string();
                        match pipeline.run_after(Operation::ToolCall, &after_ctx) {
                            Ok((_verdict, modified_ctx)) => {
                                // Extract possibly-modified result
                                let final_result = if let Ok(parsed) =
                                    serde_json::from_str::<Value>(&modified_ctx)
                                {
                                    if let Some(r) = parsed.get("result") {
                                        r.clone()
                                    } else {
                                        result_value
                                    }
                                } else {
                                    result_value
                                };
                                emit_tool_result_event(name, &final_result);
                                Ok(final_result)
                            }
                            Err(e) => Err(ToolError::ExecutionFailed(format!("hook error: {e}"))),
                        }
                    } else {
                        emit_tool_result_event(name, &result_value);
                        Ok(result_value)
                    }
                }
                Err(err) => {
                    tracing::error!(
                        gen_ai.tool.name = name,
                        error = %err,
                        "tool error"
                    );
                    Err(err)
                }
            }
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
fn map_sandbox_error(err: SandboxError) -> ToolError {
    match err {
        SandboxError::CapabilityDenied(denied) => ToolError::CapabilityDenied(denied),
        SandboxError::BudgetExhausted(exhausted) => {
            ToolError::ExecutionFailed(format!("budget exhausted: {exhausted}"))
        }
        SandboxError::Vfs(vfs_error) => ToolError::ExecutionFailed(format!("{vfs_error}")),
        other => ToolError::ExecutionFailed(format!("{other}")),
    }
}

// ---------------------------------------------------------------------------
// Helper: extract a required string field from JSON args
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
fn require_str(args: &Value, field: &str) -> Result<String, ToolError> {
    args.get(field)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidArguments(format!("missing required field: {field}")))
}

// ---------------------------------------------------------------------------
// FileReadTool
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
struct FileReadTool {
    cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for FileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_read".into(),
            description: "Read the contents of a file at the given path.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file to read." }
                },
                "required": ["path"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let path = require_str(&args, "path")?;

            match self.cell.read_file(&path) {
                Ok(data) => {
                    let content = String::from_utf8_lossy(&data).into_owned();
                    Ok(json!(content))
                }
                Err(SandboxError::Vfs(ref vfs_err))
                    if format!("{vfs_err}")
                        .to_ascii_lowercase()
                        .contains("not found") =>
                {
                    Ok(json!({"is_error": true, "content": format!("not found: {path}")}))
                }
                Err(err) => Err(map_sandbox_error(err)),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// FileWriteTool
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
struct FileWriteTool {
    cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for FileWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_write".into(),
            description: "Write content to a file, creating parent directories as needed.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file to write." },
                    "content": { "type": "string", "description": "Content to write to the file." }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let path = require_str(&args, "path")?;
            let content = require_str(&args, "content")?;

            let bytes = content.as_bytes();
            match self.cell.write_file(&path, bytes) {
                Ok(()) => {
                    let msg = format!("wrote {} bytes to {path}", bytes.len());
                    Ok(json!(msg))
                }
                Err(err) => Err(map_sandbox_error(err)),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// FileEditTool
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
struct FileEditTool {
    cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for FileEditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_edit".into(),
            description: "Apply a search-and-replace edit to an existing file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file to edit." },
                    "old_string": { "type": "string", "description": "The exact text to search for." },
                    "new_string": { "type": "string", "description": "The replacement text." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let path = require_str(&args, "path")?;
            let old_string = require_str(&args, "old_string")?;
            let new_string = require_str(&args, "new_string")?;

            // Read the file
            let data = match self.cell.read_file(&path) {
                Ok(d) => d,
                Err(SandboxError::Vfs(ref vfs_err))
                    if format!("{vfs_err}")
                        .to_ascii_lowercase()
                        .contains("not found") =>
                {
                    return Ok(
                        json!({"is_error": true, "content": format!("file not found: {path}")}),
                    );
                }
                Err(err) => {
                    return Err(map_sandbox_error(err));
                }
            };

            let content = String::from_utf8_lossy(&data).into_owned();
            let count = content.matches(&old_string).count();

            if count == 0 {
                return Ok(
                    json!({"is_error": true, "content": format!("old_string not found in {path}")}),
                );
            }

            if count > 1 {
                return Ok(
                    json!({"is_error": true, "content": format!("ambiguous: old_string appears {count} times in {path}")}),
                );
            }

            // Exactly one occurrence -- replace and write back
            let new_content = content.replacen(&old_string, &new_string, 1);
            match self.cell.write_file(&path, new_content.as_bytes()) {
                Ok(()) => Ok(json!(format!("edited {path}: replaced 1 occurrence"))),
                Err(err) => Err(map_sandbox_error(err)),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ShellExecTool
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
struct ShellExecTool {
    cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for ShellExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute a shell command in the agent's virtual shell and return \
                stdout, stderr, and exit code. \
                Supported builtins: echo, cat, ls, mkdir, cp, mv, rm, head, tail, sed, grep, \
                wc, find, sort, uniq, cut, tr, tee, true, false, cd, pwd, env, which, export, \
                curl, wget. \
                Operators: pipes (|), redirects (>, >>), conditional chains (&&, ||), \
                sequence (;). State that persists across calls: env vars and the working \
                directory (cd /tmp; later calls see /tmp as cwd). Interpreter aliases: \
                node <file.js>, node -e <code>, node - for stdin, python <script.py>, \
                python -c <code>, and python - for stdin run through mediated sandbox \
                runtimes. All paths resolve inside the agent's sandbox VFS — there is no \
                host filesystem access."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute." }
                },
                "required": ["command"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let command = require_str(&args, "command")?;

            match self.cell.execute_shell(&command) {
                Ok(cmd_result) => Ok(json!({
                    "stdout": cmd_result.stdout,
                    "stderr": cmd_result.stderr,
                    "exit_code": cmd_result.exit_code,
                })),
                Err(err) => Err(map_sandbox_error(err)),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// JsExecTool
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
struct JsExecTool {
    cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for JsExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "js_exec".into(),
            description: "Execute JavaScript code in QuickJS and return the string result or \
                stdout. Each call gets a fresh JS global/context: variables, prototypes, and \
                module singletons do not persist between calls. Use ESM `import`, not \
                `require`. Available modules include simulacra:fs/fs, simulacra:console, \
                simulacra:process, simulacra:path, and simulacra:crypto. File, fetch, and module-load \
                host operations are mediated by the sandbox."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "JavaScript code to execute." }
                },
                "required": ["code"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let code = require_str(&args, "code")?;

            // Acquire a script executor permit for backpressure.
            // JS runtime is !Send so we can't use spawn_blocking —
            // we execute inline but the permit limits concurrency.
            let _permit = if let Some(executor) = self.cell.script_executor() {
                Some(executor.acquire_permit().await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("script executor error: {e}"))
                })?)
            } else {
                None
            };

            match self.cell.execute_js(&code) {
                Ok(output) => {
                    let value_str = output.result.unwrap_or_else(|| output.stdout.clone());
                    Ok(json!(value_str))
                }
                Err(SandboxError::Js(js_err)) => {
                    Ok(json!({"is_error": true, "content": format!("js error: {js_err}")}))
                }
                Err(err) => Err(map_sandbox_error(err)),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ListDirTool
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
struct ListDirTool {
    cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for ListDirTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_dir".into(),
            description: "List the contents of a directory.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the directory to list." }
                },
                "required": ["path"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let path = require_str(&args, "path")?;

            match self.cell.list_dir(&path) {
                Ok(entries) => {
                    let mut lines = Vec::new();
                    for entry in &entries {
                        let full_path = if path == "/" {
                            format!("/{entry}")
                        } else {
                            format!("{path}/{entry}")
                        };
                        let is_dir = self
                            .cell
                            .metadata(&full_path)
                            .map_err(map_sandbox_error)?
                            .is_dir;
                        if is_dir {
                            lines.push(format!("{entry}/"));
                        } else {
                            lines.push(entry.clone());
                        }
                    }
                    Ok(json!(lines.join("\n")))
                }
                Err(ref sandbox_err @ SandboxError::Vfs(ref vfs_err))
                    if format!("{vfs_err}")
                        .to_ascii_lowercase()
                        .contains("not found")
                        || format!("{vfs_err}")
                            .to_ascii_lowercase()
                            .contains("not a directory") =>
                {
                    Ok(json!({"is_error": true, "content": format!("{sandbox_err}")}))
                }
                Err(err) => Err(map_sandbox_error(err)),
            }
        })
    }
}

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

// ---------------------------------------------------------------------------
// register_builtins
// ---------------------------------------------------------------------------

/// Register all built-in tools into the given registry.
#[cfg(feature = "sandbox")]
pub fn register_builtins(registry: &mut ToolRegistry, cell: Arc<AgentCell>) {
    registry.register(Box::new(FileReadTool {
        cell: Arc::clone(&cell),
    }));
    registry.register(Box::new(FileWriteTool {
        cell: Arc::clone(&cell),
    }));
    registry.register(Box::new(FileEditTool {
        cell: Arc::clone(&cell),
    }));
    registry.register(Box::new(ShellExecTool {
        cell: Arc::clone(&cell),
    }));
    registry.register(Box::new(JsExecTool {
        cell: Arc::clone(&cell),
    }));
    registry.register(Box::new(ListDirTool { cell }));
}
