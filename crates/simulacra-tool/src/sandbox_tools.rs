//! Sandbox-backed builtin tool adapters.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_sandbox::{AgentCell, SandboxError};
use simulacra_types::{
    CapabilityToken, Tool, ToolDefinition, ToolError, ToolOutput, truncate_chars,
};

use crate::ToolRegistry;

#[cfg(feature = "sandbox")]
mod apply_patch;
#[cfg(feature = "sandbox")]
use apply_patch::ApplyPatchTool;

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

#[cfg(feature = "sandbox")]
pub(crate) fn optional_str(args: &Value, field: &str) -> Result<Option<String>, ToolError> {
    match args.get(field) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "field must be a string: {field}"
        ))),
        None => Ok(None),
    }
}

#[cfg(feature = "sandbox")]
pub(crate) fn optional_u64(args: &Value, field: &str) -> Result<Option<u64>, ToolError> {
    match args.get(field) {
        Some(Value::Number(value)) => value.as_u64().map(Some).ok_or_else(|| {
            ToolError::InvalidArguments(format!("field must be a non-negative integer: {field}"))
        }),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "field must be a non-negative integer: {field}"
        ))),
        None => Ok(None),
    }
}

#[cfg(feature = "sandbox")]
fn reject_unknown_args(args: &Value, allowed: &[&str]) -> Result<(), ToolError> {
    let Some(object) = args.as_object() else {
        return Err(ToolError::InvalidArguments(
            "tool arguments must be an object".into(),
        ));
    };
    if let Some(unknown) = object
        .keys()
        .find(|key| !allowed.iter().any(|allowed| allowed == key))
    {
        return Err(ToolError::InvalidArguments(format!(
            "unknown argument: {unknown}"
        )));
    }
    Ok(())
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
                    Ok(ToolOutput::success(content).to_value())
                }
                Err(SandboxError::Vfs(ref vfs_err))
                    if format!("{vfs_err}")
                        .to_ascii_lowercase()
                        .contains("not found") =>
                {
                    Ok(ToolOutput::error(format!("not found: {path}")).to_value())
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
                    Ok(ToolOutput::success(msg).to_value())
                }
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
            description:
                "Execute a shell command in the sandbox shell and return structured output.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute." },
                    "workdir": { "type": "string", "description": "Optional one-call VFS working directory." },
                    "yield_time_ms": { "type": "integer", "minimum": 0, "description": "Accepted for future streaming ergonomics; no persistent session is created." },
                    "max_output_tokens": { "type": "integer", "minimum": 0, "description": "Approximate token budget for returned stdout/stderr." }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            reject_unknown_args(
                &args,
                &[
                    "command",
                    "workdir",
                    "yield_time_ms",
                    "max_output_tokens",
                    "session_id",
                    "stdin",
                    "input",
                ],
            )?;
            let command = require_str(&args, "command")?;
            let workdir = optional_str(&args, "workdir")?;
            let _yield_time_ms = optional_u64(&args, "yield_time_ms")?;
            let max_output_tokens = optional_u64(&args, "max_output_tokens")?;
            for unsupported in ["session_id", "stdin", "input"] {
                if args.get(unsupported).is_some() {
                    return Err(ToolError::InvalidArguments(format!(
                        "unsupported shell_exec argument without persistent sessions: {unsupported}"
                    )));
                }
            }

            match self
                .cell
                .execute_shell_with_workdir(&command, workdir.as_deref())
            {
                Ok(cmd_result) => {
                    let stdout_original_len = cmd_result.stdout.chars().count();
                    let stderr_original_len = cmd_result.stderr.chars().count();
                    let max_chars = max_output_tokens.map(|tokens| {
                        usize::try_from(tokens.saturating_mul(4)).unwrap_or(usize::MAX)
                    });
                    let (stdout, stdout_truncated) = if let Some(max_chars) = max_chars {
                        truncate_chars(&cmd_result.stdout, max_chars)
                    } else {
                        (cmd_result.stdout, false)
                    };
                    let (stderr, stderr_truncated) = if let Some(max_chars) = max_chars {
                        truncate_chars(&cmd_result.stderr, max_chars)
                    } else {
                        (cmd_result.stderr, false)
                    };
                    let stdout_truncated_len = stdout.chars().count();
                    let stderr_truncated_len = stderr.chars().count();
                    let structured = json!({
                        "stdout": stdout,
                        "stderr": stderr,
                        "exit_code": cmd_result.exit_code,
                        "truncated": stdout_truncated || stderr_truncated,
                        "stdout_original_len": stdout_original_len,
                        "stderr_original_len": stderr_original_len,
                        "stdout_truncated_len": stdout_truncated_len,
                        "stderr_truncated_len": stderr_truncated_len,
                    });
                    Ok(ToolOutput::success(structured.to_string())
                        .with_structured(structured)
                        .to_value())
                }
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
                    Ok(ToolOutput::success(value_str).to_value())
                }
                Err(SandboxError::Js(js_err)) => {
                    Ok(ToolOutput::error(format!("js error: {js_err}")).to_value())
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
                    Ok(ToolOutput::success(lines.join("\n")).to_value())
                }
                Err(ref sandbox_err @ SandboxError::Vfs(ref vfs_err))
                    if format!("{vfs_err}")
                        .to_ascii_lowercase()
                        .contains("not found")
                        || format!("{vfs_err}")
                            .to_ascii_lowercase()
                            .contains("not a directory") =>
                {
                    Ok(ToolOutput::error(format!("{sandbox_err}")).to_value())
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
pub fn register_builtins(
    registry: &mut ToolRegistry,
    cell: Arc<AgentCell>,
) -> Result<(), ToolError> {
    for name in [
        "file_read",
        "file_write",
        "apply_patch",
        "shell_exec",
        "js_exec",
        "list_dir",
    ] {
        if registry.metadata(name).is_some() {
            return Err(ToolError::ExecutionFailed(format!(
                "duplicate tool registration: {name}"
            )));
        }
    }

    registry.register(Box::new(FileReadTool {
        cell: Arc::clone(&cell),
    }))?;
    registry.register(Box::new(FileWriteTool {
        cell: Arc::clone(&cell),
    }))?;
    registry.register(Box::new(ApplyPatchTool {
        cell: Arc::clone(&cell),
    }))?;
    registry.register(Box::new(ShellExecTool {
        cell: Arc::clone(&cell),
    }))?;
    registry.register(Box::new(JsExecTool {
        cell: Arc::clone(&cell),
    }))?;
    registry.register(Box::new(ListDirTool { cell }))?;
    Ok(())
}
