//! Fetch-path guard tests: per-hop redirect authorization, https-only
//! requests, bounded hop count, and the module fetcher inheriting the same
//! behavior — driven through `fetch_http_inner` with a scripted fake
//! `HttpClient` that records every request.

use crate::SandboxError;
use crate::http_fetch_guard_support::*;
use crate::test_support::ScriptedHttpClient;
use simulacra_http::HttpResponse;

#[test]
fn granted_https_fetch_without_redirect_succeeds_and_disables_client_redirects() {
    let client = ScriptedHttpClient::default().with(
        "https://allowed.example/data",
        ok("https://allowed.example/data", "ok"),
    );
    let capability = capability(&["allowed.example"]);

    let response = fetch_with(&client, &capability, "https://allowed.example/data")
        .expect("granted https fetch should succeed");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"ok");
    let requests = client.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://allowed.example/data");
    assert_all_no_follow(&client);
}

#[test]
fn initial_http_url_to_granted_host_is_rejected_before_any_request() {
    let client = ScriptedHttpClient::default().with(
        "http://allowed.example/data",
        ok("http://allowed.example/data", "cleartext"),
    );
    let capability = capability(&["allowed.example"]);

    let err = fetch_with(&client, &capability, "http://allowed.example/data")
        .expect_err("initial http URL must be rejected");

    assert!(
        err.to_string().contains("https"),
        "expected an https-only rejection naming the scheme, got {err:?}"
    );
    assert!(
        !err.to_string().contains("http://allowed.example/data"),
        "the rejection must be a scheme denial, not a URL-bearing network error: {err:?}"
    );
    assert_request_sequence(&client, &[]);
}

#[test]
fn redirect_to_ungranted_host_denies_before_destination_request() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://allowed.example/start",
            redirect(
                "https://allowed.example/start",
                "https://evil.example/payload",
            ),
        )
        .with(
            "https://evil.example/payload",
            ok("https://evil.example/payload", "must not fetch"),
        );
    let capability = capability(&["allowed.example"]);

    let err = fetch_with(&client, &capability, "https://allowed.example/start")
        .expect_err("redirect to ungranted host must be denied");

    assert!(
        matches!(err, SandboxError::CapabilityDenied(_)),
        "denial must be the capability denial, got {err:?}"
    );
    assert_request_sequence(&client, &["https://allowed.example/start"]);
    assert_all_no_follow(&client);
}

#[test]
fn redirect_to_http_url_is_denied_without_destination_request_even_when_host_is_granted() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://allowed.example/start",
            redirect(
                "https://allowed.example/start",
                "http://allowed.example/plain",
            ),
        )
        .with(
            "http://allowed.example/plain",
            ok("http://allowed.example/plain", "must not fetch"),
        );
    let capability = capability(&["allowed.example"]);

    let err = fetch_with(&client, &capability, "https://allowed.example/start")
        .expect_err("redirect to http must be denied");

    assert!(
        err.to_string().contains("https"),
        "expected an https-only redirect rejection naming the scheme, got {err:?}"
    );
    assert!(
        !err.to_string().contains("http://allowed.example/plain"),
        "the rejection must be a scheme denial, not a URL-bearing network error: {err:?}"
    );
    assert_request_sequence(&client, &["https://allowed.example/start"]);
    assert_all_no_follow(&client);
}

#[test]
fn https_redirect_chain_across_granted_hosts_resolves_to_final_body() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://a.example/start",
            redirect("https://a.example/start", "https://b.example/next"),
        )
        .with(
            "https://b.example/next",
            redirect("https://b.example/next", "https://c.example/final"),
        )
        .with(
            "https://c.example/final",
            ok("https://c.example/final", "done"),
        );
    let capability = capability(&["a.example", "b.example", "c.example"]);

    let response = fetch_with(&client, &capability, "https://a.example/start")
        .expect("all granted https redirects should resolve");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"done");
    assert!(
        response.redirected,
        "a response that followed redirects must say so"
    );
    assert_eq!(response.url, "https://c.example/final");
    assert_request_sequence(
        &client,
        &[
            "https://a.example/start",
            "https://b.example/next",
            "https://c.example/final",
        ],
    );
    assert_all_no_follow(&client);
}

