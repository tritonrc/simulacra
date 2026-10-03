//! `OpenAiConfig`/`with_config` (no env vars) and endpoint joining over a
//! real HTTP round trip. Unit coverage of the joining rule lives in
//! `src/openai/endpoint.rs`.

mod support;

use simulacra_provider::{AuthStyle, OpenAiConfig, OpenAiProvider, OutputCapField, Provider};
use support::*;

#[tokio::test(flavor = "current_thread")]
async fn with_config_bearer_sends_to_the_joined_url_with_authorization_header() {
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let config = OpenAiConfig::new(
        format!("{}/api/v1", fake.base_url()),
        "sk-test",
        "gpt-4o-mini",
    );
    let provider = OpenAiProvider::with_config(config);

    let mut budget = fresh_budget();
    provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("fake upstream should answer 200");

    let request = fake.first_request();
    assert_eq!(request.path, "/api/v1/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer sk-test")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn with_config_named_header_auth_style_replaces_the_authorization_header() {
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let config = OpenAiConfig::new(fake.base_url(), "azure-secret", "gpt-4o")
        .with_auth_style(AuthStyle::Header("api-key".to_owned()));
    let provider = OpenAiProvider::with_config(config);

    let mut budget = fresh_budget();
    provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("fake upstream should answer 200");

    let request = fake.first_request();
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.headers.get("api-key").map(String::as_str),
        Some("azure-secret")
    );
    assert!(!request.headers.contains_key("authorization"));
}

#[tokio::test(flavor = "current_thread")]
async fn with_config_extra_headers_ride_alongside_auth() {
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let config = OpenAiConfig::new(fake.base_url(), "key", "model")
        .with_extra_headers(vec![("x-app".to_owned(), "simulacra".to_owned())]);
    let provider = OpenAiProvider::with_config(config);

    let mut budget = fresh_budget();
    provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("fake upstream should answer 200");

    let request = fake.first_request();
    assert_eq!(
        request.headers.get("x-app").map(String::as_str),
        Some("simulacra")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn with_config_max_tokens_field_replaces_max_completion_tokens_on_the_wire() {
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let config = OpenAiConfig::new(fake.base_url(), "key", "model")
        .with_output_cap_field(OutputCapField::MaxTokens);
    let provider = OpenAiProvider::with_config(config);

    let mut budget = fresh_budget();
    provider
        .chat(&[user_message("hi")], &[], &mut budget)
        .await
        .expect("fake upstream should answer 200");

    let request = fake.first_request();
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("request body should be JSON");
    assert!(body.get("max_tokens").is_some());
    assert!(body.get("max_completion_tokens").is_none());
}

// ── Malformed tool-call arguments fail the turn end to end ─────────
