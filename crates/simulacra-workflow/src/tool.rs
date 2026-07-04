use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError, ToolOutput, ToolSchema};

use crate::{WorkflowRunOptions, WorkflowRuntime};

/// Model-visible tool that starts a workflow run and returns promptly.
pub struct WorkflowTool {
    runtime: Arc<WorkflowRuntime>,
}

impl WorkflowTool {
    pub fn new(runtime: Arc<WorkflowRuntime>) -> Self {
        Self { runtime }
    }
}

impl Tool for WorkflowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Workflow".to_string(),
            description: "Start a persisted workflow orchestration run.".to_string(),
            input_schema: ToolSchema::object(
                [
                    ("script", ToolSchema::string("Inline workflow ESM source.")),
                    (
                        "name",
                        ToolSchema::string("Saved workflow name under /workflows."),
                    ),
                    (
                        "script_path",
                        ToolSchema::string("Saved workflow script path in the VFS."),
                    ),
                    (
                        "args",
                        json!({
                            "type": "object",
                            "description": "JSON arguments passed to the workflow.",
                            "additionalProperties": true
                        }),
                    ),
                    (
                        "resume_from_run_id",
                        ToolSchema::string("Previous workflow run id to resume from."),
                    ),
                ],
                [] as [&str; 0],
            ),
        }
    }

    fn call(
        &self,
        arguments: Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, ToolError>> + Send + '_>>
    {
        Box::pin(async move {
            let options = parse_tool_options(arguments)?;
            let handle = self
                .runtime
                .start(options)
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            let structured = json!({
                "run_id": handle.run_id(),
                "status": handle.status(),
                "script_path": handle.script_path(),
                "transcript_dir": handle.transcript_dir(),
            });
            Ok(ToolOutput::success(structured.to_string())
                .with_structured(structured)
                .to_value())
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
}

fn parse_tool_options(arguments: Value) -> Result<WorkflowRunOptions, ToolError> {
    let object = arguments
        .as_object()
        .ok_or_else(|| ToolError::InvalidArguments("Workflow input must be an object".into()))?;
    let script = optional_string(object, "script")?;
    let name = optional_string(object, "name")?;
    let script_path = optional_string(object, "script_path")?;
    if script.is_none() && name.is_none() && script_path.is_none() {
        return Err(ToolError::InvalidArguments(
            "Workflow requires script, name, or script_path".into(),
        ));
    }
    Ok(WorkflowRunOptions {
        run_id: optional_string(object, "run_id")?,
        script,
        name,
        script_path,
        args: object.get("args").cloned().unwrap_or_else(|| json!({})),
        resume_from_run_id: optional_string(object, "resume_from_run_id")?,
        concurrency_limit: object
            .get("concurrency_limit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or_else(crate::runtime::default_concurrency_limit),
    })
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<String>, ToolError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "Workflow field `{field}` must be a string"
        ))),
    }
}
