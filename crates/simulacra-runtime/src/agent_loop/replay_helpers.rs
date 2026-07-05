use super::*;

/// Return the variant name of a JournalEntryKind for telemetry.
pub(super) fn entry_kind_name(kind: &JournalEntryKind) -> &'static str {
    match kind {
        JournalEntryKind::TurnStart => "TurnStart",
        JournalEntryKind::LlmRequest { .. } => "LlmRequest",
        JournalEntryKind::LlmResponse { .. } => "LlmResponse",
        JournalEntryKind::ToolCall { .. } => "ToolCall",
        JournalEntryKind::ToolResult { .. } => "ToolResult",
        JournalEntryKind::ShellCommand { .. } => "ShellCommand",
        JournalEntryKind::CodeExecution { .. } => "CodeExecution",
        JournalEntryKind::SubAgentSpawned { .. } => "SubAgentSpawned",
        JournalEntryKind::SubAgentCompleted { .. } => "SubAgentCompleted",
        JournalEntryKind::FileWrite { .. } => "FileWrite",
        JournalEntryKind::FileDelete { .. } => "FileDelete",
        JournalEntryKind::FileMove { .. } => "FileMove",
        JournalEntryKind::HttpRequest { .. } => "HttpRequest",
        JournalEntryKind::Checkpoint { .. } => "Checkpoint",
        JournalEntryKind::HookDenial { .. } => "HookDenial",
        JournalEntryKind::HookKill { .. } => "HookKill",
    }
}

pub(super) fn replay_entries_match(expected: &JournalEntryKind, actual: &JournalEntryKind) -> bool {
    match (expected, actual) {
        (JournalEntryKind::TurnStart, JournalEntryKind::TurnStart) => true,
        (
            JournalEntryKind::LlmRequest {
                model: expected_model,
                message_count: expected_count,
            },
            JournalEntryKind::LlmRequest {
                model: actual_model,
                message_count: actual_count,
            },
        ) => expected_model == actual_model && expected_count == actual_count,
        (
            JournalEntryKind::ToolCall {
                tool_call_id: expected_id,

                tool_name: expected_tool,
                arguments: expected_args,
            },
            JournalEntryKind::ToolCall {
                tool_call_id: actual_id,

                tool_name: actual_tool,
                arguments: actual_args,
            },
        ) => {
            let ids_match = match (expected_id, actual_id) {
                (Some(expected), Some(actual)) => expected == actual,
                // Backward compatibility: old journals did not record ids.
                (_, None) => true,
                (None, Some(_)) => true,
            };
            ids_match && expected_tool == actual_tool && expected_args == actual_args
        }
        _ => false,
    }
}

pub(super) fn describe_replay_entry(kind: &JournalEntryKind) -> String {
    match kind {
        JournalEntryKind::LlmRequest {
            model,
            message_count,
        } => format!("LlmRequest(model={model}, message_count={message_count})"),
        JournalEntryKind::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        } => format!(
            "ToolCall(tool_call_id={}, tool_name={tool_name}, arguments={arguments})",
            tool_call_id.as_deref().unwrap_or("<legacy>")
        ),
        other => entry_kind_name(other).to_string(),
    }
}

/// Extract a ProviderResponse from a replayed LlmResponse journal entry.
pub(super) fn replay_llm_response(
    kind: &JournalEntryKind,
) -> Result<simulacra_types::ProviderResponse, RuntimeError> {
    if let JournalEntryKind::LlmResponse {
        model,
        token_usage,
        finish_reason,
        assistant_message,
    } = kind
    {
        let fr = match finish_reason.as_str() {
            "EndTurn" => simulacra_types::FinishReason::EndTurn,
            "ToolUse" => simulacra_types::FinishReason::ToolUse,
            "MaxTokens" => simulacra_types::FinishReason::MaxTokens,
            "StopSequence" => simulacra_types::FinishReason::StopSequence,
            _ => simulacra_types::FinishReason::EndTurn,
        };

        // Use the stored assistant message (with tool_calls) if available,
        // otherwise reconstruct a minimal message (backwards compat with older journals).
        let message = assistant_message.clone().unwrap_or_else(|| Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        });

        Ok(simulacra_types::ProviderResponse {
            message,
            token_usage: token_usage.clone(),
            finish_reason: fr,
            provider_response_id: None,
            model: model.clone(),
        })
    } else {
        let actual_kind = entry_kind_name(kind);
        tracing::error!(
            expected = "LlmResponse",
            actual = actual_kind,
            "replay divergence: expected LlmResponse but got {actual_kind}"
        );
        Err(RuntimeError::Journal(
            simulacra_types::JournalError::Storage(format!(
                "expected LlmResponse during replay, got {kind:?}"
            )),
        ))
    }
}

/// Extract tool result from a replayed ToolResult journal entry.
pub(super) fn replay_tool_result(kind: &JournalEntryKind) -> Result<(String, bool), RuntimeError> {
    if let JournalEntryKind::ToolResult {
        content, is_error, ..
    } = kind
    {
        Ok((content.clone(), *is_error))
    } else {
        Err(RuntimeError::Journal(
            simulacra_types::JournalError::Storage(format!(
                "expected ToolResult during replay, got {kind:?}"
            )),
        ))
    }
}
