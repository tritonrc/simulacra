//! WHATWG-compliant `fetch()` global function implementation.
//!
//! Registers a `fetch(input, init?)` function on the JS global object that
//! delegates HTTP operations to a [`FetchProxy`] trait implementation. This
//! enables capability checking, budget enforcement, and test faking at the
//! Rust layer while exposing a standards-compliant JS API.

use std::sync::Arc;

use rquickjs::{Ctx, Function};
use thiserror::Error;

use crate::{register_abort, register_blob, register_headers, register_request, register_response};

/// Errors returned by [`FetchProxy::fetch`].
///
/// Each variant maps to a distinct JS rejection behavior, following WHATWG
/// conventions where applicable.
#[derive(Debug, Error)]
pub enum FetchError {
    /// The caller lacks permission to access the requested resource.
    #[error("capability denied: {0}")]
    CapabilityDenied(String),
    /// The caller's budget (e.g. token or request quota) has been exhausted.
    #[error("budget exhausted: {0}")]
    BudgetExhausted(String),
    /// A network-level error occurred (DNS, connection refused, TLS, etc.).
    #[error("network error: {0}")]
    NetworkError(String),
    /// The request timed out.
    #[error("request timed out")]
    Timeout,
    /// The request was aborted via an `AbortSignal`.
    #[error("aborted: {0}")]
    Aborted(String),
}

/// Response data returned by [`FetchProxy::fetch`] on success.
///
/// This is a plain data struct — the `register_fetch_global` function converts
/// it into a JS `Response` object with headers, body, etc.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// HTTP status code (e.g. 200, 404, 500).
    pub status: u16,
    /// HTTP status text (e.g. "OK", "Not Found").
    pub status_text: String,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
    /// The final URL after any redirects.
    pub url: String,
    /// Whether the response was the result of a redirect.
    pub redirected: bool,
}

