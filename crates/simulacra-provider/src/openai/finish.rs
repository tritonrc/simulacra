//! Mapping an OpenAI-compatible `finish_reason` (plus a `refusal` field) to
//! `simulacra_types::FinishReason`, and failing a malformed tool-call
//! argument payload instead of silently substituting `{}`.

use simulacra_types::{FinishReason, ProviderError};

use crate::finish_reason::sanitize_other_reason;

/// Map a chat-completions `finish_reason` string (and whether a non-empty
/// `refusal` field accompanied it) to our `FinishReason`. A refusal — either
/// `finish_reason: "content_filter"` or a populated `refusal` field — always
/// wins: the generation was blocked, so nothing downstream should treat it
/// as a normal stop. An unrecognized or missing value carries its raw string
/// (or `"missing"`) in `Other` rather than silently becoming `EndTurn`,
/// mirroring how the Anthropic client handles a missing `stop_reason`.
pub(super) fn map_finish_reason(finish_reason: Option<&str>, refusal_seen: bool) -> FinishReason {
    if refusal_seen {
        return FinishReason::Refusal;
    }
    match finish_reason {
        Some("stop") => FinishReason::EndTurn,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolUse,
        Some("length") => FinishReason::MaxTokens,
        Some("content_filter") => FinishReason::Refusal,
        Some(other) => FinishReason::Other(sanitize_other_reason(other)),
        None => FinishReason::Other("missing".to_string()),
    }
}

/// Parse one tool call's accumulated argument text. An empty payload (a
/// tool that takes no arguments) becomes `{}`; anything else that fails to
/// parse must not silently become `{}` too — executing a tool call with
/// fabricated empty arguments is worse than failing the turn visibly. Never
/// logs the argument text itself, only its length.
pub(super) fn parse_tool_call_arguments(
    tool_name: &str,
    raw_args: &str,
) -> Result<serde_json::Value, ProviderError> {
    if raw_args.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(raw_args).map_err(|e| {
        let raw_len = raw_args.len();
        tracing::warn!(
            tool_name,
            raw_args_len = raw_len,
            error = %e,
            "tool call arguments failed to parse as JSON; failing the turn"
        );
        ProviderError::MalformedToolInput {
            tool_name: tool_name.to_owned(),
            raw_len,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_finish_reasons_map_as_the_api_documents() {
        assert_eq!(
            map_finish_reason(Some("stop"), false),
            FinishReason::EndTurn
        );
        assert_eq!(
            map_finish_reason(Some("tool_calls"), false),
            FinishReason::ToolUse
        );
        assert_eq!(
            map_finish_reason(Some("function_call"), false),
            FinishReason::ToolUse
        );
        assert_eq!(
            map_finish_reason(Some("length"), false),
            FinishReason::MaxTokens
        );
        assert_eq!(
            map_finish_reason(Some("content_filter"), false),
            FinishReason::Refusal
        );
    }

    #[test]
    fn a_refusal_field_wins_over_any_finish_reason() {
        assert_eq!(map_finish_reason(Some("stop"), true), FinishReason::Refusal);
        assert_eq!(
            map_finish_reason(Some("tool_calls"), true),
            FinishReason::Refusal
        );
        assert_eq!(map_finish_reason(None, true), FinishReason::Refusal);
    }

    #[test]
    fn missing_finish_reason_is_other_missing_not_end_turn() {
        assert_eq!(
            map_finish_reason(None, false),
            FinishReason::Other("missing".to_string())
        );
    }

    #[test]
    fn unknown_finish_reason_is_sanitized_other_not_end_turn() {
        assert_eq!(
            map_finish_reason(Some("some_future_reason"), false),
            FinishReason::Other("some_future_reason".to_string())
        );
        assert_eq!(
            map_finish_reason(Some("weird\"va\\lue"), false),
            FinishReason::Other("weird_va_lue".to_string())
        );
    }

    #[test]
    fn empty_arguments_parse_as_an_empty_object() {
        let value = parse_tool_call_arguments("get_weather", "").unwrap();
        assert_eq!(value, serde_json::json!({}));
        let value = parse_tool_call_arguments("get_weather", "   ").unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn valid_json_arguments_parse_through() {
        let value = parse_tool_call_arguments("get_weather", "{\"location\":\"SF\"}").unwrap();
        assert_eq!(value, serde_json::json!({"location": "SF"}));
    }

    #[test]
    fn malformed_arguments_fail_the_turn_instead_of_substituting_empty_object() {
        let err = parse_tool_call_arguments("get_weather", "{not json").unwrap_err();
        match err {
            ProviderError::MalformedToolInput { tool_name, raw_len } => {
                assert_eq!(tool_name, "get_weather");
                assert_eq!(raw_len, "{not json".len());
            }
            other => panic!("expected MalformedToolInput, got: {other:?}"),
        }
    }
}
