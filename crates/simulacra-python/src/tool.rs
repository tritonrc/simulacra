use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use opentelemetry::KeyValue;
use serde_json::{Value, json};
use simulacra_sandbox::{AgentCell, SandboxError};
use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError};

use crate::{ExternalDispatcher, PythonError, PythonMeters, PythonResourceLimits, PythonRuntime};

/// Simulacra Tool implementation for Python code execution via Monty.
pub struct PyExecTool {
    pub(crate) cell: Arc<AgentCell>,
}

impl PyExecTool {
    /// Create a new PyExecTool backed by the given AgentCell.
    pub fn new(cell: Arc<AgentCell>) -> Self {
        Self { cell }
    }
}

impl Tool for PyExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "py_exec".into(),
            description: "Execute Python code in the Monty runtime and return the result.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python code to execute." }
                },
                "required": ["code"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        // Capability check: use the passed capability token (not self.cell.capability)
        // so that the caller's effective permissions are enforced.
        // Done eagerly before the async block to avoid lifetime issues.
        let cap_result = capability
            .check_python()
            .map_err(ToolError::CapabilityDenied);

        Box::pin(async move {
            cap_result?;

            let code = args
                .get("code")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("missing required field: code".into())
                })?;

            self.cell
                .begin_python_execution()
                .map_err(map_sandbox_error)?;

            let meters = PythonMeters::get();

            let code_length = code.len() as i64;

            let start = std::time::Instant::now();

            // Build a dispatcher that routes through the AgentCell
            let dispatcher = AgentCellDispatcher {
                cell: Arc::clone(&self.cell),
            };

            // Create runtime with sensible hardcoded limits to prevent infinite loops
            // and stack overflows. Memory/allocation limits left at None (Monty defaults).
            let limits = PythonResourceLimits {
                max_duration: Some(std::time::Duration::from_secs(30)),
                max_recursion_depth: Some(1000),
                ..PythonResourceLimits::default()
            };
            let runtime = PythonRuntime::new(limits);

            let span = tracing::info_span!(
                "simulacra_py_exec",
                simulacra.operation.name = "simulacra_py_exec",
                simulacra.python.code_length = code_length,
                simulacra.python.output_length = tracing::field::Empty,
            );

            // Execute Python on a blocking thread through the ScriptExecutor
            // (if configured) or inline as a fallback.
            // Note: we don't enter() the span before the await because
            // EnteredSpan is !Send. We record on it after execution.
            let exec_result = if let Some(executor) = self.cell.script_executor() {
                executor
                    .execute(move || runtime.execute(&code, &dispatcher))
                    .await
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!("script executor error: {e}"))
                    })?
            } else {
                let _entered = span.enter();
                runtime.execute(&code, &dispatcher)
            };

            let _span = span.entered();

            match exec_result {
                Ok(output) => {
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    let success_attrs = &[
                        KeyValue::new("simulacra.tool", "py_exec"),
                        KeyValue::new("status", "success"),
                    ];
                    meters.executions.add(1, success_attrs);
                    meters.execution_time.record(elapsed_ms, success_attrs);

                    // Return stdout if non-empty, otherwise the result value.
                    // If the result is Python None (or absent), return "" not "null".
                    let value_str = if !output.stdout.is_empty() {
                        output.stdout
                    } else {
                        match output.result.as_ref() {
                            Some(r) => {
                                let json_val = crate::monty_to_json(r);
                                if json_val.is_null() {
                                    String::new()
                                } else {
                                    json_val.to_string()
                                }
                            }
                            None => String::new(),
                        }
                    };

                    _span.record("simulacra.python.output_length", value_str.len() as i64);
                    tracing::info!(
                        output_length = value_str.len(),
                        elapsed_ms,
                        "py_exec completed successfully"
                    );

                    Ok(json!(value_str))
                }
                Err(PythonError::ResourceLimitExceeded(msg)) => {
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    let error_attrs = &[
                        KeyValue::new("simulacra.tool", "py_exec"),
                        KeyValue::new("status", "error"),
                    ];
                    meters.executions.add(1, error_attrs);
                    meters.execution_time.record(elapsed_ms, error_attrs);
                    let limit_attrs = &[
                        KeyValue::new("simulacra.tool", "py_exec"),
                        KeyValue::new("limit_type", msg.clone()),
                    ];
                    meters.resource_limit_exceeded.add(1, limit_attrs);

                    tracing::warn!(
                        limit_type = %msg,
                        elapsed_ms,
                        "py_exec resource limit exceeded"
                    );

                    Err(ToolError::ExecutionFailed(format!(
                        "resource limit exceeded: {msg}"
                    )))
                }
                Err(err) => {
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    let error_attrs = &[
                        KeyValue::new("simulacra.tool", "py_exec"),
                        KeyValue::new("status", "error"),
                    ];
                    meters.executions.add(1, error_attrs);
                    meters.execution_time.record(elapsed_ms, error_attrs);

                    tracing::error!(
                        error = %err,
                        elapsed_ms,
                        "py_exec execution failed"
                    );

                    Err(ToolError::ExecutionFailed(format!("python error: {err}")))
                }
            }
        })
    }
}

