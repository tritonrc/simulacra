//! Builtin tool sets and their registration.

use std::sync::Arc;

use simulacra_sandbox::AgentCell;
use simulacra_types::{Tool, ToolError};

use super::{ApplyPatchTool, FileReadTool, FileWriteTool, JsExecTool, ListDirTool, ShellExecTool};
use crate::{FilePresenter, LossyTextPresenter, ToolRegistry};

fn register_tool_batch(
    registry: &mut ToolRegistry,
    tools: Vec<Box<dyn Tool>>,
) -> Result<(), ToolError> {
    let mut names = Vec::with_capacity(tools.len());
    for tool in &tools {
        let name = tool.definition().name;
        if registry.metadata(&name).is_some() || names.contains(&name) {
            return Err(ToolError::ExecutionFailed(format!(
                "duplicate tool registration: {name}"
            )));
        }
        names.push(name);
    }

    for tool in tools {
        registry.register(tool)?;
    }
    Ok(())
}

fn exec_tool_batch(cell: Arc<AgentCell>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ShellExecTool {
            cell: Arc::clone(&cell),
        }),
        Box::new(JsExecTool { cell }),
    ]
}

/// The four file tools over `cell`, for a host that wraps or registers them
/// itself. `file_read` presents through `presenter`.
pub fn file_tools(cell: Arc<AgentCell>, presenter: Arc<dyn FilePresenter>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(FileReadTool {
            cell: Arc::clone(&cell),
            presenter,
        }),
        Box::new(FileWriteTool {
            cell: Arc::clone(&cell),
        }),
        Box::new(ApplyPatchTool {
            cell: Arc::clone(&cell),
        }),
        Box::new(ListDirTool { cell }),
    ]
}

/// Register the four sandbox file tools into the given registry.
pub fn register_file_tools(
    registry: &mut ToolRegistry,
    cell: Arc<AgentCell>,
) -> Result<(), ToolError> {
    register_file_tools_with_presenter(registry, cell, Arc::new(LossyTextPresenter))
}

/// Like [`register_file_tools`], with `file_read` presenting through `presenter`.
pub fn register_file_tools_with_presenter(
    registry: &mut ToolRegistry,
    cell: Arc<AgentCell>,
    presenter: Arc<dyn FilePresenter>,
) -> Result<(), ToolError> {
    register_tool_batch(registry, file_tools(cell, presenter))
}

/// Register the two sandbox execution tools into the given registry.
pub fn register_exec_tools(
    registry: &mut ToolRegistry,
    cell: Arc<AgentCell>,
) -> Result<(), ToolError> {
    register_tool_batch(registry, exec_tool_batch(cell))
}

/// Register all six built-in tools into the given registry.
pub fn register_builtins(
    registry: &mut ToolRegistry,
    cell: Arc<AgentCell>,
) -> Result<(), ToolError> {
    register_builtins_with_presenter(registry, cell, Arc::new(LossyTextPresenter))
}

/// Like [`register_builtins`], with `file_read` presenting through `presenter`.
pub fn register_builtins_with_presenter(
    registry: &mut ToolRegistry,
    cell: Arc<AgentCell>,
    presenter: Arc<dyn FilePresenter>,
) -> Result<(), ToolError> {
    let mut tools = file_tools(Arc::clone(&cell), presenter);
    tools.extend(exec_tool_batch(cell));
    register_tool_batch(registry, tools)
}
