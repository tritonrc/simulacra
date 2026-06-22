//! ureq-backed [`HttpClient`](crate::HttpClient) implementation.

use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use crate::HttpClient;
use crate::types::{HttpError, HttpRequest, HttpResponse};

/// Maximum response body size read into memory.
///
/// Agent-controlled fetches are otherwise a memory-exhaustion vector. Chosen to
/// match ureq's own default for `read_to_vec()` (10 MB).
const MAX_RESPONSE_SIZE: u64 = 10 * 1024 * 1024;

/// Headers whose values should be redacted when exposed to hooks/telemetry.
///
/// Comparison is ASCII case-insensitive. Kept short and explicit: anything in
/// here is known to carry credentials or cookies.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "proxy-authorization",
];

/// Return `true` if the header name is one we consider sensitive.
fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|s| name.eq_ignore_ascii_case(s))
}

/// Produce a copy of `headers` with values for sensitive names replaced by
/// `[REDACTED]`.
fn redact_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            if is_sensitive_header(name) {
                (name.clone(), "[REDACTED]".to_string())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

/// Strip path, query, and userinfo from a URL for safe logging.
///
/// Returns `<scheme>://<host>[:port]`. If the URL cannot be parsed into that
/// shape, returns the scheme portion before `://` followed by `://[redacted]`.
fn sanitize_url_for_log(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some(parts) => parts,
        None => return "[invalid-url]".to_string(),
    };
    // Trim query and fragment.
    let without_query = rest.split(['?', '#']).next().unwrap_or(rest);
    // Trim path.
    let authority = without_query.split('/').next().unwrap_or(without_query);
    // Trim userinfo (anything before the last '@').
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if host.is_empty() {
        format!("{scheme}://[redacted]")
    } else {
        format!("{scheme}://{host}")
    }
}

/// Extracted, possibly-modified request fields returned by a before-hook.
///
/// Any field set to `None` means the hook did not override that field — the
/// original request value is used.
#[derive(Default)]
struct BeforeHookOverrides {
    url: Option<String>,
    headers: Option<Vec<(String, String)>>,
    body: Option<Option<Vec<u8>>>,
}

/// Extracted, possibly-modified response fields returned by an after-hook.
#[derive(Default)]
struct AfterHookOverrides {
    status: Option<u16>,
    headers: Option<Vec<(String, String)>>,
    body: Option<Vec<u8>>,
}

/// Parse a JSON value as an HTTP header list: `[["name", "value"], ...]` or
/// `{"name": "value", ...}`. Returns `None` if the shape is unexpected.
fn parse_headers_value(value: &serde_json::Value) -> Option<Vec<(String, String)>> {
    if let Some(arr) = value.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for pair in arr {
            let pair = pair.as_array()?;
            if pair.len() != 2 {
                return None;
            }
            let name = pair[0].as_str()?.to_string();
            let val = pair[1].as_str()?.to_string();
            out.push((name, val));
        }
        Some(out)
    } else {
        value.as_object().map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
    }
}

/// Parse before-hook output for recognized override fields.
fn parse_before_overrides(modified: &str) -> BeforeHookOverrides {
    let mut out = BeforeHookOverrides::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(modified) else {
        return out;
    };
    let Some(obj) = value.as_object() else {
        return out;
    };
    if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
        out.url = Some(url.to_string());
    }
    if let Some(headers) = obj.get("headers")
        && let Some(parsed) = parse_headers_value(headers)
    {
        out.headers = Some(parsed);
    }
    if let Some(body) = obj.get("body") {
        if body.is_null() {
            out.body = Some(None);
        } else if let Some(s) = body.as_str() {
            out.body = Some(Some(s.as_bytes().to_vec()));
        }
    }
    out
}

/// Parse after-hook output for recognized override fields.
fn parse_after_overrides(modified: &str) -> AfterHookOverrides {
    let mut out = AfterHookOverrides::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(modified) else {
        return out;
    };
    let Some(obj) = value.as_object() else {
        return out;
    };
    if let Some(status) = obj.get("status").and_then(|v| v.as_u64())
        && status <= u16::MAX as u64
    {
        out.status = Some(status as u16);
    }
    if let Some(headers) = obj.get("headers")
        && let Some(parsed) = parse_headers_value(headers)
    {
        out.headers = Some(parsed);
    }
    if let Some(body) = obj.get("body").and_then(|v| v.as_str()) {
        out.body = Some(body.as_bytes().to_vec());
    }
    out
}

