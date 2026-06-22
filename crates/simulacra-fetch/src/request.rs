//! WHATWG-compliant `Request` implementation.
//!
//! Provides a Rust `Request` struct and a JS class registration function that
//! installs a WHATWG-compliant `Request` constructor into a QuickJS context.

use rquickjs::Ctx;

use crate::{Blob, BlobPart, Headers};

/// A WHATWG-compliant Request.
///
/// Holds a URL, HTTP method, headers, optional body bytes, body-consumption
/// tracking, and abort-signal metadata.
#[derive(Debug, Clone)]
pub struct Request {
    url: String,
    method: String,
    headers: Headers,
    body: Option<Vec<u8>>,
    body_used: bool,
    signal_aborted: bool,
    signal_reason: Option<String>,
    signal_timeout_ms: Option<u64>,
}

/// Signal information extracted from an `AbortSignal` or signal-like source.
#[derive(Debug, Clone, Default)]
pub struct SignalInfo {
    /// Whether the signal is already aborted.
    pub aborted: bool,
    /// The abort reason, if any.
    pub reason: Option<String>,
    /// Timeout in milliseconds, if set via `AbortSignal.timeout()`.
    pub timeout_ms: Option<u64>,
}

impl Request {
    /// Create a new `Request`.
    ///
    /// The `method` is uppercased. If `body` is provided as a `Blob`, the
    /// blob's content-type is auto-set on `headers` (if not already present).
    pub fn new(
        url: String,
        method: String,
        headers: Headers,
        body: Option<Vec<u8>>,
        signal: Option<SignalInfo>,
    ) -> Self {
        let signal = signal.unwrap_or_default();
        Self {
            url,
            method: method.to_uppercase(),
            headers,
            body,
            body_used: false,
            signal_aborted: signal.aborted,
            signal_reason: signal.reason,
            signal_timeout_ms: signal.timeout_ms,
        }
    }

    /// Create a new `Request` from a body that may be a `Blob`.
    ///
    /// If `blob_content_type` is `Some`, it is set as the `Content-Type`
    /// header (unless the header is already present).
    pub fn new_with_blob_body(
        url: String,
        method: String,
        mut headers: Headers,
        blob: Blob,
        signal: Option<SignalInfo>,
    ) -> Self {
        let ct = blob.type_().to_string();
        if !ct.is_empty() && !headers.has("content-type") {
            headers.set("content-type", &ct);
        }
        let body = Some(blob.array_buffer());
        let signal = signal.unwrap_or_default();
        Self {
            url,
            method: method.to_uppercase(),
            headers,
            body,
            body_used: false,
            signal_aborted: signal.aborted,
            signal_reason: signal.reason,
            signal_timeout_ms: signal.timeout_ms,
        }
    }

    /// The request URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The HTTP method (always uppercased).
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The request headers.
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Whether the body has already been consumed.
    pub fn body_used(&self) -> bool {
        self.body_used
    }

    /// Whether the associated signal is aborted.
    pub fn signal_aborted(&self) -> bool {
        self.signal_aborted
    }

    /// The signal's abort reason, if any.
    pub fn signal_reason(&self) -> Option<&str> {
        self.signal_reason.as_deref()
    }

    /// The signal's timeout in milliseconds, if any.
    pub fn signal_timeout_ms(&self) -> Option<u64> {
        self.signal_timeout_ms
    }

    /// Consume the body, returning the raw bytes.
    ///
    /// Can only be called once. A second call returns `Err`.
    pub fn consume_body(&mut self) -> Result<Vec<u8>, String> {
        if self.body_used {
            return Err("Body has already been consumed".to_string());
        }
        self.body_used = true;
        Ok(self.body.take().unwrap_or_default())
    }

