//! Sandbox-backed builtin tool adapters.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_sandbox::{AgentCell, SandboxError};
use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError, ToolOutput};

use crate::{FilePresenter, FileReadRequest};

#[cfg(feature = "sandbox")]
mod apply_patch;
#[cfg(feature = "sandbox")]
mod blocking;
#[cfg(feature = "sandbox")]
mod exec_tools;
#[cfg(feature = "sandbox")]
use apply_patch::ApplyPatchTool;
#[cfg(feature = "sandbox")]
use exec_tools::{JsExecTool, ShellExecTool};
#[cfg(feature = "sandbox")]
mod tool_sets;
#[cfg(feature = "sandbox")]
mod window_args;
#[cfg(feature = "sandbox")]
use blocking::on_blocking_thread;
#[cfg(feature = "sandbox")]
pub use tool_sets::{
    file_tools, register_builtins, register_builtins_with_presenter, register_exec_tools,
    register_file_tools, register_file_tools_with_presenter,
};
#[cfg(feature = "sandbox")]
use window_args::optional_window;

// File-tool advertisement can only use the coarse, statically decidable
// signal that some matching grant exists. It does not imply that the path in a
// future call is allowed; AgentCell performs the authoritative per-path check.

fn has_read_grant(capability: &CapabilityToken) -> bool {
    !capability.paths_read.is_empty()
        || (capability.memory.enabled && !capability.memory.search_scopes.is_empty())
}

fn has_write_grant(capability: &CapabilityToken) -> bool {
    !capability.paths_write.is_empty()
        || (capability.memory.enabled && !capability.memory.write_scopes.is_empty())
}

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
    presenter: Arc<dyn FilePresenter>,
}

#[cfg(feature = "sandbox")]
impl Tool for FileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_read".into(),
            description: "Read a file at the given path. offset and limit select a byte window."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file to read." },
                    "offset": { "type": "integer", "minimum": 0, "description": "First byte to read. Default 0." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Most bytes to read. Default: the whole file, or the host's limit." }
                },
                "required": ["path"]
            }),
        }
    }

    fn advertised_to(&self, capability: &CapabilityToken) -> bool {
        has_read_grant(capability)
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let path = require_str(&args, "path")?;
            let offset = optional_window(&args, "offset", 0)?;
            let limit = optional_window(&args, "limit", 1)?;

            let read_path = path.clone();
            let result =
                on_blocking_thread(&self.cell, move |cell| cell.read_file(&read_path)).await?;
            match result {
                Ok(data) => self
                    .presenter
                    .present(
                        FileReadRequest {
                            path: path.clone(),
                            offset,
                            limit,
                        },
                        data,
                    )
                    .await
                    .map(|output| output.to_value()),
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

    fn advertised_to(&self, capability: &CapabilityToken) -> bool {
        has_write_grant(capability)
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let path = require_str(&args, "path")?;
            let content = require_str(&args, "content")?;

            let data = content.into_bytes();
            let len = data.len();
            let write_path = path.clone();
            let result =
                on_blocking_thread(&self.cell, move |cell| cell.write_file(&write_path, &data))
                    .await?;
            match result {
                Ok(()) => {
                    let msg = format!("wrote {len} bytes to {path}");
                    Ok(ToolOutput::success(msg).to_value())
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

    fn advertised_to(&self, capability: &CapabilityToken) -> bool {
        has_read_grant(capability)
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let path = require_str(&args, "path")?;

            let list_path = path.clone();
            let result = on_blocking_thread(&self.cell, move |cell| {
                // Only a failed listing takes the lenient not-found arms
                // below; a failed per-entry lookup is a tool error.
                let listing = match cell.list_dir_bounded(&list_path) {
                    Ok(listing) => listing,
                    Err(err) => return Ok::<_, ToolError>(Err(err)),
                };
                let mut lines = Vec::with_capacity(listing.entries.len() + 1);
                for entry in &listing.entries {
                    let full_path = if list_path == "/" {
                        format!("/{entry}")
                    } else {
                        format!("{list_path}/{entry}")
                    };
                    let is_dir = cell.metadata(&full_path).map_err(map_sandbox_error)?.is_dir;
                    if is_dir {
                        lines.push(format!("{entry}/"));
                    } else {
                        lines.push(entry.clone());
                    }
                }
                if listing.truncated {
                    lines.push(format!(
                        "… truncated: listing stopped at {} entries",
                        listing.entries.len()
                    ));
                }
                Ok(Ok(lines))
            })
            .await??;
            match result {
                Ok(lines) => Ok(ToolOutput::success(lines.join("\n")).to_value()),
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