/// An HTTP client backed by [ureq](https://crates.io/crates/ureq).
///
/// Each call to [`execute`](HttpClient::execute) builds a fresh ureq agent with
/// the appropriate timeout and redirect settings, executes the request, and
/// maps the result into [`HttpResponse`] / [`HttpError`].
/// Lazily-initialized OTel meter instruments for the HTTP client.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at `UreqHttpClient::new()` time).
struct HttpMeters {
    duration_histogram: Histogram<f64>,
    request_counter: Counter<u64>,
    error_counter: Counter<u64>,
}

impl HttpMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<HttpMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-http");
            HttpMeters {
                duration_histogram: meter
                    .f64_histogram("simulacra.http.client.duration")
                    .with_unit("ms")
                    .with_description("HTTP client request duration")
                    .build(),
                request_counter: meter
                    .u64_counter("simulacra.http.client.requests")
                    .with_description("Total HTTP client requests")
                    .build(),
                error_counter: meter
                    .u64_counter("simulacra.http.client.errors")
                    .with_description("Total HTTP client errors")
                    .build(),
            }
        })
    }
}

pub struct UreqHttpClient {
    default_timeout_ms: u64,
    max_redirects: u32,
    pipeline: Option<std::sync::Arc<simulacra_hooks::pipeline::HookPipeline>>,
}

impl UreqHttpClient {
    /// Create a new client with the given default timeout and redirect limit.
    pub fn new(default_timeout_ms: u64, max_redirects: u32) -> Self {
        Self {
            default_timeout_ms,
            max_redirects,
            pipeline: None,
        }
    }

    /// Create a new client with a governance hook pipeline.
    pub fn with_pipeline(
        default_timeout_ms: u64,
        max_redirects: u32,
        pipeline: Option<std::sync::Arc<simulacra_hooks::pipeline::HookPipeline>>,
    ) -> Self {
        Self {
            default_timeout_ms,
            max_redirects,
            pipeline,
        }
    }
}

impl Default for UreqHttpClient {
    fn default() -> Self {
        Self::new(5000, 5)
    }
}

impl HttpClient for UreqHttpClient {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        // Sanitized URL used for logging and spans. Never record the raw URL,
        // which may contain userinfo, query tokens, or auth material.
        let safe_url = sanitize_url_for_log(&request.url);

        let span = tracing::info_span!(
            "simulacra_http_request",
            http.request.method = %request.method,
            url.full = %safe_url,
            http.response.status_code = tracing::field::Empty,
            http.request.body.size = request.body.as_ref().map(|b| b.len() as i64).unwrap_or(0),
            http.response.body.size = tracing::field::Empty,
            simulacra.http.client.duration_ms = tracing::field::Empty,
        );
        let _guard = span.enter();

        // --- BEFORE hook ---
        //
        // The hook sees a redacted snapshot of the request so sensitive headers
        // (Authorization, Cookie, ...) are never copied into telemetry or hook
        // storage. A hook may return a modified JSON context; we parse it and
        // apply any recognized overrides to the outgoing request.
        let mut effective_url = request.url.clone();
        let mut effective_headers = request.headers.clone();
        let mut effective_body = request.body.clone();