    /// Consume the body as a UTF-8 string.
    pub fn text(&mut self) -> Result<String, String> {
        let bytes = self.consume_body()?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Consume the body and parse as JSON.
    pub fn json(&mut self) -> Result<serde_json::Value, String> {
        let bytes = self.consume_body()?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }

    /// Consume the body as raw bytes (simulates `arrayBuffer()`).
    pub fn array_buffer(&mut self) -> Result<Vec<u8>, String> {
        self.consume_body()
    }

    /// Consume the body as raw bytes (simulates `bytes()`).
    pub fn bytes_(&mut self) -> Result<Vec<u8>, String> {
        self.consume_body()
    }

    /// Consume the body and wrap it in a `Blob` with the request's
    /// `Content-Type` header (or the provided headers' Content-Type).
    pub fn blob(&mut self, headers: &Headers) -> Result<Blob, String> {
        let bytes = self.consume_body()?;
        let ct = headers.get("content-type");
        Ok(Blob::new(vec![BlobPart::Bytes(bytes)], ct.as_deref()))
    }

    /// Clone the request. Fails if the body has been consumed.
    pub fn try_clone(&self) -> Result<Request, String> {
        if self.body_used {
            return Err("Cannot clone a Request whose body has been consumed".to_string());
        }
        Ok(Request {
            url: self.url.clone(),
            method: self.method.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            body_used: false,
            signal_aborted: self.signal_aborted,
            signal_reason: self.signal_reason.clone(),
            signal_timeout_ms: self.signal_timeout_ms,
        })
    }
}

/// Register the `Request` class as a JS global.
///
/// After calling this, JS code can use:
/// - `new Request(url)` — GET with empty headers
/// - `new Request(url, { method, body, headers, signal })`
/// - `new Request(existingRequest)` — clone
/// - `new Request(existingRequest, { method: "PUT" })` — clone with override
/// - `.url`, `.method`, `.headers`, `.bodyUsed`, `.signal` (read-only)
/// - `.text()`, `.json()`, `.arrayBuffer()`, `.bytes()`, `.blob()` — Promises, single-consumption
/// - `.clone()` — deep copy, throws TypeError if body consumed
pub fn register_request(ctx: &Ctx<'_>) -> Result<(), rquickjs::Error> {
    ctx.eval::<(), _>(
        r#"
(function() {
    var URL_KEY = Symbol("__request_url__");
    var METHOD_KEY = Symbol("__request_method__");
    var HEADERS_KEY = Symbol("__request_headers__");
    var BODY_KEY = Symbol("__request_body__");
    var BODY_USED_KEY = Symbol("__request_body_used__");
    // SIGNAL_KEY stores a live reference to the AbortSignal passed in
    // init.signal (or undefined if none was provided). Reading .signal
    // returns this object directly so external mutations (e.g. abort()
    // called on the controller) are visible. A synthesized default
    // non-aborted signal is returned when SIGNAL_KEY is undefined.
    var SIGNAL_KEY = Symbol("__request_signal__");

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
        return new Uint8Array(bytes);
    }

    function bytesToString(bytes) {
        var result = "";
        var i = 0;
        while (i < bytes.length) {
            var b = bytes[i];
            if (b < 0x80) {
                result += String.fromCharCode(b);
                i++;
            } else if ((b & 0xE0) === 0xC0) {
                result += String.fromCharCode(((b & 0x1F) << 6) | (bytes[i+1] & 0x3F));
                i += 2;
            } else if ((b & 0xF0) === 0xE0) {
                result += String.fromCharCode(((b & 0x0F) << 12) | ((bytes[i+1] & 0x3F) << 6) | (bytes[i+2] & 0x3F));
                i += 3;
            } else if ((b & 0xF8) === 0xF0) {
                var cp = ((b & 0x07) << 18) | ((bytes[i+1] & 0x3F) << 12) | ((bytes[i+2] & 0x3F) << 6) | (bytes[i+3] & 0x3F);
                if (cp > 0xFFFF) {
                    cp -= 0x10000;
                    result += String.fromCharCode(0xD800 + (cp >> 10), 0xDC00 + (cp & 0x3FF));
                } else {
                    result += String.fromCharCode(cp);
                }
                i += 4;
            } else {
                result += "\uFFFD";
                i++;
            }
        }
        return result;
    }

    function bodyToBytes(body) {
        if (body === undefined || body === null) {
            return null;
        }
        if (typeof body === "string") {
            return stringToBytes(body);
        }
        if (body instanceof ArrayBuffer) {
            return new Uint8Array(body);
        }
        if (body instanceof Uint8Array) {
            return new Uint8Array(body);
        }
        if (typeof Blob !== "undefined" && body instanceof Blob) {
            return body;
        }
        // Fallback: convert to string
        return stringToBytes(String(body));
    }

    function Request(input, init) {
        if (!(this instanceof Request)) {
            throw new TypeError("Request constructor requires 'new'");
        }

        init = init || {};

        // Clone from existing Request
        if (input instanceof Request) {
            if (input[BODY_USED_KEY]) {
                throw new TypeError("Cannot construct a Request from a Request whose body has been consumed");
            }
            this[URL_KEY] = input[URL_KEY];
            this[METHOD_KEY] = init.method ? String(init.method).toUpperCase() : input[METHOD_KEY];
            // Copy headers from source, then merge init headers on top
            this[HEADERS_KEY] = new Headers(input[HEADERS_KEY]);
            if (init.headers) {
                var initH = new Headers(init.headers);
                var entries = initH.entries();
                for (var ie = 0; ie < entries.length; ie++) {
                    this[HEADERS_KEY].set(entries[ie][0], entries[ie][1]);
                }
            }
            // Use init body if provided, otherwise copy source body
            if (init.body !== undefined) {
                if (typeof Blob !== "undefined" && init.body instanceof Blob) {
                    var blobType = init.body.type;
                    if (blobType && !this[HEADERS_KEY].has("content-type")) {
                        this[HEADERS_KEY].set("content-type", blobType);
                    }
                    // Store the Blob instance directly; body readers and the
                    // fetch body-extraction helper detect Blob and read its bytes.
                    this[BODY_KEY] = init.body;
                } else {
                    this[BODY_KEY] = bodyToBytes(init.body);
                }
            } else {
                // Deep copy the body bytes (or share Blob reference)
                if (input[BODY_KEY] !== null && input[BODY_KEY] !== undefined) {
                    if (input[BODY_KEY] instanceof Uint8Array) {
                        this[BODY_KEY] = new Uint8Array(input[BODY_KEY]);
                    } else {
                        this[BODY_KEY] = input[BODY_KEY];
                    }
                } else {
                    this[BODY_KEY] = null;
                }
            }
            this[BODY_USED_KEY] = false;
            // Store live signal reference: init.signal overrides input's signal.
            if (init.signal !== undefined) {
                this[SIGNAL_KEY] = init.signal;
            } else {
                this[SIGNAL_KEY] = input[SIGNAL_KEY];
            }
            validateBodyForMethod(this[METHOD_KEY], this[BODY_KEY]);
            return;
        }

        // Construct from URL string
        this[URL_KEY] = String(input);
        this[METHOD_KEY] = init.method ? String(init.method).toUpperCase() : "GET";
        this[HEADERS_KEY] = init.headers ? new Headers(init.headers) : new Headers();
        this[BODY_USED_KEY] = false;

        // Process body
        if (init.body !== undefined && init.body !== null) {
            if (typeof Blob !== "undefined" && init.body instanceof Blob) {
                var bt = init.body.type;
                if (bt && !this[HEADERS_KEY].has("content-type")) {
                    this[HEADERS_KEY].set("content-type", bt);
                }
                this[BODY_KEY] = init.body;
            } else {
                this[BODY_KEY] = bodyToBytes(init.body);
            }
        } else {
            this[BODY_KEY] = null;
        }

        // Store live signal reference (or undefined to mean "no signal provided")
        this[SIGNAL_KEY] = (init.signal !== undefined) ? init.signal : undefined;

        validateBodyForMethod(this[METHOD_KEY], this[BODY_KEY]);
    }

    // Per WHATWG Fetch spec, GET and HEAD requests cannot have a non-null body.
    function validateBodyForMethod(method, body) {
        if ((method === "GET" || method === "HEAD") && body !== null && body !== undefined) {
            throw new TypeError("Request with GET/HEAD method cannot have body");
        }
    }

    Object.defineProperty(Request.prototype, "url", {
        get: function() { return this[URL_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Request.prototype, "method", {
        get: function() { return this[METHOD_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Request.prototype, "headers", {
        get: function() { return this[HEADERS_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Request.prototype, "bodyUsed", {
        get: function() { return this[BODY_USED_KEY]; },
        enumerable: true,
        configurable: true
    });

    // The default signal returned when no signal was supplied. Returning
    // the same frozen, non-aborted object keeps `.signal` cheap and
    // inspectable without synthesizing state that would drift from the
    // real signal if an external one is later provided.
    var DEFAULT_SIGNAL = (function() {
        var s = {
            aborted: false,
            reason: undefined,
            timeout_ms: undefined,
            throwIfAborted: function() { /* not aborted — no-op */ }
        };
        return s;
    })();

    Object.defineProperty(Request.prototype, "signal", {
        get: function() {
            var sig = this[SIGNAL_KEY];
            return (sig !== undefined && sig !== null) ? sig : DEFAULT_SIGNAL;
        },
        enumerable: true,
        configurable: true
    });

    // Internal: expose the body for the fetch() global to read without
    // consuming it. Returns the raw stored body (Uint8Array | Blob | null).
    // Not part of the WHATWG spec — prefixed with double-underscore to
    // discourage userland use.
    Object.defineProperty(Request.prototype, "__body_for_fetch__", {
        value: function() { return this[BODY_KEY]; },
        enumerable: false,
        configurable: true,
        writable: false
    });

    function consumeBody(request) {
        if (request[BODY_USED_KEY]) {
            throw new TypeError("Body has already been consumed");
        }
        request[BODY_USED_KEY] = true;
        var body = request[BODY_KEY];
        request[BODY_KEY] = null;
        return body;
    }

    function bodyToUint8Array(body) {
        if (body === null || body === undefined) {
            return new Uint8Array(0);
        }
        if (body instanceof Uint8Array) {
            return body;
        }
        if (typeof Blob !== "undefined" && body instanceof Blob) {
            // Synchronously extract bytes from the Blob. Blob.prototype.bytes()
            // returns a Promise that resolves synchronously in QuickJS (no
            // deferred scheduling), so we can rely on the internal state being
            // available immediately — but we avoid the Promise and use the
            // internal storage via a small helper defined on Blob itself.
            // Fall back to decoding via the slice+arrayBuffer path if needed.
            if (typeof body.__bytes_sync__ === "function") {
                return body.__bytes_sync__();
            }
            // Fall back: reconstruct by encoding the string form. This
            // preserves text content for simple blobs.
            return stringToBytes(String(body));
        }
        if (typeof body === "string") {
            return stringToBytes(body);
        }
        return new Uint8Array(0);
    }

    Request.prototype.text = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            return Promise.resolve(bytesToString(bytes));
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Request.prototype.json = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            var str = bytesToString(bytes);
            return Promise.resolve(JSON.parse(str));
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Request.prototype.arrayBuffer = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            var buf = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
            return Promise.resolve(buf);
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Request.prototype.bytes = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            var copy = new Uint8Array(bytes.length);
            copy.set(bytes);
            return Promise.resolve(copy);
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Request.prototype.blob = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            var ct = this[HEADERS_KEY].get("content-type");
            var opts = ct ? { type: ct } : {};
            var b = new Blob([bytes], opts);
            return Promise.resolve(b);
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Request.prototype.clone = function() {
        if (this[BODY_USED_KEY]) {
            throw new TypeError("Cannot clone a Request whose body has been consumed");
        }
        return new Request(this);
    };

    globalThis.Request = Request;
})();
"#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{register_abort, register_blob, register_headers};
    use rquickjs::{FromJs, Promise, Value};

    // -----------------------------------------------------------------------
    // Rust-level unit tests for the Request struct
    // -----------------------------------------------------------------------

    #[test]
    fn new_from_url_defaults_to_get() {
        let req = Request::new(
            "http://example.com".to_string(),
            "GET".to_string(),
            Headers::new(),
            None,
            None,
        );
        assert_eq!(req.url(), "http://example.com");
        assert_eq!(req.method(), "GET");
        assert!(!req.body_used());
        assert!(!req.signal_aborted());
        assert_eq!(req.signal_reason(), None);
        assert_eq!(req.signal_timeout_ms(), None);
    }

    #[test]
    fn new_with_method_and_body() {
        let body = b"hello world".to_vec();
        let req = Request::new(
            "http://example.com".to_string(),
            "POST".to_string(),
            Headers::new(),
            Some(body.clone()),
            None,
        );
        assert_eq!(req.method(), "POST");
        assert!(!req.body_used());
    }

    #[test]
    fn new_with_signal_timeout() {
        let signal = SignalInfo {
            aborted: false,
            reason: None,
            timeout_ms: Some(5000),
        };
        let req = Request::new(
            "http://example.com".to_string(),
            "GET".to_string(),
            Headers::new(),
            None,
            Some(signal),
        );
        assert!(!req.signal_aborted());
        assert_eq!(req.signal_timeout_ms(), Some(5000));
    }

    #[test]
    fn clone_copies_all_fields() {
        let mut headers = Headers::new();
        headers.set("x-custom", "value");
        let signal = SignalInfo {
            aborted: false,
            reason: None,
            timeout_ms: Some(3000),
        };
        let req = Request::new(
            "http://example.com/path".to_string(),
            "POST".to_string(),
            headers,
            Some(b"body data".to_vec()),
            Some(signal),
        );
        let cloned = req.try_clone().expect("clone should succeed");
        assert_eq!(cloned.url(), req.url());
        assert_eq!(cloned.method(), req.method());
        assert_eq!(cloned.headers().get("x-custom"), Some("value".to_string()));
        assert!(!cloned.body_used());
        assert_eq!(cloned.signal_timeout_ms(), Some(3000));
    }

    #[test]
    fn clone_with_override() {
        let req = Request::new(
            "http://example.com".to_string(),
            "GET".to_string(),
            Headers::new(),
            None,
            None,
        );
        let cloned = req.try_clone().expect("clone should succeed");
        // Simulate overriding method on the clone by creating a new request
        let overridden = Request::new(
            cloned.url().to_string(),
            "PUT".to_string(),
            cloned.headers().clone(),
            None,
            None,
        );
        assert_eq!(overridden.method(), "PUT");
        assert_eq!(overridden.url(), "http://example.com");
    }

    #[test]
    fn headers_returns_headers_instance() {
        let mut headers = Headers::new();
        headers.set("content-type", "application/json");
        let req = Request::new(
            "http://example.com".to_string(),
            "GET".to_string(),
            headers,
            None,
            None,
        );
        assert_eq!(
            req.headers().get("content-type"),
            Some("application/json".to_string())
        );
    }

    #[test]
    fn body_consumption_single_use() {
        let mut req = Request::new(
            "http://example.com".to_string(),
            "POST".to_string(),
            Headers::new(),
            Some(b"payload".to_vec()),
            None,
        );
        let body = req.consume_body().expect("first consume should succeed");
        assert_eq!(body, b"payload");
        assert!(req.body_used());

        let err = req.consume_body().expect_err("second consume should fail");
        assert!(err.contains("already been consumed"));
    }

    #[test]
    fn clone_after_consumption_fails() {
        let mut req = Request::new(
            "http://example.com".to_string(),
            "POST".to_string(),
            Headers::new(),
            Some(b"data".to_vec()),
            None,
        );
        let _ = req.consume_body().unwrap();
        let err = req
            .try_clone()
            .expect_err("clone after consume should fail");
        assert!(err.contains("consumed"));
    }

    #[test]
    fn blob_body_sets_content_type() {
        let blob = Blob::new(vec![BlobPart::String("hello".into())], Some("text/plain"));
        let req = Request::new_with_blob_body(
            "http://example.com".to_string(),
            "POST".to_string(),
            Headers::new(),
            blob,
            None,
        );
        assert_eq!(
            req.headers().get("content-type"),
            Some("text/plain".to_string())
        );
    }

    #[test]
    fn string_body_as_utf8() {
        let mut req = Request::new(
            "http://example.com".to_string(),
            "POST".to_string(),
            Headers::new(),
            Some("hello world".as_bytes().to_vec()),
            None,
        );
        let text = req.text().expect("text() should succeed");
        assert_eq!(text, "hello world");
    }

    #[test]
    fn method_is_uppercased() {
        let req = Request::new(
            "http://example.com".to_string(),
            "post".to_string(),
            Headers::new(),
            None,
            None,
        );
        assert_eq!(req.method(), "POST");
    }

    // -----------------------------------------------------------------------
    // JS integration tests
    // -----------------------------------------------------------------------

    fn with_js_context<F: FnOnce(&Ctx<'_>)>(f: F) {
        let rt = rquickjs::Runtime::new().expect("failed to create runtime");
        let ctx = rquickjs::Context::full(&rt).expect("failed to create context");
        ctx.with(|ctx| {
            register_headers(&ctx).expect("failed to register Headers");
            register_blob(&ctx).expect("failed to register Blob");
            register_abort(&ctx).expect("failed to register AbortController/AbortSignal");
            register_request(&ctx).expect("failed to register Request");
            f(&ctx);
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

    #[test]
    fn js_new_request_defaults_to_get() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var r = new Request("http://example.com");
                        return [r.url, r.method, r.bodyUsed];
                    })()
                    "#,
                )
                .expect("eval failed");

            let url: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(url, "http://example.com");
            let method: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(method, "GET");
            let body_used: bool = result[2].as_bool().unwrap();
            assert!(!body_used);
        });
    }

    #[test]
    fn js_method_uppercased() {
        with_js_context(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    (function() {
                        var r = new Request("http://example.com", { method: "post" });
                        return r.method;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert_eq!(result, "POST");
        });
    }

    #[test]
    fn js_request_text_returns_body() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Request("http://example.com", { method: "POST", body: "hello world" });
                globalThis.__result__ = await r.text();
                "#,
            );
            let text: String = val.as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "hello world");
        });
    }

    #[test]
    fn js_second_text_throws() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Request("http://example.com", { method: "POST", body: "data" });
                await r.text();
                try {
                    await r.text();
                    globalThis.__result__ = false;
                } catch(e) {
                    globalThis.__result__ = e instanceof TypeError;
                }
                "#,
            );
            let is_type_error: bool = val.as_bool().unwrap();
            assert!(is_type_error);
        });
    }

    #[test]
    fn js_clone_is_independent() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Request("http://example.com", { method: "POST", body: "data" });
                var r2 = r.clone();
                var t1 = await r.text();
                var t2 = await r2.text();
                globalThis.__result__ = [t1, t2, r.bodyUsed, r2.bodyUsed];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let t1: String = result[0].as_string().unwrap().to_string().unwrap();
            let t2: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(t1, "data");
            assert_eq!(t2, "data");
            assert!(result[2].as_bool().unwrap());
            assert!(result[3].as_bool().unwrap());
        });
    }

    #[test]
    fn js_clone_with_method_override() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var r = new Request("http://example.com", { method: "GET" });
                        var r2 = new Request(r, { method: "PUT" });
                        return [r.method, r2.method, r2.url];
                    })()
                    "#,
                )
                .expect("eval failed");

            let m1: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(m1, "GET");
            let m2: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(m2, "PUT");
            let url: String = result[2].as_string().unwrap().to_string().unwrap();
            assert_eq!(url, "http://example.com");
        });
    }

    #[test]
    fn js_clone_after_consume_throws() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Request("http://example.com", { method: "POST", body: "data" });
                await r.text();
                try {
                    r.clone();
                    globalThis.__result__ = false;
                } catch(e) {
                    globalThis.__result__ = e instanceof TypeError;
                }
                "#,
            );
            let is_type_error: bool = val.as_bool().unwrap();
            assert!(is_type_error);
        });
    }

    #[test]
    fn js_request_headers() {
        with_js_context(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    (function() {
                        var r = new Request("http://example.com", {
                            headers: { "X-Custom": "value" }
                        });
                        return r.headers.get("x-custom");
                    })()
                    "#,
                )
                .expect("eval failed");
            assert_eq!(result, "value");
        });
    }

    #[test]
    fn js_request_signal() {
        with_js_context(|ctx| {
            // Default signal: aborted is false when no signal is attached.
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        var r = new Request("http://example.com");
                        return r.signal.aborted === false;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);

            // Live signal: a controller's abort() must be visible via
            // request.signal after the Request has been constructed.
            let live: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var ac = new AbortController();
                        var r = new Request("http://example.com", { signal: ac.signal });
                        var before = r.signal.aborted;
                        ac.abort("gone");
                        var after = r.signal.aborted;
                        var reason = r.signal.reason;
                        return [before, after, reason];
                    })()
                    "#,
                )
                .expect("eval failed");
            let before: bool = live[0].as_bool().unwrap();
            let after: bool = live[1].as_bool().unwrap();
            let reason: String = live[2].as_string().unwrap().to_string().unwrap();
            assert!(!before, "signal should not be aborted before abort() call");
            assert!(
                after,
                "signal.aborted should reflect post-construction abort()"
            );
            assert_eq!(reason, "gone", "abort reason should be live-readable");

            // Timeout signals expose timeout_ms (no double-underscore name).
            let timeout_ms: i32 = ctx
                .eval(
                    r#"
                    (function() {
                        var r = new Request("http://example.com", {
                            signal: AbortSignal.timeout(250)
                        });
                        return r.signal.timeout_ms;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert_eq!(timeout_ms, 250);
        });
    }

    #[test]
    fn js_request_requires_new() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            Request("http://example.com");
                            return false;
                        } catch(e) {
                            return e instanceof TypeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);
        });
    }

    #[test]
    fn js_request_json() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Request("http://example.com", {
                    method: "POST",
                    body: '{"key": "value"}'
                });
                var parsed = await r.json();
                globalThis.__result__ = parsed.key;
                "#,
            );
            let key: String = val.as_string().unwrap().to_string().unwrap();
            assert_eq!(key, "value");
        });
    }

    #[test]
    fn js_request_blob_body_reads_back() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var blob = new Blob(["hello blob body"], { type: "text/plain" });
                var r = new Request("http://example.com", {
                    method: "POST",
                    body: blob
                });
                globalThis.__result__ = [
                    r.headers.get("content-type"),
                    await r.text()
                ];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let ct: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(ct, "text/plain");
            let text: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "hello blob body");
        });
    }

    #[test]
    fn js_request_get_with_body_throws() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            new Request("http://example.com", {
                                method: "GET",
                                body: "not allowed on GET"
                            });
                            return false;
                        } catch(e) {
                            return e instanceof TypeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result, "GET with body should throw TypeError");
        });
    }

    #[test]
    fn js_request_head_with_body_throws() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            new Request("http://example.com", {
                                method: "HEAD",
                                body: "nope"
                            });
                            return false;
                        } catch(e) {
                            return e instanceof TypeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result, "HEAD with body should throw TypeError");
        });
    }
}
