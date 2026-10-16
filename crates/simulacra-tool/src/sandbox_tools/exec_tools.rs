//! The `shell_exec` and `js_exec` tools.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_sandbox::{AgentCell, SandboxError};
use simulacra_types::{
    CapabilityToken, Tool, ToolDefinition, ToolError, ToolOutput, truncate_chars,
};

use super::{map_sandbox_error, optional_str, optional_u64, reject_unknown_args, require_str};

// ---------------------------------------------------------------------------
// ShellExecTool
// ---------------------------------------------------------------------------

#[cfg(feature = "sandbox")]
pub(super) struct ShellExecTool {
    pub(super) cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for ShellExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell_exec".into(),
            description:
                "Execute a shell command in the sandbox shell and return structured output. Supports common builtins, pipes, redirects, heredocs, and persistent cwd/env.".into(),
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

    fn advertised_to(&self, capability: &CapabilityToken) -> bool {
        capability.check_shell().is_ok()
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
pub(super) struct JsExecTool {
    pub(super) cell: Arc<AgentCell>,
}

#[cfg(feature = "sandbox")]
impl Tool for JsExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "js_exec".into(),
            description: "Execute JavaScript code in QuickJS and return the string result or \
                stdout. Each call gets a fresh JS global/context: variables, prototypes, and \
                module singletons do not persist between calls. Use ESM `import`, not \
                `require`. Built-in modules can be imported with either `simulacra:*` names \
                or Node-like aliases (`fs`, `console`, `process`, `path`, `crypto`). \
                `simulacra:fs`/`fs` exports readFileSync, writeFileSync, readFile, \
                writeFile, existsSync, appendFileSync, readdirSync, statSync, renameSync, \
                unlinkSync, and mkdirSync. File, fetch, and module-load \
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

    fn advertised_to(&self, capability: &CapabilityToken) -> bool {
        capability.check_javascript().is_ok()
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let code = require_str(&args, "code")?;

            match self.cell.execute_js_async(&code).await {
                Ok(output) => {
                    let value_str = output.result.unwrap_or_else(|| output.stdout.clone());
                    // Script output is model-facing content; keep it out of the
                    // dispatch layer's result log with a bounded placeholder.
                    Ok(ToolOutput::success(value_str)
                        .with_log_preview("[js evaluation output — content is model-facing]")
                        .to_value())
                }
                Err(SandboxError::Js(js_err)) => {
                    Ok(ToolOutput::error(format!("js error: {js_err}"))
                        .with_log_preview("[js evaluation failed — error text is model-facing]")
                        .to_value())
                }
                Err(err) => Err(map_sandbox_error(err)),
            }
        })
    }
}