/// Trait for proxying HTTP fetch operations through a capability-checking layer.
///
/// Implementations are responsible for:
/// - Capability and budget checks
/// - Performing the actual HTTP request
/// - Timeout enforcement (using `timeout_ms` if provided)
/// - Returning structured errors via [`FetchError`]
pub trait FetchProxy: Send + Sync {
    /// Perform an HTTP fetch.
    fn fetch(
        &self,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<FetchResponse, FetchError>;
}

/// Register the `fetch()` global function into a QuickJS context.
///
/// The function accepts `(input, init?)` where `input` is a URL string or a
/// `Request` object, and `init` is an optional options object. It delegates the
/// actual HTTP operation to the provided [`FetchProxy`].
pub fn register_fetch_global(
    ctx: &Ctx<'_>,
    proxy: Arc<dyn FetchProxy>,
) -> Result<(), rquickjs::Error> {
    // Register `__simulacra_fetch_proxy__` as a synchronous Rust host function.
    // It accepts a single JSON string containing all request parameters.
    // Returns a JSON string with either success response data or error info.
    //
    // Using a single JSON string avoids rquickjs type conversion issues
    // (e.g., `undefined` -> f64 fails).

    let fetch_impl = Function::new(
        ctx.clone(),
        move |request_json: String| -> rquickjs::Result<String> {
            #[derive(serde::Deserialize)]
            struct FetchRequest {
                url: String,
                method: String,
                headers: Vec<(String, String)>,
                body: Option<Vec<u8>>,
                timeout_ms: Option<u64>,
            }

            let req: FetchRequest = serde_json::from_str(&request_json).map_err(|e| {
                rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
            })?;

            match proxy.fetch(
                &req.url,
                &req.method,
                &req.headers,
                req.body.as_deref(),
                req.timeout_ms,
            ) {
                Ok(resp) => {
                    let response_obj = serde_json::json!({
                        "ok": true,
                        "status": resp.status,
                        "statusText": resp.status_text,
                        "headers": resp.headers,
                        "body": resp.body,
                        "url": resp.url,
                        "redirected": resp.redirected,
                    });
                    Ok(response_obj.to_string())
                }
                Err(e) => {
                    let (error_type, message) = match &e {
                        FetchError::CapabilityDenied(msg) => ("CapabilityDenied", msg.clone()),
                        FetchError::BudgetExhausted(msg) => ("BudgetExhausted", msg.clone()),
                        FetchError::NetworkError(msg) => ("NetworkError", msg.clone()),
                        FetchError::Timeout => ("Timeout", "The operation timed out".to_string()),
                        FetchError::Aborted(msg) => ("Aborted", msg.clone()),
                    };

                    let err_obj = serde_json::json!({
                        "ok": false,
                        "type": error_type,
                        "message": message,
                    });
                    Ok(err_obj.to_string())
                }
            }
        },
    )?;

    let globals = ctx.globals();
    globals.set("__simulacra_fetch_proxy__", fetch_impl)?;

    // Register a minimal DOMException polyfill if not already present.
    // QuickJS does not include DOMException, but we need it for WHATWG-
    // compliant error types (AbortError, TimeoutError).
    ctx.eval::<(), _>(
        r#"
(function() {
    if (typeof globalThis.DOMException === "undefined") {
        function DOMException(message, name) {
            this.message = message || "";
            this.name = name || "Error";
        }
        DOMException.prototype = Object.create(Error.prototype);
        DOMException.prototype.constructor = DOMException;
        globalThis.DOMException = DOMException;
    }
})();
"#,
    )?;

    // Define the `fetch` global as a JS function that:
    // 1. Parses `input` (string or Request) and `init` options
    // 2. Extracts signal/timeout information
    // 3. Calls `__simulacra_fetch_proxy__` synchronously
    // 4. Constructs a proper Response object from the result
    // 5. Returns a Promise (resolved or rejected)
    ctx.eval::<(), _>(
        r#"
(function() {
    function stringToBytes(str) {
        var bytes = [];
        for (var i = 0; i < str.length; i++) {
            var code = str.charCodeAt(i);
            if (code < 0x80) {
                bytes.push(code);
            } else if (code < 0x800) {
                bytes.push(0xC0 | (code >> 6));
                bytes.push(0x80 | (code & 0x3F));
            } else if (code >= 0xD800 && code <= 0xDBFF) {
                var next = str.charCodeAt(++i);
                var cp = ((code - 0xD800) << 10) + (next - 0xDC00) + 0x10000;
                bytes.push(0xF0 | (cp >> 18));
                bytes.push(0x80 | ((cp >> 12) & 0x3F));
                bytes.push(0x80 | ((cp >> 6) & 0x3F));
                bytes.push(0x80 | (cp & 0x3F));
            } else {
                bytes.push(0xE0 | (code >> 12));
                bytes.push(0x80 | ((code >> 6) & 0x3F));
                bytes.push(0x80 | (code & 0x3F));
            }
        }
        return bytes;
    }

    function bodyToByteArray(body) {
        if (body === undefined || body === null) {
            return null;
        }
        if (typeof body === "string") {
            return stringToBytes(body);
        }
        if (body instanceof ArrayBuffer) {
            var view = new Uint8Array(body);
            var arr = [];
            for (var i = 0; i < view.length; i++) arr.push(view[i]);
            return arr;
        }
        if (body instanceof Uint8Array) {
            var arr2 = [];
            for (var j = 0; j < body.length; j++) arr2.push(body[j]);
            return arr2;
        }
        // Blob: extract bytes synchronously via internal helper.
        if (typeof Blob !== "undefined" && body instanceof Blob) {
            var bytes = (typeof body.__bytes_sync__ === "function")
                ? body.__bytes_sync__()
                : new Uint8Array(0);
            var arr3 = [];
            for (var k = 0; k < bytes.length; k++) arr3.push(bytes[k]);
            return arr3;
        }
        // Fallback: convert to string
        return stringToBytes(String(body));
    }

    globalThis.fetch = function fetch(input, init) {
        try {
            init = init || {};
            var url, method, headers, body, signal, timeoutMs;

            // Parse input: string URL or Request object
            if (typeof Request !== "undefined" && input instanceof Request) {
                url = input.url;
                method = input.method;
                headers = input.headers;
                // Pull the body from the Request's internal storage so that
                // fetch(new Request(url, { body })) forwards it to the proxy.
                body = (typeof input.__body_for_fetch__ === "function")
                    ? input.__body_for_fetch__()
                    : null;
                signal = input.signal;
            } else {
                url = String(input);
                method = "GET";
                headers = null;
                body = null;
                signal = null;
            }

            // Apply init overrides
            if (init.method !== undefined) {
                method = String(init.method).toUpperCase();
            } else if (typeof method === "string") {
                method = method.toUpperCase();
            }

            if (init.headers !== undefined) {
                headers = new Headers(init.headers);
            } else if (headers && !(headers instanceof Headers)) {
                headers = new Headers(headers);
            } else if (!headers) {
                headers = new Headers();
            }

            if (init.body !== undefined) {
                body = init.body;
            }

            if (init.signal !== undefined) {
                signal = init.signal;
            }

            // Check signal — if already aborted, reject immediately
            if (signal && signal.aborted) {
                var abortReason = signal.reason || "The operation was aborted";
                return Promise.reject(
                    new DOMException(
                        typeof abortReason === "string" ? abortReason : String(abortReason),
                        "AbortError"
                    )
                );
            }

            // Extract timeout_ms from signal
            timeoutMs = null;
            if (signal && signal.timeout_ms !== undefined && signal.timeout_ms !== null) {
                timeoutMs = signal.timeout_ms;
            }

            // Serialize headers to array of [name, value] pairs
            var headerPairs = [];
            if (headers instanceof Headers) {
                var entries = headers.entries();
                for (var i = 0; i < entries.length; i++) {
                    headerPairs.push(entries[i]);
                }
            }

            // Serialize body to byte array
            var bodyBytes = bodyToByteArray(body);

            // Build the request JSON for the Rust proxy
            var requestObj = {
                url: url,
                method: method,
                headers: headerPairs,
                body: bodyBytes,
                timeout_ms: timeoutMs
            };

            var rawResult = __simulacra_fetch_proxy__(JSON.stringify(requestObj));
            var parsed = JSON.parse(rawResult);

            // Check if this is an error response
            if (!parsed.ok) {
                var errType = parsed.type;
                var errMsg = parsed.message;

                if (errType === "NetworkError") {
                    return Promise.reject(new TypeError(errMsg));
                }
                if (errType === "Timeout") {
                    return Promise.reject(new DOMException(errMsg, "TimeoutError"));
                }
                if (errType === "Aborted") {
                    return Promise.reject(new DOMException(errMsg, "AbortError"));
                }
                if (errType === "CapabilityDenied") {
                    return Promise.reject(new Error("capability denied: " + errMsg));
                }
                if (errType === "BudgetExhausted") {
                    return Promise.reject(new Error("budget exhausted: " + errMsg));
                }
                return Promise.reject(new Error(errMsg));
            }

            // Build a Response from the proxy result
            var respHeaders = new Headers(parsed.headers || []);

            // Convert body byte array back to Uint8Array
            var respBodyBytes = null;
            if (parsed.body && parsed.body.length > 0) {
                respBodyBytes = new Uint8Array(parsed.body);
            }

            var resp = new Response(respBodyBytes, {
                status: parsed.status,
                statusText: parsed.statusText || "",
                headers: respHeaders
            });

            // Override url and redirected properties since Response
            // constructor doesn't accept them directly.
            Object.defineProperty(resp, "url", {
                get: function() { return parsed.url || ""; },
                enumerable: true,
                configurable: true
            });
            Object.defineProperty(resp, "redirected", {
                get: function() { return !!parsed.redirected; },
                enumerable: true,
                configurable: true
            });

            return Promise.resolve(resp);
        } catch(e) {
            return Promise.reject(e);
        }
    };
})();
"#,
    )?;

    Ok(())
}

/// Register all WHATWG Fetch API globals into a QuickJS context.
///
/// Registers `Headers`, `Blob`, `AbortController`/`AbortSignal`, `Request`,
/// `Response`, and `fetch()` in the correct dependency order.
pub fn register_globals(ctx: &Ctx<'_>, proxy: Arc<dyn FetchProxy>) -> Result<(), rquickjs::Error> {
    register_headers(ctx)?;
    register_blob(ctx)?;
    register_abort(ctx)?;
    register_request(ctx)?;
    register_response(ctx)?;
    register_fetch_global(ctx, proxy)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{FromJs, Promise, Value};
    use std::sync::Mutex;

    /// A configurable mock implementation of [`FetchProxy`] for tests.
    ///
    /// By default returns a 200 OK response. Can be configured to return
    /// specific responses or errors, and records the last request for
    /// verification.
    struct MockFetchProxy {
        response: Mutex<Option<Result<FetchResponse, FetchError>>>,
        last_request: Mutex<Option<CapturedRequest>>,
    }

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
        timeout_ms: Option<u64>,
    }

