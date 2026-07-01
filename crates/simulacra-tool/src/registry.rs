//! Tool registry and generic tool-call hook orchestration.

use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_hooks::pipeline::HookPipeline;
use simulacra_hooks::verdict::Operation;
use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError};

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