#[test]
fn sixth_redirect_is_rejected_before_requesting_the_sixth_destination() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://allowed.example/r0",
            redirect("https://allowed.example/r0", "https://allowed.example/r1"),
        )
        .with(
            "https://allowed.example/r1",
            redirect("https://allowed.example/r1", "https://allowed.example/r2"),
        )
        .with(
            "https://allowed.example/r2",
            redirect("https://allowed.example/r2", "https://allowed.example/r3"),
        )
        .with(
            "https://allowed.example/r3",
            redirect("https://allowed.example/r3", "https://allowed.example/r4"),
        )
        .with(
            "https://allowed.example/r4",
            redirect("https://allowed.example/r4", "https://allowed.example/r5"),
        )
        .with(
            "https://allowed.example/r5",
            redirect("https://allowed.example/r5", "https://allowed.example/r6"),
        )
        .with(
            "https://allowed.example/r6",
            ok("https://allowed.example/r6", "too far"),
        );
    let capability = capability(&["allowed.example"]);

    let err = fetch_with(&client, &capability, "https://allowed.example/r0")
        .expect_err("sixth redirect must be rejected");

    assert!(
        err.to_string().contains("redirect"),
        "expected redirect-limit error, got {err:?}"
    );
    assert_request_sequence(
        &client,
        &[
            "https://allowed.example/r0",
            "https://allowed.example/r1",
            "https://allowed.example/r2",
            "https://allowed.example/r3",
            "https://allowed.example/r4",
            "https://allowed.example/r5",
        ],
    );
    assert_all_no_follow(&client);
}

/// The boundary the hop cap defines: exactly five redirects, then a 200 —
/// this must succeed, so a blanket 3xx rejection cannot pass the suite.
#[test]
fn exactly_five_redirects_resolve_to_the_final_body() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://allowed.example/r0",
            redirect("https://allowed.example/r0", "https://allowed.example/r1"),
        )
        .with(
            "https://allowed.example/r1",
            redirect("https://allowed.example/r1", "https://allowed.example/r2"),
        )
        .with(
            "https://allowed.example/r2",
            redirect("https://allowed.example/r2", "https://allowed.example/r3"),
        )
        .with(
            "https://allowed.example/r3",
            redirect("https://allowed.example/r3", "https://allowed.example/r4"),
        )
        .with(
            "https://allowed.example/r4",
            redirect("https://allowed.example/r4", "https://allowed.example/r5"),
        )
        .with(
            "https://allowed.example/r5",
            ok("https://allowed.example/r5", "just enough"),
        );
    let capability = capability(&["allowed.example"]);

    let response = fetch_with(&client, &capability, "https://allowed.example/r0")
        .expect("five redirects are within the hop budget");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"just enough");
    assert_all_no_follow(&client);
}

/// A final-hop 3xx with no `Location` is returned as-is (a terminal
/// redirect response is a response, not an instruction).
#[test]
fn redirect_response_without_location_is_returned_unchanged() {
    let terminal = HttpResponse {
        status: 304,
        status_text: "Not Modified".into(),
        headers: vec![],
        body: vec![],
        url: "https://allowed.example/here".into(),
        redirected: false,
    };
    let client = ScriptedHttpClient::default().with("https://allowed.example/here", terminal);
    let capability = capability(&["allowed.example"]);

    let response = fetch_with(&client, &capability, "https://allowed.example/here")
        .expect("a 3xx without Location is a terminal response, not an error");

    assert_eq!(response.status, 304);
    assert_request_sequence(&client, &["https://allowed.example/here"]);
    assert_all_no_follow(&client);
}

/// Relative `Location` resolution happens against the CURRENT hop's URL,
/// not the initial request URL: an absolute redirect first moves the chain
/// to a different host and directory, and the relative destination is
/// resolved from there.
#[test]
fn relative_location_resolves_against_the_current_hops_url() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://a.example/pkg/index.js",
            redirect(
                "https://a.example/pkg/index.js",
                "https://b.example/deep/dir/next.js",
            ),
        )
        .with(
            "https://b.example/deep/dir/next.js",
            redirect("https://b.example/deep/dir/next.js", "../shared/util.js"),
        )
        .with(
            "https://b.example/deep/shared/util.js",
            ok("https://b.example/deep/shared/util.js", "relative ok"),
        );
    let capability = capability(&["a.example", "b.example"]);

    let response = fetch_with(&client, &capability, "https://a.example/pkg/index.js")
        .expect("relative redirect resolved from the current hop should resolve");

    assert_eq!(response.body, b"relative ok");
    assert_request_sequence(
        &client,
        &[
            "https://a.example/pkg/index.js",
            "https://b.example/deep/dir/next.js",
            "https://b.example/deep/shared/util.js",
        ],
    );
    assert_all_no_follow(&client);
}

