//! Where a gateway credential could escape: a redirect to another host, an
//! upstream error body that echoes it, and a base URL that carries one.

mod support;

use serde_json::json;
use simulacra_provider::{AuthStyle, OpenAiConfig, OpenAiProvider, Provider};
use support::*;

#[tokio::test(flavor = "current_thread")]
async fn a_redirect_is_not_followed_so_a_header_key_never_reaches_another_host() {
    let elsewhere = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let gateway = FakeHttpClient::new(CannedResponse {
        status: 307,
        headers: vec![(
            "location".into(),
            format!("{}/v1/chat/completions", elsewhere.base_url()),
        )],
        body: Vec::new(),
    });
    let config = OpenAiConfig::new(format!("{}/v1", gateway.base_url()), "sk-redirect", "m")
        .with_auth_style(AuthStyle::Header("api-key".into()));
    let provider = OpenAiProvider::with_config(config);

    let result = provider
        .chat(&[user_message("hi")], &[], &mut fresh_budget())
        .await;

    assert!(result.is_err(), "a 3xx must surface as an error");
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
    assert!(
        elsewhere.requests.lock().unwrap().is_empty(),
        "the redirect target must never be contacted"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_upstream_error_that_echoes_the_key_does_not_carry_it_on() {
    let gateway = FakeHttpClient::new(CannedResponse::json(
        401,
        json!({ "error": { "message": "Invalid api-key: sk-echoed-secret" } }),
    ));
    let config = OpenAiConfig::new(gateway.base_url(), "sk-echoed-secret", "m");
    let provider = OpenAiProvider::with_config(config);

    let err = provider
        .chat(&[user_message("hi")], &[], &mut fresh_budget())
        .await
        .expect_err("401 is an error");

    for rendered in [err.to_string(), format!("{err:?}")] {
        assert!(!rendered.contains("sk-echoed-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_large_non_json_error_body_is_bounded() {
    let gateway = FakeHttpClient::new(CannedResponse {
        status: 500,
        headers: vec![("content-type".into(), "text/plain".into())],
        body: "x".repeat(20_000).into_bytes(),
    });
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(gateway.base_url(), "k", "m"));

    let err = provider
        .chat(&[user_message("hi")], &[], &mut fresh_budget())
        .await
        .expect_err("500 is an error");

    assert!(
        err.to_string().len() < 1_000,
        "{} bytes",
        err.to_string().len()
    );
}

#[test]
fn debug_of_a_config_hides_credentials_in_the_base_url() {
    let config = OpenAiConfig::new(
        "https://user:pw-in-url@gw.example/v1?token=tok-in-query",
        "sk-key",
        "m",
    );

    let rendered = format!("{config:?}");

    for secret in ["pw-in-url", "tok-in-query", "sk-key"] {
        assert!(!rendered.contains(secret), "{rendered}");
    }
    assert!(rendered.contains("gw.example/v1"), "{rendered}");
}
#[tokio::test(flavor = "current_thread")]
async fn a_base_url_carrying_credentials_is_refused_before_anything_is_sent() {
    let gateway = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let base = gateway
        .base_url()
        .replace("http://", "http://user:pw-in-url@")
        + "/v1";
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(base, "k", "m"));

    let err = provider
        .chat(&[user_message("hi")], &[], &mut fresh_budget())
        .await
        .expect_err("credentials in the URL are refused");

    assert!(!format!("{err:?}").contains("pw-in-url"));
    assert!(gateway.requests.lock().unwrap().is_empty());
}