fn map_sandbox_error(err: SandboxError) -> ToolError {
    match err {
        SandboxError::CapabilityDenied(denied) => ToolError::CapabilityDenied(denied),
        other => ToolError::ExecutionFailed(other.to_string()),
    }
}

/// Bridges the synchronous ExternalDispatcher trait to AgentCell operations.
struct AgentCellDispatcher {
    cell: Arc<AgentCell>,
}

impl ExternalDispatcher for AgentCellDispatcher {
    fn read_file(&self, path: &str) -> Result<String, String> {
        let meters = PythonMeters::get();
        let attrs = &[KeyValue::new("simulacra.py.external_call", "read_file")];
        meters.external_calls.add(1, attrs);

        let _span = tracing::info_span!(
            "simulacra_py_external_call",
            simulacra.operation.name = "simulacra_py_external_call",
            simulacra.py.call = "read_file",
        )
        .entered();

        self.cell
            .read_file(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|e| format!("{e}"))
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        let meters = PythonMeters::get();
        let attrs = &[KeyValue::new("simulacra.py.external_call", "write_file")];
        meters.external_calls.add(1, attrs);

        let _span = tracing::info_span!(
            "simulacra_py_external_call",
            simulacra.operation.name = "simulacra_py_external_call",
            simulacra.py.call = "write_file",
        )
        .entered();

        self.cell
            .write_file(path, content.as_bytes())
            .map_err(|e| format!("{e}"))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let meters = PythonMeters::get();
        let attrs = &[KeyValue::new("simulacra.py.external_call", "list_dir")];
        meters.external_calls.add(1, attrs);

        let _span = tracing::info_span!(
            "simulacra_py_external_call",
            simulacra.operation.name = "simulacra_py_external_call",
            simulacra.py.call = "list_dir",
        )
        .entered();

        self.cell.list_dir(path).map_err(|e| format!("{e}"))
    }

    fn http_get(&self, url: &str) -> Result<String, String> {
        let meters = PythonMeters::get();
        let attrs = &[KeyValue::new("simulacra.py.external_call", "http_get")];
        meters.external_calls.add(1, attrs);

        let _span = tracing::info_span!(
            "simulacra_py_external_call",
            simulacra.operation.name = "simulacra_py_external_call",
            simulacra.py.call = "http_get",
        )
        .entered();

        // HTTP operations need async runtime — block on it
        self.cell
            .fetch_http(url, "GET", &[], None, None)
            .map(|resp| String::from_utf8_lossy(&resp.body).into_owned())
            .map_err(|e| format!("{e}"))
    }

    fn http_post(&self, url: &str, body: &str) -> Result<String, String> {
        let meters = PythonMeters::get();
        let attrs = &[KeyValue::new("simulacra.py.external_call", "http_post")];
        meters.external_calls.add(1, attrs);

        let _span = tracing::info_span!(
            "simulacra_py_external_call",
            simulacra.operation.name = "simulacra_py_external_call",
            simulacra.py.call = "http_post",
        )
        .entered();

        self.cell
            .fetch_http(url, "POST", &[], Some(body.as_bytes()), None)
            .map(|resp| String::from_utf8_lossy(&resp.body).into_owned())
            .map_err(|e| format!("{e}"))
    }

    fn env_get(&self, name: &str) -> Result<Option<String>, String> {
        let meters = PythonMeters::get();
        let attrs = &[KeyValue::new("simulacra.py.external_call", "env_get")];
        meters.external_calls.add(1, attrs);

        let _span = tracing::info_span!(
            "simulacra_py_external_call",
            simulacra.operation.name = "simulacra_py_external_call",
            simulacra.py.call = "env_get",
        )
        .entered();

        // Never read from host process environment — that would bypass the
        // capability system and leak host secrets to Python-capable agents.
        let _ = name;
        Ok(None)
    }
}
