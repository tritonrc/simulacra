//! Finish-reason mapping and malformed tool-call arguments end to end.
//! Unit coverage of the table lives in `src/openai/finish.rs`.

mod support;

use serde_json::json;
use simulacra_provider::{FinishReason, OpenAiConfig, OpenAiProvider, Provider, ProviderError};
use simulacra_types::StreamingProvider;
use support::*;

fn tool_call_response_json(arguments: &str) -> serde_json::Value {
    json!({
        "id": "chatcmpl_bad_tool",
        "object": "chat.completion",
        "created": 1_726_000_001_u64,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": arguments }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    })
}

#[tokio::test(flavor = "current_thread")]
async fn non_streaming_malformed_tool_arguments_fail_the_turn() {
    let fake = FakeHttpClient::new(CannedResponse::json(
        200,
        tool_call_response_json("{not json"),
    ));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "key", "model"));

    let mut budget = fresh_budget();
    let err = provider
        .chat(&[user_message("weather")], &[], &mut budget)
        .await
        .expect_err("malformed tool arguments must fail the turn");

    match err {
        ProviderError::MalformedToolInput { tool_name, .. } => {
            assert_eq!(tool_name, "get_weather");
        }
        other => panic!("expected MalformedToolInput, got: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_malformed_tool_arguments_fail_the_turn() {
    let body = concat!(
        "data: {\"id\":\"s\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{not\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    )
    .as_bytes()
    .to_vec();
    let fake = FakeHttpClient::new(CannedResponse::sse(body));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "key", "model"));

    let mut budget = fresh_budget();
    let err = StreamingProvider::chat_stream(
        &provider,
        &[user_message("weather")],
        &[],
        &mut budget,
        &NullStreamSink,
    )
    .await
    .expect_err("malformed streamed tool arguments must fail the turn");

    assert!(matches!(err, ProviderError::MalformedToolInput { .. }));
}

// ── Finish-reason mapping end to end ───────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn content_filter_finish_reason_surfaces_as_refusal() {
    let fake = FakeHttpClient::new(CannedResponse::json(
        200,
        success_response_json("content_filter"),
    ));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "key", "model"));

    let mut budget = fresh_budget();
    let resp = provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("content_filter is not a transport error");

    assert_eq!(resp.finish_reason, FinishReason::Refusal);
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_function_call_finish_reason_maps_to_tool_use() {
    let fake = FakeHttpClient::new(CannedResponse::json(
        200,
        success_response_json("function_call"),
    ));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "key", "model"));

    let mut budget = fresh_budget();
    let resp = provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("legacy function_call is not a transport error");

    assert_eq!(resp.finish_reason, FinishReason::ToolUse);
}

#[tokio::test(flavor = "current_thread")]
async fn non_streaming_refusal_field_wins_over_a_stop_finish_reason() {
    let mut body = success_response_json("stop");
    body["choices"][0]["message"]["refusal"] = json!("I can't help with that.");
    let fake = FakeHttpClient::new(CannedResponse::json(200, body));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "key", "model"));

    let mut budget = fresh_budget();
    let resp = provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("a refusal field is not a transport error");

    assert_eq!(resp.finish_reason, FinishReason::Refusal);
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_refusal_delta_wins_over_a_stop_finish_reason() {
    let body = concat!(
        "data: {\"id\":\"s\",\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"no\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    )
    .as_bytes()
    .to_vec();
    let fake = FakeHttpClient::new(CannedResponse::sse(body));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "key", "model"));

    let mut budget = fresh_budget();
    let resp = StreamingProvider::chat_stream(
        &provider,
        &[user_message("hi")],
        &[],
        &mut budget,
        &NullStreamSink,
    )
    .await
    .expect("a refusal delta is not a transport error");

    assert_eq!(resp.finish_reason, FinishReason::Refusal);
}

#[tokio::test(flavor = "current_thread")]
async fn a_missing_finish_reason_on_the_wire_is_other_missing() {
    let mut body = success_response_json("stop");
    body["choices"][0]
        .as_object_mut()
        .expect("choice is an object")
        .remove("finish_reason");
    let fake = FakeHttpClient::new(CannedResponse::json(200, body));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "key", "model"));

    let mut budget = fresh_budget();
    let resp = provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("a missing finish_reason is not a transport error");

    assert_eq!(
        resp.finish_reason,
        FinishReason::Other("missing".to_string())
    );
}