    impl MockFetchProxy {
        fn new() -> Self {
            Self {
                response: Mutex::new(None),
                last_request: Mutex::new(None),
            }
        }

        fn with_response(response: FetchResponse) -> Self {
            Self {
                response: Mutex::new(Some(Ok(response))),
                last_request: Mutex::new(None),
            }
        }

        fn with_error(error: FetchError) -> Self {
            Self {
                response: Mutex::new(Some(Err(error))),
                last_request: Mutex::new(None),
            }
        }

        fn last_request(&self) -> Option<CapturedRequest> {
            self.last_request.lock().unwrap().clone()
        }
    }

    impl FetchProxy for MockFetchProxy {
        fn fetch(
            &self,
            url: &str,
            method: &str,
            headers: &[(String, String)],
            body: Option<&[u8]>,
            timeout_ms: Option<u64>,
        ) -> Result<FetchResponse, FetchError> {
            *self.last_request.lock().unwrap() = Some(CapturedRequest {
                url: url.to_string(),
                method: method.to_string(),
                headers: headers.to_vec(),
                body: body.map(|b| b.to_vec()),
                timeout_ms,
            });

            let mut response_guard = self.response.lock().unwrap();
            match response_guard.take() {
                Some(Ok(resp)) => {
                    // Put it back so subsequent calls still work
                    *response_guard = Some(Ok(resp.clone()));
                    Ok(resp)
                }
                Some(Err(e)) => {
                    // Reconstruct the error for subsequent calls
                    let cloned = match &e {
                        FetchError::CapabilityDenied(m) => FetchError::CapabilityDenied(m.clone()),
                        FetchError::BudgetExhausted(m) => FetchError::BudgetExhausted(m.clone()),
                        FetchError::NetworkError(m) => FetchError::NetworkError(m.clone()),
                        FetchError::Timeout => FetchError::Timeout,
                        FetchError::Aborted(m) => FetchError::Aborted(m.clone()),
                    };
                    *response_guard = Some(Err(cloned));
                    Err(e)
                }
                None => {
                    // Default: 200 OK with empty body
                    Ok(FetchResponse {
                        status: 200,
                        status_text: "OK".to_string(),
                        headers: vec![],
                        body: vec![],
                        url: url.to_string(),
                        redirected: false,
                    })
                }
            }
        }
    }