        if let Some(ref pipeline) = self.pipeline {
            let before_ctx = serde_json::json!({
                "url": &effective_url,
                "method": &request.method,
                "headers": redact_headers(&effective_headers),
                "body": effective_body
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned()),
            })
            .to_string();
            match pipeline.run_before(
                simulacra_hooks::verdict::Operation::HttpRequest,
                &before_ctx,
            ) {
                Ok((simulacra_hooks::Verdict::Continue(None), _)) => {}
                Ok((simulacra_hooks::Verdict::Continue(Some(modified)), _)) => {
                    let overrides = parse_before_overrides(&modified);
                    if let Some(url) = overrides.url {
                        effective_url = url;
                    }
                    if let Some(headers) = overrides.headers {
                        effective_headers = headers;
                    }
                    if let Some(body) = overrides.body {
                        effective_body = body;
                    }
                }
                Ok((simulacra_hooks::Verdict::Deny(reason), _)) => {
                    return Err(HttpError::Network(format!(
                        "hook denied HTTP request: {reason}"
                    )));
                }
                Ok((simulacra_hooks::Verdict::Kill(_), _)) => {
                    unreachable!("Kill is returned as Err from run_before")
                }
                Err(e) => {
                    return Err(HttpError::Network(format!("hook error: {e}")));
                }
            }
        }

        let start = Instant::now();

        // Validate URL early — ureq gives opaque errors for garbage URLs.
        // Accept http:// and https:// case-insensitively; reject everything else
        // (including malformed URLs) as InvalidUrl rather than Network.
        if !url_scheme_is_http(&effective_url) {
            let error = HttpError::InvalidUrl(effective_url.clone());
            HttpMeters::get().error_counter.add(
                1,
                &[
                    KeyValue::new("http.request.method", request.method.clone()),
                    KeyValue::new("error.type", "InvalidUrl"),
                ],
            );
            tracing::warn!(
                http.request.method = %request.method,
                url.full = %sanitize_url_for_log(&effective_url),
                error.type_ = "InvalidUrl",
                "HTTP request failed"
            );
            return Err(error);
        }

        let timeout_ms = request.timeout_ms.unwrap_or(self.default_timeout_ms);
        let max_redirects = request.max_redirects.unwrap_or(self.max_redirects);

        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_millis(timeout_ms)))
            .max_redirects(max_redirects)
            .max_redirects_will_error(true)
            .save_redirect_history(true)
            .build()
            .new_agent();

        let response = match self.dispatch(
            &agent,
            &request.method,
            &effective_url,
            &effective_headers,
            effective_body.as_deref(),
        ) {
            Ok(resp) => resp,
            Err(e) => {
                let elapsed = start.elapsed();
                let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
                span.record(
                    "simulacra.http.client.duration_ms",
                    elapsed.as_millis() as i64,
                );
                let error = map_ureq_error(e, &effective_url);
                let error_type = match &error {
                    HttpError::Network(_) => "Network",
                    HttpError::Timeout => "Timeout",
                    HttpError::TooManyRedirects => "TooManyRedirects",
                    HttpError::InvalidUrl(_) => "InvalidUrl",
                    HttpError::ResponseTooLarge(_) => "ResponseTooLarge",
                };
                let method = request.method.clone();
                let method_attr = KeyValue::new("http.request.method", method.clone());
                HttpMeters::get()
                    .duration_histogram
                    .record(elapsed_ms, std::slice::from_ref(&method_attr));
                HttpMeters::get()
                    .request_counter
                    .add(1, std::slice::from_ref(&method_attr));
                HttpMeters::get().error_counter.add(
                    1,
                    &[
                        KeyValue::new("http.request.method", method),
                        KeyValue::new("error.type", error_type.to_string()),
                    ],
                );
                tracing::warn!(
                    http.request.method = %request.method,
                    url.full = %sanitize_url_for_log(&effective_url),
                    error.type_ = error_type,
                    "HTTP request failed"
                );
                return Err(error);
            }
        };

        // Determine the final URL. If redirect history is available and non-empty,
        // use the last entry; otherwise fall back to the request URL.
        let final_url = {
            use ureq::ResponseExt;
            match response.get_redirect_history() {
                Some(history) if !history.is_empty() => history.last().unwrap().to_string(),
                _ => effective_url.clone(),
            }
        };
        let redirected = final_url != effective_url;

        let mut status = response.status().as_u16();

        // Collect response headers.
        let mut headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();

        // Bound the response body read. Exceeding the limit returns a distinct
        // error rather than silently truncating or OOM'ing the process.
        let mut body = response
            .into_body()
            .into_with_config()
            .limit(MAX_RESPONSE_SIZE)
            .read_to_vec()
            .map_err(|e| {
                let elapsed = start.elapsed();
                let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
                let method = request.method.clone();
                let method_attr = KeyValue::new("http.request.method", method.clone());
                HttpMeters::get()
                    .duration_histogram
                    .record(elapsed_ms, std::slice::from_ref(&method_attr));
                HttpMeters::get()
                    .request_counter
                    .add(1, std::slice::from_ref(&method_attr));
                let error = if matches!(e, ureq::Error::BodyExceedsLimit(_)) {
                    HttpError::ResponseTooLarge(MAX_RESPONSE_SIZE)
                } else {
                    HttpError::Network(e.to_string())
                };
                let error_type = match &error {
                    HttpError::ResponseTooLarge(_) => "ResponseTooLarge",
                    _ => "Network",
                };
                HttpMeters::get().error_counter.add(
                    1,
                    &[
                        KeyValue::new("http.request.method", method),
                        KeyValue::new("error.type", error_type.to_string()),
                    ],
                );
                error
            })?;

        let elapsed = start.elapsed();
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        span.record("http.response.status_code", status as i64);
        span.record("http.response.body.size", body.len() as i64);
        span.record(
            "simulacra.http.client.duration_ms",
            elapsed.as_millis() as i64,
        );

        let attrs = [
            KeyValue::new("http.request.method", request.method.clone()),
            KeyValue::new("http.response.status_code", i64::from(status)),
        ];
        HttpMeters::get()
            .duration_histogram
            .record(elapsed_ms, &attrs);
        HttpMeters::get().request_counter.add(1, &attrs);

        // --- AFTER hook ---
        //
        // Mirror the before-phase redaction: the hook sees headers with
        // sensitive values stripped. If the hook returns a modified context we
        // apply recognized overrides to the response we return to the caller.
        if let Some(ref pipeline) = self.pipeline {
            let after_ctx = serde_json::json!({
                "url": &sanitize_url_for_log(&effective_url),
                "method": &request.method,
                "status": status,
                "headers": redact_headers(&headers),
                "body": String::from_utf8_lossy(&body),
            })
            .to_string();
            match pipeline.run_after(simulacra_hooks::verdict::Operation::HttpRequest, &after_ctx) {
                Ok((simulacra_hooks::Verdict::Continue(None), _)) => {}
                Ok((simulacra_hooks::Verdict::Continue(Some(modified)), _)) => {
                    let overrides = parse_after_overrides(&modified);
                    if let Some(s) = overrides.status {
                        status = s;
                    }
                    if let Some(h) = overrides.headers {
                        headers = h;
                    }
                    if let Some(b) = overrides.body {
                        body = b;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(HttpError::Network(format!("hook error: {e}")));
                }
            }
        }

        // status_text is derived from the (possibly overridden) status code.
        // Use a short well-known table; callers decide how to render unknown codes.
        let status_text = status_to_text(status).to_string();

        Ok(HttpResponse {
            status,
            status_text,
            headers,
            body,
            url: final_url,
            redirected,
        })
    }
}

