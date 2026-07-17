//! Tool registry and generic tool-call hook orchestration.

use std::sync::Arc;

use serde_json::Value;
use simulacra_hooks::pipeline::HookPipeline;
use simulacra_hooks::verdict::Operation;
use simulacra_types::{
    CapabilityToken, TOOL_PREVIEW_MAX_CHARS, Tool, ToolDefinition, ToolError, ToolOutput,
    truncate_chars,
};

/// Model/provider exposure level for a registered tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExposure {
    Direct,
    Hidden,
    Deferred,
}

/// Registry-owned tool metadata that providers do not need to receive.
#[derive(Debug, Clone)]
pub struct ToolMetadata {
    pub exposure: ToolExposure,
    pub output_schema: Option<Value>,
    pub supports_parallel_tool_calls: bool,
    pub waits_for_runtime_cancellation: bool,
}

struct RegisteredTool {
    tool: Box<dyn Tool>,
    exposure: ToolExposure,
}

impl RegisteredTool {
    fn definition(&self) -> ToolDefinition {
        self.tool.definition()
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            exposure: self.exposure,
            output_schema: self.tool.output_schema(),
            supports_parallel_tool_calls: self.tool.supports_parallel_tool_calls(),
            waits_for_runtime_cancellation: self.tool.waits_for_runtime_cancellation(),
        }
    }
}

fn tool_message_event_payload(output: &ToolOutput) -> (String, usize, bool) {
    let full = output.log_preview.clone();
    let full_len = output.content.chars().count();
    let content_was_truncated = output.content.chars().count() > TOOL_PREVIEW_MAX_CHARS;
    let (message, preview_was_truncated) = truncate_chars(&full, TOOL_PREVIEW_MAX_CHARS);
    let truncated = content_was_truncated || preview_was_truncated;
    (message, full_len, truncated)
}

fn emit_tool_result_event(name: &str, output: &ToolOutput) {
    let (result_message, result_len, truncated) = if name == "mcp_call" {
        (
            "[REDACTED]".to_string(),
            output.content.chars().count(),
            false,
        )
    } else {
        tool_message_event_payload(output)
    };
    tracing::info!(
        gen_ai.tool.name = name,
        gen_ai.tool.message = result_message,
        gen_ai.tool.result_length = result_len,
        gen_ai.tool.message_truncated = truncated,
        "tool result"
    );
}

fn emit_tool_error_event(name: &str, err: &ToolError) {
    tracing::error!(
        gen_ai.tool.name = name,
        error = %err,
        "tool error"
    );
}