    fn with_fetch_context<F: FnOnce(&Ctx<'_>, &Arc<MockFetchProxy>)>(proxy: MockFetchProxy, f: F) {
        let rt = rquickjs::Runtime::new().expect("failed to create runtime");
        let ctx = rquickjs::Context::full(&rt).expect("failed to create context");
        let proxy = Arc::new(proxy);
        let proxy_clone = Arc::clone(&proxy);
        ctx.with(|ctx| {
            register_globals(&ctx, proxy_clone as Arc<dyn FetchProxy>)
                .expect("failed to register globals");
            f(&ctx, &proxy);
        });
    }

    /// Evaluate JS code that uses top-level `await` and resolve all promises.
    fn eval_async<'js>(ctx: &Ctx<'js>, code: &str) -> Value<'js> {
        let promise: Promise<'js> = ctx.eval_promise(code).expect("eval_promise failed");
        promise
            .finish::<Value<'_>>()
            .expect("promise resolution failed");
        ctx.eval("globalThis.__result__")
            .expect("failed to read __result__")
    }

    // -----------------------------------------------------------------------
    // Test 1: fetch_url_returns_response
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_url_returns_response() {
        let mock = MockFetchProxy::with_response(FetchResponse {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: b"hello world".to_vec(),
            url: "http://example.com/data".to_string(),
            redirected: false,
        });

        with_fetch_context(mock, |ctx, _proxy| {
            let val = eval_async(
                ctx,
                r#"
                var resp = await fetch("http://example.com/data");
                var text = await resp.text();
                globalThis.__result__ = [resp.status, text];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            // Status comes through as a JS Number; use as_float since JSON.parse
            // produces floats for numbers from the proxy response JSON.
            let status = result[0]
                .as_int()
                .or_else(|| result[0].as_float().map(|f| f as i32))
                .expect("status should be a number");
            assert_eq!(status, 200);
            let text: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "hello world");
        });
    }

    // -----------------------------------------------------------------------
    // Test 2: fetch_with_post_and_headers
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_with_post_and_headers() {
        let mock = MockFetchProxy::with_response(FetchResponse {
            status: 201,
            status_text: "Created".to_string(),
            headers: vec![],
            body: b"created".to_vec(),
            url: "http://example.com/api".to_string(),
            redirected: false,
        });

        with_fetch_context(mock, |ctx, proxy| {
            let _val = eval_async(
                ctx,
                r#"
                var resp = await fetch("http://example.com/api", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Custom": "test-value" },
                    body: '{"key":"value"}'
                });
                globalThis.__result__ = resp.status;
                "#,
            );

            let req = proxy.last_request().expect("should have captured request");
            assert_eq!(req.method, "POST");
            assert_eq!(req.url, "http://example.com/api");

            // Verify headers were passed
            let has_ct = req
                .headers
                .iter()
                .any(|(n, v)| n == "content-type" && v == "application/json");
            assert!(has_ct, "should have content-type header");

            let has_custom = req
                .headers
                .iter()
                .any(|(n, v)| n == "x-custom" && v == "test-value");
            assert!(has_custom, "should have x-custom header");

            // Verify body was passed
            assert!(req.body.is_some(), "body should be present");
            let body_str = String::from_utf8(req.body.unwrap()).unwrap();
            assert_eq!(body_str, r#"{"key":"value"}"#);
        });
    }

    // -----------------------------------------------------------------------
    // Test 3: fetch_with_request_input
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_with_request_input() {
        let mock = MockFetchProxy::with_response(FetchResponse {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![],
            body: b"ok".to_vec(),
            url: "http://example.com/req".to_string(),
            redirected: false,
        });

        with_fetch_context(mock, |ctx, proxy| {
            let _val = eval_async(
                ctx,
                r#"
                var req = new Request("http://example.com/req", {
                    method: "POST",
                    headers: { "X-From-Request": "yes" },
                    body: "request-body-payload"
                });
                var resp = await fetch(req);
                globalThis.__result__ = resp.status;
                "#,
            );

            let req = proxy.last_request().expect("should have captured request");
            assert_eq!(req.url, "http://example.com/req");
            assert_eq!(req.method, "POST");
            // Body from the Request must be forwarded to the proxy.
            let body = req.body.expect("body should be forwarded from Request");
            assert_eq!(String::from_utf8(body).unwrap(), "request-body-payload");
            // Headers from the Request must also flow through.
            assert!(
                req.headers
                    .iter()
                    .any(|(n, v)| n == "x-from-request" && v == "yes"),
                "x-from-request header should have been forwarded"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Test 4: fetch_with_request_and_init_override
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_with_request_and_init_override() {
        let mock = MockFetchProxy::with_response(FetchResponse {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![],
            body: vec![],
            url: "http://example.com/override".to_string(),
            redirected: false,
        });

        with_fetch_context(mock, |ctx, proxy| {
            let _val = eval_async(
                ctx,
                r#"
                // Request starts as POST with a body; init overrides the method
                // to PUT but leaves the body intact (init.body is undefined).
                var req = new Request("http://example.com/override", {
                    method: "POST",
                    body: "original-body"
                });
                var resp = await fetch(req, { method: "PUT" });
                globalThis.__result__ = resp.status;
                "#,
            );

            let req = proxy.last_request().expect("should have captured request");
            assert_eq!(req.method, "PUT", "init.method overrides Request.method");
            assert_eq!(req.url, "http://example.com/override");
            let body = req
                .body
                .expect("body from Request should survive method override");
            assert_eq!(String::from_utf8(body).unwrap(), "original-body");
        });
    }

    // -----------------------------------------------------------------------
    // Test 5: fetch_with_timeout_signal
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_with_timeout_signal() {
        let mock = MockFetchProxy::with_response(FetchResponse {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![],
            body: vec![],
            url: "http://example.com/timeout".to_string(),
            redirected: false,
        });

        with_fetch_context(mock, |ctx, proxy| {
            let _val = eval_async(
                ctx,
                r#"
                var resp = await fetch("http://example.com/timeout", {
                    signal: AbortSignal.timeout(100)
                });
                globalThis.__result__ = resp.status;
                "#,
            );

            let req = proxy.last_request().expect("should have captured request");
            assert_eq!(req.timeout_ms, Some(100));
        });
    }

    // -----------------------------------------------------------------------
    // Test 6: fetch_with_aborted_signal_rejects
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_with_aborted_signal_rejects() {
        let mock = MockFetchProxy::new();

        with_fetch_context(mock, |ctx, proxy| {
            let val = eval_async(
                ctx,
                r#"
                var ac = new AbortController();
                ac.abort("cancelled by user");
                try {
                    await fetch("http://example.com/abort", { signal: ac.signal });
                    globalThis.__result__ = false;
                } catch(e) {
                    globalThis.__result__ = [true, e.message, e.name];
                }
                "#,
            );

            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let caught: bool = result[0].as_bool().unwrap();
            assert!(caught, "fetch should have rejected");

            let message: String = result[1].as_string().unwrap().to_string().unwrap();
            assert!(
                message.contains("cancelled by user"),
                "error message should contain abort reason, got: {message}"
            );

            // The proxy should NOT have been called since signal was pre-aborted
            assert!(
                proxy.last_request().is_none(),
                "proxy should not be called for pre-aborted signal"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Test 7: fetch_capability_denied_rejects
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_capability_denied_rejects() {
        let mock =
            MockFetchProxy::with_error(FetchError::CapabilityDenied("http not allowed".into()));

        with_fetch_context(mock, |ctx, _proxy| {
            let val = eval_async(
                ctx,
                r#"
                try {
                    await fetch("http://example.com/denied");
                    globalThis.__result__ = false;
                } catch(e) {
                    globalThis.__result__ = [true, e.message];
                }
                "#,
            );

            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let caught: bool = result[0].as_bool().unwrap();
            assert!(caught, "fetch should have rejected");

            let message: String = result[1].as_string().unwrap().to_string().unwrap();
            assert!(
                message.contains("capability denied"),
                "error should mention capability denied, got: {message}"
            );
            assert!(
                message.contains("http not allowed"),
                "error should contain the specific message, got: {message}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Test 8: fetch_budget_exhausted_rejects
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_budget_exhausted_rejects() {
        let mock =
            MockFetchProxy::with_error(FetchError::BudgetExhausted("rate limit exceeded".into()));

        with_fetch_context(mock, |ctx, _proxy| {
            let val = eval_async(
                ctx,
                r#"
                try {
                    await fetch("http://example.com/budget");
                    globalThis.__result__ = false;
                } catch(e) {
                    globalThis.__result__ = [true, e.message];
                }
                "#,
            );

            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let caught: bool = result[0].as_bool().unwrap();
            assert!(caught, "fetch should have rejected");

            let message: String = result[1].as_string().unwrap().to_string().unwrap();
            assert!(
                message.contains("budget exhausted"),
                "error should mention budget exhausted, got: {message}"
            );
            assert!(
                message.contains("rate limit exceeded"),
                "error should contain the specific message, got: {message}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Test 9: fetch_network_error_rejects_with_typeerror
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_network_error_rejects_with_typeerror() {
        let mock =
            MockFetchProxy::with_error(FetchError::NetworkError("connection refused".into()));

        with_fetch_context(mock, |ctx, _proxy| {
            let val = eval_async(
                ctx,
                r#"
                try {
                    await fetch("http://example.com/network-err");
                    globalThis.__result__ = false;
                } catch(e) {
                    globalThis.__result__ = [e instanceof TypeError, e.message];
                }
                "#,
            );

            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let is_type_error: bool = result[0].as_bool().unwrap();
            assert!(is_type_error, "NetworkError should produce a TypeError");

            let message: String = result[1].as_string().unwrap().to_string().unwrap();
            assert!(
                message.contains("connection refused"),
                "error message should contain details, got: {message}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Test 10: fetch_timeout_rejects_with_timeout_error
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_timeout_rejects_with_timeout_error() {
        let mock = MockFetchProxy::with_error(FetchError::Timeout);

        with_fetch_context(mock, |ctx, _proxy| {
            let val = eval_async(
                ctx,
                r#"
                try {
                    await fetch("http://example.com/timeout-err");
                    globalThis.__result__ = false;
                } catch(e) {
                    globalThis.__result__ = [e.name, e.message];
                }
                "#,
            );

            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let name: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(name, "TimeoutError", "timeout should produce TimeoutError");

            let message: String = result[1].as_string().unwrap().to_string().unwrap();
            assert!(
                message.contains("timed out"),
                "error message should mention timeout, got: {message}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Test 11: fetch_response_has_correct_properties
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_with_blob_body_forwards_bytes() {
        let mock = MockFetchProxy::with_response(FetchResponse {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![],
            body: vec![],
            url: "http://example.com/blob".to_string(),
            redirected: false,
        });

        with_fetch_context(mock, |ctx, proxy| {
            let _val = eval_async(
                ctx,
                r#"
                var blob = new Blob(["payload-from-blob"], { type: "text/plain" });
                var resp = await fetch("http://example.com/blob", {
                    method: "POST",
                    body: blob
                });
                globalThis.__result__ = resp.status;
                "#,
            );

            let req = proxy.last_request().expect("should have captured request");
            assert_eq!(req.method, "POST");
            let body = req.body.expect("blob body should be extracted");
            assert_eq!(
                String::from_utf8(body).unwrap(),
                "payload-from-blob",
                "blob body bytes should be forwarded, not stringified"
            );
        });
    }

    #[test]
    fn fetch_response_has_correct_properties() {
        let mock = MockFetchProxy::with_response(FetchResponse {
            status: 201,
            status_text: "Created".to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-request-id".to_string(), "abc123".to_string()),
            ],
            body: vec![],
            url: "http://example.com/resource".to_string(),
            redirected: true,
        });

        with_fetch_context(mock, |ctx, _proxy| {
            let val = eval_async(
                ctx,
                r#"
                var resp = await fetch("http://example.com/resource");
                globalThis.__result__ = [
                    resp.status,
                    resp.statusText,
                    resp.ok,
                    resp.headers.get("content-type"),
                    resp.headers.get("x-request-id"),
                    resp.url,
                    resp.redirected,
                    resp.type,
                    resp.body === null
                ];
                "#,
            );

            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();

            let status = result[0]
                .as_int()
                .or_else(|| result[0].as_float().map(|f| f as i32))
                .expect("status should be a number");
            assert_eq!(status, 201, "status should be 201");

            let status_text: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(status_text, "Created", "statusText should be 'Created'");

            let ok: bool = result[2].as_bool().unwrap();
            assert!(ok, "201 should be ok");

            let ct: String = result[3].as_string().unwrap().to_string().unwrap();
            assert_eq!(ct, "application/json", "content-type header");

            let req_id: String = result[4].as_string().unwrap().to_string().unwrap();
            assert_eq!(req_id, "abc123", "x-request-id header");

            let url: String = result[5].as_string().unwrap().to_string().unwrap();
            assert_eq!(url, "http://example.com/resource", "url");

            let redirected: bool = result[6].as_bool().unwrap();
            assert!(redirected, "redirected should be true");

            let resp_type: String = result[7].as_string().unwrap().to_string().unwrap();
            assert_eq!(resp_type, "basic", "type should be 'basic'");

            let body_null: bool = result[8].as_bool().unwrap();
            assert!(body_null, "body should be null (no ReadableStream)");
        });
    }
}
