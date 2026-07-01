//! Sandbox-backed builtin tool adapters.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_sandbox::{AgentCell, SandboxError};
use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError};

use crate::ToolRegistry;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
pub(crate) fn map_sandbox_error(err: SandboxError) -> ToolError {
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
pub(crate) fn require_str(args: &Value, field: &str) -> Result<String, ToolError> {
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