/// Credentials must never ride a redirect: a grant to reach host B does not
/// authorize forwarding host A's Authorization/Cookie to it.
#[test]
fn credential_headers_are_stripped_from_follow_on_requests() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://a.example/auth",
            redirect("https://a.example/auth", "https://b.example/next"),
        )
        .with("https://b.example/next", ok("https://b.example/next", "ok"));
    let capability = capability(&["a.example", "b.example"]);
    let headers = vec![
        ("Authorization".to_string(), "Bearer a-secret".to_string()),
        ("Cookie".to_string(), "session=a-secret".to_string()),
        (
            "Proxy-Authorization".to_string(),
            "Basic a-secret".to_string(),
        ),
        ("X-Custom".to_string(), "keep-me".to_string()),
    ];

    let response = fetch_full(
        &client,
        &capability,
        "https://a.example/auth",
        "GET",
        &headers,
    )
    .expect("the redirected fetch should resolve");

    assert_eq!(response.status, 200);
    let follow_on = &client.requests()[1];
    for credential in ["authorization", "cookie", "proxy-authorization"] {
        assert!(
            !follow_on
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(credential)),
            "{credential} must be stripped from the follow-on request: {:?}",
            follow_on.headers
        );
    }
    assert!(
        follow_on
            .headers
            .iter()
            .any(|(name, value)| name == "X-Custom" && value == "keep-me"),
        "non-credential headers must survive the redirect: {:?}",
        follow_on.headers
    );
    assert_all_no_follow(&client);
}

/// Cleartext is allowed to loopback only: an http:// URL to `localhost`
/// under a loopback grant succeeds, and a redirect to a loopback http://
/// destination is followed when granted.
#[test]
fn loopback_cleartext_is_admitted_when_granted() {
    let direct = ScriptedHttpClient::default()
        .with("http://localhost/svc", ok("http://localhost/svc", "local"));
    let loopback_caps = capability(&["localhost"]);

    let response = fetch_with(&direct, &loopback_caps, "http://localhost/svc")
        .expect("loopback cleartext under a loopback grant must succeed");
    assert_eq!(response.body, b"local");

    let redirected = ScriptedHttpClient::default()
        .with(
            "https://allowed.example/start",
            redirect("https://allowed.example/start", "http://127.0.0.1:9/x"),
        )
        .with(
            "http://127.0.0.1:9/x",
            ok("http://127.0.0.1:9/x", "loopback"),
        );
    let both_caps = capability(&["allowed.example", "127.0.0.1"]);

    let response = fetch_with(&redirected, &both_caps, "https://allowed.example/start")
        .expect("a granted loopback redirect destination must be followed");
    assert_eq!(response.body, b"loopback");
    assert_all_no_follow(&redirected);
}

/// Non-GET/HEAD methods do not auto-follow: a 3xx answer to a POST is a
/// terminal response, not an instruction.
#[test]
fn post_redirect_response_is_terminal_not_followed() {
    let client = ScriptedHttpClient::default().with(
        "https://allowed.example/submit",
        redirect(
            "https://allowed.example/submit",
            "https://allowed.example/other",
        ),
    );
    let capability = capability(&["allowed.example"]);

    let response = fetch_full(
        &client,
        &capability,
        "https://allowed.example/submit",
        "POST",
        &[],
    )
    .expect("a POST answered by a 3xx returns the response, not an error");

    assert_eq!(response.status, 302);
    assert_request_sequence(&client, &["https://allowed.example/submit"]);
    assert_all_no_follow(&client);
}

/// A scheme-relative `Location` (`//host/path`) resolves to `https://host`
/// and then goes through host authorization like any other destination.
#[test]
fn scheme_relative_location_is_resolved_then_authorized() {
    let client = ScriptedHttpClient::default()
        .with(
            "https://allowed.example/start",
            redirect("https://allowed.example/start", "//evil.example/payload"),
        )
        .with(
            "https://evil.example/payload",
            ok("https://evil.example/payload", "must not fetch"),
        );
    let capability = capability(&["allowed.example"]);

    let err = fetch_with(&client, &capability, "https://allowed.example/start")
        .expect_err("scheme-relative redirect to an ungranted host must be denied");

    assert!(
        matches!(err, SandboxError::CapabilityDenied(_)),
        "denial must be the capability denial after resolution, got {err:?}"
    );
    assert_request_sequence(&client, &["https://allowed.example/start"]);
    assert_all_no_follow(&client);
}