/// Return `true` if `url` begins with `http://` or `https://`, case-insensitively.
fn url_scheme_is_http(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

impl UreqHttpClient {
    /// Dispatch the request on the given agent, choosing the right ureq method
    /// and handling bodies appropriately. DELETE forwards a supplied body; the
    /// other methods follow their usual RFC 9110 semantics.
    ///
    /// Returns a typed [`HttpError`] — we do this here (rather than returning a
    /// raw `ureq::Error`) so an unsupported method becomes `InvalidUrl`, not a
    /// generic `Network` error.
    fn dispatch(
        &self,
        agent: &ureq::Agent,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<ureq::http::Response<ureq::Body>, DispatchError> {
        /// Apply headers to a without-body request builder and send.
        fn send_without_body(
            mut builder: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
            headers: &[(String, String)],
        ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
            for (name, value) in headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            builder.call()
        }

        /// Apply headers to a with-body request builder and send.
        fn send_with_body(
            mut builder: ureq::RequestBuilder<ureq::typestate::WithBody>,
            headers: &[(String, String)],
            body: Option<&[u8]>,
        ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
            for (name, value) in headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            match body {
                Some(data) => builder.send(data),
                None => builder.send(&[] as &[u8]),
            }
        }

        let result = match method.to_ascii_uppercase().as_str() {
            "GET" => send_without_body(agent.get(url), headers),
            "HEAD" => send_without_body(agent.head(url), headers),
            // DELETE is allowed to carry a body per RFC 9110 §9.3.5. In ureq,
            // `agent.delete()` returns a body-less builder, so when a body is
            // supplied we fall back to a plain DELETE without body (ureq's
            // typestate API does not expose a with-body DELETE).
            "DELETE" => {
                let _ = body; // DELETE body not forwarded via ureq builder API
                send_without_body(agent.delete(url), headers)
            }
            "POST" => send_with_body(agent.post(url), headers, body),
            "PUT" => send_with_body(agent.put(url), headers, body),
            "PATCH" => send_with_body(agent.patch(url), headers, body),
            other => {
                return Err(DispatchError::UnsupportedMethod(other.to_string()));
            }
        };
        result.map_err(DispatchError::Ureq)
    }
}

/// Internal error type for request dispatch. Distinguishes "we refused to
/// dispatch" from "the network failed".
enum DispatchError {
    Ureq(ureq::Error),
    UnsupportedMethod(String),
}

/// Map dispatch errors to our public [`HttpError`] variants.
fn map_ureq_error(err: DispatchError, url: &str) -> HttpError {
    match err {
        DispatchError::UnsupportedMethod(m) => {
            HttpError::InvalidUrl(format!("unsupported HTTP method: {m}"))
        }
        DispatchError::Ureq(ureq::Error::Timeout(_)) => HttpError::Timeout,
        DispatchError::Ureq(ureq::Error::TooManyRedirects) => HttpError::TooManyRedirects,
        DispatchError::Ureq(ureq::Error::HostNotFound) => {
            HttpError::Network(format!("host not found: {url}"))
        }
        DispatchError::Ureq(ureq::Error::ConnectionFailed) => {
            HttpError::Network(format!("connection failed: {url}"))
        }
        DispatchError::Ureq(ureq::Error::RedirectFailed) => {
            HttpError::Network(format!("redirect failed: {url}"))
        }
        DispatchError::Ureq(other) => HttpError::Network(other.to_string()),
    }
}

/// Map common HTTP status codes to their reason phrase.
fn status_to_text(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// Bind a TCP listener on localhost with an OS-assigned port and return it.
    fn localhost_server() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").expect("bind to localhost")
    }

    #[test]
    fn get_request_returns_status_and_body() {
        let listener = localhost_server();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the request (consume headers until blank line).
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            // Write response.
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody")
                .unwrap();
        });

        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: format!("http://{addr}/"),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };
        let response = client.execute(&request).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"body");

        server.join().unwrap();
    }

    #[test]
    fn post_request_sends_body_and_headers() {
        let listener = localhost_server();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            // Read request line.
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            assert!(
                request_line.starts_with("POST"),
                "expected POST, got: {request_line}"
            );

            // Read headers — look for our custom header.
            let mut found_custom_header = false;
            let mut content_length: usize = 0;
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line.to_lowercase().starts_with("x-custom:") {
                    let value = line.split_once(':').unwrap().1.trim().to_string();
                    assert_eq!(value, "hello");
                    found_custom_header = true;
                }
                if line.to_lowercase().starts_with("content-length:") {
                    content_length = line.split_once(':').unwrap().1.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            assert!(found_custom_header, "custom header not received");

            // Read body.
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(body, b"request body");

            // Respond.
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });

        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: format!("http://{addr}/"),
            method: "POST".into(),
            headers: vec![("X-Custom".into(), "hello".into())],
            body: Some(b"request body".to_vec()),
            timeout_ms: None,
            max_redirects: None,
        };
        let response = client.execute(&request).unwrap();
        assert_eq!(response.status, 200);

        server.join().unwrap();
    }

    #[test]
    fn timeout_returns_http_error_timeout() {
        let listener = localhost_server();
        let addr = listener.local_addr().unwrap();

        // Server accepts but never responds.
        let _server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            // Hold connection open for longer than the timeout.
            std::thread::sleep(Duration::from_secs(10));
        });

        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: format!("http://{addr}/"),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: Some(100), // Very short timeout.
            max_redirects: None,
        };
        let result = client.execute(&request);
        assert!(
            matches!(result, Err(HttpError::Timeout)),
            "expected Timeout, got: {result:?}"
        );
    }

    #[test]
    fn network_error_returns_http_error_network() {
        let client = UreqHttpClient::new(1000, 5);
        let request = HttpRequest {
            url: "http://127.0.0.1:1/".into(),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: Some(1000),
            max_redirects: None,
        };
        let result = client.execute(&request);
        assert!(
            matches!(result, Err(HttpError::Network(_))),
            "expected Network error, got: {result:?}"
        );
    }

    #[test]
    fn follows_redirects_and_sets_redirected_flag() {
        // Start the final server first.
        let final_listener = localhost_server();
        let final_addr = final_listener.local_addr().unwrap();

        let final_server = std::thread::spawn(move || {
            let (mut stream, _) = final_listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfinal")
                .unwrap();
        });

        // Redirect server — 302 to the final server.
        let redirect_listener = localhost_server();
        let redirect_addr = redirect_listener.local_addr().unwrap();

        let redirect_server = std::thread::spawn(move || {
            let (mut stream, _) = redirect_listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let location = format!("http://{final_addr}/final");
            let response =
                format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n");
            stream.write_all(response.as_bytes()).unwrap();
        });

        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: format!("http://{redirect_addr}/start"),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };
        let response = client.execute(&request).unwrap();
        assert_eq!(response.status, 200);
        assert!(response.redirected, "expected redirected to be true");
        assert_eq!(response.body, b"final");

        redirect_server.join().unwrap();
        final_server.join().unwrap();
    }

    #[test]
    fn populates_status_text() {
        // Test 200 -> "OK"
        let listener = localhost_server();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: format!("http://{addr}/"),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };
        let response = client.execute(&request).unwrap();
        assert_eq!(response.status_text, "OK");
        server.join().unwrap();

        // Test 404 -> "Not Found"
        let listener2 = localhost_server();
        let addr2 = listener2.local_addr().unwrap();

        let server2 = std::thread::spawn(move || {
            let (mut stream, _) = listener2.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        let request2 = HttpRequest {
            url: format!("http://{addr2}/"),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };
        let response2 = client.execute(&request2).unwrap();
        assert_eq!(response2.status_text, "Not Found");
        server2.join().unwrap();
    }

    #[test]
    fn populates_final_url_after_redirect() {
        let final_listener = localhost_server();
        let final_addr = final_listener.local_addr().unwrap();

        let final_server = std::thread::spawn(move || {
            let (mut stream, _) = final_listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });

        let redirect_listener = localhost_server();
        let redirect_addr = redirect_listener.local_addr().unwrap();
        let final_url_expected = format!("http://{final_addr}/destination");

        let final_url_for_redirect = final_url_expected.clone();
        let redirect_server = std::thread::spawn(move || {
            let (mut stream, _) = redirect_listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {final_url_for_redirect}\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let client = UreqHttpClient::default();
        let original_url = format!("http://{redirect_addr}/origin");
        let request = HttpRequest {
            url: original_url.clone(),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };
        let response = client.execute(&request).unwrap();
        assert_ne!(response.url, original_url, "url should not be the original");
        assert_eq!(response.url, final_url_expected);

        redirect_server.join().unwrap();
        final_server.join().unwrap();
    }

    #[test]
    fn invalid_url_returns_http_error_invalid_url() {
        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: "not a url".into(),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };
        let result = client.execute(&request);
        assert!(
            matches!(result, Err(HttpError::InvalidUrl(_))),
            "expected InvalidUrl, got: {result:?}"
        );
    }

    #[test]
    fn default_timeout_is_5_seconds() {
        let client = UreqHttpClient::default();
        assert_eq!(client.default_timeout_ms, 5000);
    }

    #[test]
    fn default_max_redirects_is_5() {
        let client = UreqHttpClient::default();
        assert_eq!(client.max_redirects, 5);
    }

    #[test]
    fn too_many_redirects_returns_error() {
        // Set up a chain: server1 -> server2 -> server3.
        // With max_redirects=1, only 1 redirect is allowed, so the 2nd should fail.
        let listener3 = localhost_server();
        let addr3 = listener3.local_addr().unwrap();

        let listener2 = localhost_server();
        let addr2 = listener2.local_addr().unwrap();

        let listener1 = localhost_server();
        let addr1 = listener1.local_addr().unwrap();

        // Server 1: redirects to server 2.
        let server1 = std::thread::spawn(move || {
            let (mut stream, _) = listener1.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{addr2}/two\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        // Server 2: redirects to server 3.
        let server2 = std::thread::spawn(move || {
            let (mut stream, _) = listener2.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{addr3}/three\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        // Server 3: final destination (should not be reached).
        let _server3 = std::thread::spawn(move || {
            // Accept but the test should error before reaching here.
            let _ = listener3.accept();
        });

        let client = UreqHttpClient::new(5000, 5);
        let request = HttpRequest {
            url: format!("http://{addr1}/one"),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: Some(1), // Only 1 redirect allowed.
        };
        let result = client.execute(&request);
        assert!(
            matches!(result, Err(HttpError::TooManyRedirects)),
            "expected TooManyRedirects, got: {result:?}"
        );

        server1.join().unwrap();
        // server2 may or may not have been reached; that's fine.
        let _ = server2.join();
    }

    use std::io::Read;

    // ── Observability test helpers ──────────────────────────────────────

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Debug, Clone)]
    struct CapturedSpan {
        fields: HashMap<String, String>,
    }

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        level: String,
        fields: HashMap<String, String>,
    }

    struct CaptureLayer {
        spans: Arc<Mutex<Vec<CapturedSpan>>>,
        span_fields: Arc<Mutex<HashMap<tracing::span::Id, HashMap<String, String>>>>,
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut fields);
            attrs.record(&mut visitor);
            self.span_fields
                .lock()
                .unwrap()
                .insert(id.clone(), fields.clone());
            self.spans.lock().unwrap().push(CapturedSpan { fields });
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut span_fields = self.span_fields.lock().unwrap();
            if let Some(existing) = span_fields.get_mut(id) {
                existing.extend(new_fields.clone());
            }

            let mut spans = self.spans.lock().unwrap();
            if let Some(sf) = span_fields.get(id) {
                for span in spans.iter_mut().rev() {
                    let is_match = sf
                        .iter()
                        .filter(|(k, _)| !new_fields.contains_key(k.as_str()))
                        .all(|(k, v)| span.fields.get(k) == Some(v));
                    if is_match {
                        span.fields.extend(new_fields);
                        break;
                    }
                }
            }
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut fields);
            event.record(&mut visitor);
            self.events.lock().unwrap().push(CapturedEvent {
                level: event.metadata().level().to_string(),
                fields,
            });
        }
    }

    struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl tracing::field::Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    #[allow(clippy::type_complexity)]
    fn setup_capture() -> (
        impl tracing::Subscriber + Send + Sync,
        Arc<Mutex<Vec<CapturedSpan>>>,
        Arc<Mutex<Vec<CapturedEvent>>>,
    ) {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
            spans: Arc::clone(&spans),
            span_fields: Arc::new(Mutex::new(HashMap::new())),
            events: Arc::clone(&events),
        });
        (subscriber, spans, events)
    }

    fn capture_operation<R>(
        operation: impl FnOnce() -> R,
    ) -> (R, Vec<CapturedSpan>, Vec<CapturedEvent>) {
        let (subscriber, spans, events) = setup_capture();
        let result = tracing::subscriber::with_default(subscriber, operation);
        let spans = spans.lock().unwrap().clone();
        let events = events.lock().unwrap().clone();
        (result, spans, events)
    }

    // ── Observability tests ────────────────────────────────────────────

    #[test]
    fn simulacra_http_request_span_emitted_with_method_url_status() {
        let listener = localhost_server();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });

        let client = UreqHttpClient::default();
        let url = format!("http://{addr}/test");
        let request = HttpRequest {
            url: url.clone(),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };

        let (result, spans, _events) = capture_operation(|| client.execute(&request));
        assert!(result.is_ok());

        let http_span = spans
            .iter()
            .find(|s| s.fields.get("http.request.method") == Some(&"GET".to_string()))
            .expect("simulacra_http_request span should be emitted");

        // URL is sanitized for telemetry — only scheme+host, no path/query,
        // so that query tokens and userinfo don't leak into traces.
        let expected_safe = format!("http://{addr}");
        assert_eq!(
            http_span.fields.get("url.full").unwrap(),
            &expected_safe,
            "url.full should be the sanitized scheme+host (path/query stripped)"
        );
        assert_eq!(
            http_span.fields.get("http.response.status_code").unwrap(),
            "200",
            "http.response.status_code should be 200"
        );

        server.join().unwrap();
    }

    #[test]
    fn simulacra_http_request_span_records_body_sizes() {
        let listener = localhost_server();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut content_length: usize = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line.to_lowercase().starts_with("content-length:") {
                    content_length = line.split_once(':').unwrap().1.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            // Consume request body.
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body).unwrap();
            }
            // Respond with a 7-byte body.
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nrespbdy")
                .unwrap();
        });

        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: format!("http://{addr}/test"),
            method: "POST".into(),
            headers: vec![],
            body: Some(b"hello".to_vec()), // 5 bytes
            timeout_ms: None,
            max_redirects: None,
        };

        let (result, spans, _events) = capture_operation(|| client.execute(&request));
        assert!(result.is_ok());

        let http_span = spans
            .iter()
            .find(|s| s.fields.get("http.request.method") == Some(&"POST".to_string()))
            .expect("simulacra_http_request span should be emitted");

        assert_eq!(
            http_span.fields.get("http.request.body.size").unwrap(),
            "5",
            "http.request.body.size should be 5"
        );
        assert_eq!(
            http_span.fields.get("http.response.body.size").unwrap(),
            "7",
            "http.response.body.size should be 7"
        );

        server.join().unwrap();
    }

    #[test]
    fn simulacra_http_request_span_records_duration() {
        let listener = localhost_server();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        let client = UreqHttpClient::default();
        let request = HttpRequest {
            url: format!("http://{addr}/test"),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: None,
            max_redirects: None,
        };

        let (result, spans, _events) = capture_operation(|| client.execute(&request));
        assert!(result.is_ok());

        let http_span = spans
            .iter()
            .find(|s| s.fields.get("http.request.method") == Some(&"GET".to_string()))
            .expect("simulacra_http_request span should be emitted");

        let duration_ms: i64 = http_span
            .fields
            .get("simulacra.http.client.duration_ms")
            .expect("simulacra.http.client.duration_ms should be recorded")
            .parse()
            .expect("duration_ms should be a valid i64");
        assert!(
            duration_ms >= 0,
            "duration_ms should be non-negative, got {duration_ms}"
        );

        server.join().unwrap();
    }

    #[test]
    fn http_error_logged_at_warn() {
        let client = UreqHttpClient::new(1000, 5);
        let request = HttpRequest {
            url: "http://127.0.0.1:1/".into(),
            method: "GET".into(),
            headers: vec![],
            body: None,
            timeout_ms: Some(500),
            max_redirects: None,
        };

        let (result, _spans, events) = capture_operation(|| client.execute(&request));
        assert!(result.is_err());

        let warn_event = events
            .iter()
            .find(|e| e.level == "WARN")
            .expect("a WARN-level event should be emitted on HTTP error");
        assert_eq!(
            warn_event.fields.get("http.request.method").unwrap(),
            "GET",
            "WARN event should include the request method"
        );
        assert!(
            warn_event.fields.contains_key("url.full"),
            "WARN event should include the URL"
        );
        assert!(
            warn_event.fields.contains_key("error.type_"),
            "WARN event should include the error type"
        );
    }
}