/// Registry holding available tools.
///
/// Provides lookup by name and capability-checked invocation.
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
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

    /// Register a directly exposed tool, returning an error for duplicate names.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> {
        self.try_register(tool)
    }

    /// Register a directly exposed tool, returning an error for duplicate names.
    pub fn try_register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> {
        self.try_register_with_exposure(tool, ToolExposure::Direct)
    }

    /// Register a tool with explicit exposure metadata.
    pub fn try_register_with_exposure(
        &mut self,
        tool: Box<dyn Tool>,
        exposure: ToolExposure,
    ) -> Result<(), ToolError> {
        let name = tool.definition().name;
        if self
            .tools
            .iter()
            .any(|registered| registered.definition().name == name)
        {
            return Err(ToolError::ExecutionFailed(format!(
                "duplicate tool registration: {name}"
            )));
        }
        self.tools.push(RegisteredTool { tool, exposure });
        Ok(())
    }

    /// Register a hidden dispatch-only tool.
    pub fn try_register_hidden(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> {
        self.try_register_with_exposure(tool, ToolExposure::Hidden)
    }

    /// Register a deferred tool that is discoverable but not initially exposed.
    pub fn try_register_deferred(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> {
        self.try_register_with_exposure(tool, ToolExposure::Deferred)
    }

    /// Return definitions for directly exposed tools.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|registered| registered.exposure == ToolExposure::Direct)
            .map(RegisteredTool::definition)
            .collect()
    }

    /// Return registry metadata for a tool.
    pub fn metadata(&self, name: &str) -> Option<ToolMetadata> {
        self.tools
            .iter()
            .find(|registered| registered.definition().name == name)
            .map(RegisteredTool::metadata)
    }

    /// Search deferred tools by simple case-insensitive name/description match.
    pub fn search_deferred(&self, query: &str) -> Vec<ToolDefinition> {
        let query = query.to_ascii_lowercase();
        self.tools
            .iter()
            .filter(|registered| registered.exposure == ToolExposure::Deferred)
            .filter_map(|registered| {
                let definition = registered.definition();
                let haystack = format!(
                    "{}\n{}",
                    definition.name.to_ascii_lowercase(),
                    definition.description.to_ascii_lowercase()
                );
                if query.is_empty() || haystack.contains(&query) {
                    Some(definition)
                } else {
                    None
                }
            })
            .collect()
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
        self.call_raw(name, arguments, capability)
    }

    /// Call a tool and return the typed output contract.
    pub async fn call_output(
        &self,
        name: &str,
        arguments: serde_json::Value,
        capability: &CapabilityToken,
    ) -> Result<ToolOutput, ToolError> {
        let raw = self.call_raw(name, arguments, capability).await?;
        let tool = self
            .tools
            .iter()
            .find(|registered| registered.definition().name == name)
            .ok_or_else(|| ToolError::ExecutionFailed(format!("unknown tool: {name}")))?;
        Ok(tool.tool.output_from_value(raw))
    }

    fn call_raw<'a>(
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

            let tool = match self
                .tools
                .iter()
                .find(|registered| registered.definition().name == name)
            {
                Some(tool) => tool,
                None => {
                    let err = ToolError::ExecutionFailed(format!("unknown tool: {name}"));
                    emit_tool_error_event(name, &err);
                    return Err(err);
                }
            };

            // Tools that own their own hook lifecycle (e.g. memory tools that
            // translate deny verdicts into graceful payload shapes) opt out of
            // the generic before/after wrapping so hooks do not fire twice.
            let tool_owns_hooks = tool.tool.handles_own_hooks();

            // --- BEFORE hook ---
            let effective_args = if let Some(ref pipeline) = self.pipeline
                && !tool_owns_hooks
            {
                let before_ctx = tool.tool.hook_input_payload(name, &arguments).to_string();
                match pipeline.run_before(Operation::ToolCall, &before_ctx) {
                    Ok((verdict, modified_ctx)) => match verdict {
                        simulacra_hooks::Verdict::Continue(_) => {
                            // If the hook modified the context, extract the
                            // updated arguments from it.
                            if let Ok(parsed) = serde_json::from_str::<Value>(&modified_ctx) {
                                tool.tool.arguments_from_hook_input(arguments, &parsed)
                            } else {
                                arguments
                            }
                        }
                        simulacra_hooks::Verdict::Deny(reason) => {
                            let err = ToolError::ExecutionFailed(format!(
                                "hook denied tool call: {reason}"
                            ));
                            emit_tool_error_event(name, &err);
                            return Err(err);
                        }
                        simulacra_hooks::Verdict::Kill(_) => {
                            unreachable!("Kill is returned as Err from run_before")
                        }
                    },
                    Err(e) => {
                        let err = ToolError::ExecutionFailed(format!("hook error: {e}"));
                        emit_tool_error_event(name, &err);
                        return Err(err);
                    }
                }
            } else {
                arguments
            };

            // --- EXECUTE ---
            let result = tool.tool.call(effective_args.clone(), capability).await;

            // --- AFTER hook ---
            match result {
                Ok(result_value) => {
                    if let Some(ref pipeline) = self.pipeline
                        && !tool_owns_hooks
                    {
                        let after_ctx = tool
                            .tool
                            .hook_output_payload(name, &effective_args, &result_value)
                            .to_string();
                        match pipeline.run_after(Operation::ToolCall, &after_ctx) {
                            Ok((_verdict, modified_ctx)) => {
                                // Extract possibly-modified result
                                let final_result = if let Ok(parsed) =
                                    serde_json::from_str::<Value>(&modified_ctx)
                                {
                                    tool.tool.result_from_hook_output(result_value, &parsed)
                                } else {
                                    result_value
                                };
                                let output = tool.tool.output_from_value(final_result.clone());
                                emit_tool_result_event(name, &output);
                                Ok(final_result)
                            }
                            Err(e) => {
                                let err = ToolError::ExecutionFailed(format!("hook error: {e}"));
                                emit_tool_error_event(name, &err);
                                Err(err)
                            }
                        }
                    } else {
                        let output = tool.tool.output_from_value(result_value.clone());
                        emit_tool_result_event(name, &output);
                        Ok(result_value)
                    }
                }
                Err(err) => {
                    emit_tool_error_event(name, &err);
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
