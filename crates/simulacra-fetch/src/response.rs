//! WHATWG-compliant `Response` implementation.
//!
//! Provides a Rust `Response` struct and a JS class registration function that
//! installs a WHATWG-compliant `Response` constructor into a QuickJS context.

use rquickjs::Ctx;

use crate::{Blob, BlobPart, Headers};

/// A WHATWG-compliant Response.
///
/// Holds HTTP status information, headers, optional body bytes, body-consumption
/// tracking, URL, redirect state, and response type.
#[derive(Debug, Clone)]
pub struct Response {
    status: u16,
    status_text: String,
    headers: Headers,
    body: Option<Vec<u8>>,
    body_used: bool,
    url: String,
    redirected: bool,
    response_type: String,
}

impl Response {
    /// Create a new `Response` with all fields specified.
    pub fn new(
        status: u16,
        status_text: String,
        headers: Headers,
        body: Option<Vec<u8>>,
        url: String,
        redirected: bool,
        response_type: String,
    ) -> Self {
        Self {
            status,
            status_text,
            headers,
            body,
            body_used: false,
            url,
            redirected,
            response_type,
        }
    }

    /// Create a `Response` from a body and optional init parameters.
    ///
    /// Simulates the JS constructor: `new Response("body", { status, statusText, headers })`.
    pub fn from_body(
        body: Option<Vec<u8>>,
        status: Option<u16>,
        status_text: Option<String>,
        headers: Option<Headers>,
    ) -> Self {
        Self {
            status: status.unwrap_or(200),
            status_text: status_text.unwrap_or_default(),
            headers: headers.unwrap_or_default(),
            body,
            body_used: false,
            url: String::new(),
            redirected: false,
            response_type: "basic".to_string(),
        }
    }

    /// Create a `Response` with JSON-serialized body and `Content-Type: application/json`.
    ///
    /// Equivalent to the static `Response.json(data, init)` in the WHATWG spec.
    pub fn new_json(
        data: &serde_json::Value,
        status: Option<u16>,
        status_text: Option<String>,
        headers: Option<Headers>,
    ) -> Self {
        let json_bytes = serde_json::to_vec(data).unwrap_or_default();
        let mut h = headers.unwrap_or_default();
        if !h.has("content-type") {
            h.set("content-type", "application/json");
        }
        Self {
            status: status.unwrap_or(200),
            status_text: status_text.unwrap_or_default(),
            headers: h,
            body: Some(json_bytes),
            body_used: false,
            url: String::new(),
            redirected: false,
            response_type: "basic".to_string(),
        }
    }

    /// Create an error `Response` with status 0 and type "error".
    ///
    /// Equivalent to the static `Response.error()` in the WHATWG spec.
    pub fn error() -> Self {
        Self {
            status: 0,
            status_text: String::new(),
            headers: Headers::new(),
            body: None,
            body_used: false,
            url: String::new(),
            redirected: false,
            response_type: "error".to_string(),
        }
    }

    /// Create a redirect `Response` with a `Location` header.
    ///
    /// Equivalent to the static `Response.redirect(url, status)` in the WHATWG spec.
    /// Returns `Err` if `status` is not a valid redirect status (301, 302, 303, 307, 308).
    pub fn redirect(url: &str, status: Option<u16>) -> Result<Self, String> {
        let status = status.unwrap_or(302);
        if ![301, 302, 303, 307, 308].contains(&status) {
            return Err(format!("Invalid redirect status: {status}"));
        }
        let mut headers = Headers::new();
        headers.set("location", url);
        Ok(Self {
            status,
            status_text: String::new(),
            headers,
            body: None,
            body_used: false,
            url: String::new(),
            redirected: false,
            response_type: "basic".to_string(),
        })
    }

    /// The HTTP status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The HTTP status text.
    pub fn status_text(&self) -> &str {
        &self.status_text
    }

    /// Whether the status is in the 200-299 range.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The response headers.
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// The response URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether the response was the result of a redirect.
    pub fn redirected(&self) -> bool {
        self.redirected
    }

    /// The response type (e.g. "basic" or "error").
    pub fn response_type(&self) -> &str {
        &self.response_type
    }

    /// Whether the body has already been consumed.
    pub fn body_used(&self) -> bool {
        self.body_used
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

    /// Consume the body and wrap it in a `Blob` with the response's
    /// `Content-Type` header.
    pub fn blob(&mut self) -> Result<Blob, String> {
        let bytes = self.consume_body()?;
        let ct = self.headers.get("content-type");
        Ok(Blob::new(vec![BlobPart::Bytes(bytes)], ct.as_deref()))
    }

    /// Clone the response. Fails if the body has been consumed.
    pub fn try_clone(&self) -> Result<Response, String> {
        if self.body_used {
            return Err("Cannot clone a Response whose body has been consumed".to_string());
        }
        Ok(Response {
            status: self.status,
            status_text: self.status_text.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            body_used: false,
            url: self.url.clone(),
            redirected: self.redirected,
            response_type: self.response_type.clone(),
        })
    }
}

/// Register the `Response` class as a JS global.
///
/// After calling this, JS code can use:
/// - `new Response("body")` — 200 OK with string body
/// - `new Response("body", { status: 201, statusText: "Created", headers: {...} })`
/// - `new Response(null)` — empty body
/// - `Response.json({ key: "value" })`, `Response.json(data, { status: 201 })`
/// - `Response.error()` — status 0, type "error"
/// - `Response.redirect("http://example.com")`, `Response.redirect(url, 301)`
/// - `.status`, `.statusText`, `.ok`, `.headers`, `.url`, `.redirected`, `.type`,
///   `.body`, `.bodyUsed` (read-only)
/// - `.text()`, `.json()`, `.arrayBuffer()`, `.bytes()`, `.blob()` — Promises, single-consumption
/// - `.clone()` — deep copy, throws TypeError if body consumed
pub fn register_response(ctx: &Ctx<'_>) -> Result<(), rquickjs::Error> {
    ctx.eval::<(), _>(
        r#"
(function() {
    var STATUS_KEY = Symbol("__response_status__");
    var STATUS_TEXT_KEY = Symbol("__response_status_text__");
    var HEADERS_KEY = Symbol("__response_headers__");
    var BODY_KEY = Symbol("__response_body__");
    var BODY_USED_KEY = Symbol("__response_body_used__");
    var URL_KEY = Symbol("__response_url__");
    var REDIRECTED_KEY = Symbol("__response_redirected__");
    var TYPE_KEY = Symbol("__response_type__");

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
            // Read the Blob's bytes synchronously via the internal helper.
            if (typeof body.__bytes_sync__ === "function") {
                return body.__bytes_sync__();
            }
            return new Uint8Array(0);
        }
        // Fallback: convert to string
        return stringToBytes(String(body));
    }

    function Response(body, init) {
        if (!(this instanceof Response)) {
            throw new TypeError("Response constructor requires 'new'");
        }

        init = init || {};

        var status = (init.status !== undefined) ? Number(init.status) : 200;
        // Per WHATWG, Response status must be in [200, 599]. Reject outside.
        if (!Number.isFinite(status) || status < 200 || status > 599) {
            throw new RangeError("Response status must be between 200 and 599, got " + status);
        }

        this[STATUS_KEY] = status;
        this[STATUS_TEXT_KEY] = (init.statusText !== undefined) ? String(init.statusText) : "";
        this[HEADERS_KEY] = init.headers ? new Headers(init.headers) : new Headers();

        // If body is a Blob and no Content-Type header is set, use the blob's type.
        if (body !== null && body !== undefined
            && typeof Blob !== "undefined" && body instanceof Blob
            && body.type && !this[HEADERS_KEY].has("content-type")) {
            this[HEADERS_KEY].set("content-type", body.type);
        }

        this[BODY_KEY] = bodyToBytes(body);
        this[BODY_USED_KEY] = false;
        this[URL_KEY] = "";
        this[REDIRECTED_KEY] = false;
        this[TYPE_KEY] = "basic";
    }

    Object.defineProperty(Response.prototype, "status", {
        get: function() { return this[STATUS_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "statusText", {
        get: function() { return this[STATUS_TEXT_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "ok", {
        get: function() {
            var s = this[STATUS_KEY];
            return s >= 200 && s <= 299;
        },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "headers", {
        get: function() { return this[HEADERS_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "url", {
        get: function() { return this[URL_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "redirected", {
        get: function() { return this[REDIRECTED_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "type", {
        get: function() { return this[TYPE_KEY]; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "body", {
        get: function() { return null; },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Response.prototype, "bodyUsed", {
        get: function() { return this[BODY_USED_KEY]; },
        enumerable: true,
        configurable: true
    });

    function consumeBody(response) {
        if (response[BODY_USED_KEY]) {
            throw new TypeError("Body has already been consumed");
        }
        response[BODY_USED_KEY] = true;
        var body = response[BODY_KEY];
        response[BODY_KEY] = null;
        return body;
    }

    function bodyToUint8Array(body) {
        if (body === null || body === undefined) {
            return new Uint8Array(0);
        }
        if (body instanceof Uint8Array) {
            return body;
        }
        if (typeof body === "string") {
            return stringToBytes(body);
        }
        return new Uint8Array(0);
    }

    Response.prototype.text = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            return Promise.resolve(bytesToString(bytes));
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Response.prototype.json = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            var str = bytesToString(bytes);
            return Promise.resolve(JSON.parse(str));
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Response.prototype.arrayBuffer = function() {
        try {
            var body = consumeBody(this);
            var bytes = bodyToUint8Array(body);
            var buf = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
            return Promise.resolve(buf);
        } catch(e) {
            return Promise.reject(e);
        }
    };

    Response.prototype.bytes = function() {
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

    Response.prototype.blob = function() {
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

    Response.prototype.clone = function() {
        if (this[BODY_USED_KEY]) {
            throw new TypeError("Cannot clone a Response whose body has been consumed");
        }
        var cloned = new Response();
        cloned[STATUS_KEY] = this[STATUS_KEY];
        cloned[STATUS_TEXT_KEY] = this[STATUS_TEXT_KEY];
        cloned[HEADERS_KEY] = new Headers(this[HEADERS_KEY]);
        if (this[BODY_KEY] !== null && this[BODY_KEY] !== undefined) {
            if (this[BODY_KEY] instanceof Uint8Array) {
                cloned[BODY_KEY] = new Uint8Array(this[BODY_KEY]);
            } else {
                cloned[BODY_KEY] = this[BODY_KEY];
            }
        } else {
            cloned[BODY_KEY] = null;
        }
        cloned[BODY_USED_KEY] = false;
        cloned[URL_KEY] = this[URL_KEY];
        cloned[REDIRECTED_KEY] = this[REDIRECTED_KEY];
        cloned[TYPE_KEY] = this[TYPE_KEY];
        return cloned;
    };

    // Static method: Response.json(data, init?)
    Response.json = function(data, init) {
        init = init || {};
        var jsonStr = JSON.stringify(data);
        var headers = init.headers ? new Headers(init.headers) : new Headers();
        if (!headers.has("content-type")) {
            headers.set("content-type", "application/json");
        }
        var resp = new Response(jsonStr, {
            status: init.status,
            statusText: init.statusText,
            headers: headers
        });
        return resp;
    };

    // Static method: Response.error()
    Response.error = function() {
        var resp = new Response(null);
        resp[STATUS_KEY] = 0;
        resp[STATUS_TEXT_KEY] = "";
        resp[TYPE_KEY] = "error";
        return resp;
    };

    // Static method: Response.redirect(url, status?)
    Response.redirect = function(url, status) {
        if (status === undefined || status === null) {
            status = 302;
        }
        status = Number(status);
        var validStatuses = [301, 302, 303, 307, 308];
        var valid = false;
        for (var i = 0; i < validStatuses.length; i++) {
            if (validStatuses[i] === status) {
                valid = true;
                break;
            }
        }
        if (!valid) {
            throw new RangeError("Invalid redirect status: " + status);
        }
        var resp = new Response(null, { status: status });
        resp[HEADERS_KEY].set("location", String(url));
        return resp;
    };

    globalThis.Response = Response;
})();
"#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{register_blob, register_headers};
    use rquickjs::{FromJs, Promise, Value};

    // -----------------------------------------------------------------------
    // Rust-level unit tests for the Response struct
    // -----------------------------------------------------------------------

    #[test]
    fn new_response_with_body_and_status() {
        let resp = Response::new(
            201,
            "Created".to_string(),
            Headers::new(),
            Some(b"hello".to_vec()),
            "http://example.com".to_string(),
            false,
            "basic".to_string(),
        );
        assert_eq!(resp.status(), 201);
        assert_eq!(resp.status_text(), "Created");
        assert!(!resp.body_used());
        assert_eq!(resp.url(), "http://example.com");
        assert!(!resp.redirected());
        assert_eq!(resp.response_type(), "basic");
    }

    #[test]
    fn response_json_static() {
        let data = serde_json::json!({"key": "value"});
        let mut resp = Response::new_json(&data, None, None, None);
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type"),
            Some("application/json".to_string())
        );
        let text = resp.text().expect("text() should succeed");
        assert_eq!(text, r#"{"key":"value"}"#);
    }

    #[test]
    fn response_error_static() {
        let resp = Response::error();
        assert_eq!(resp.status(), 0);
        assert_eq!(resp.response_type(), "error");
        assert!(!resp.ok());
    }

    #[test]
    fn response_redirect_static() {
        let resp = Response::redirect("http://example.com", None).expect("redirect should succeed");
        assert_eq!(resp.status(), 302);
        assert_eq!(
            resp.headers().get("location"),
            Some("http://example.com".to_string())
        );
    }

    #[test]
    fn response_redirect_invalid_status() {
        let err = Response::redirect("http://example.com", Some(200))
            .expect_err("should fail for status 200");
        assert!(err.contains("Invalid redirect status"));
    }

    #[test]
    fn ok_true_for_200_range() {
        for status in [200, 204, 299] {
            let resp = Response::from_body(None, Some(status), None, None);
            assert!(resp.ok(), "status {status} should be ok");
        }
    }

    #[test]
    fn ok_false_outside_range() {
        for status in [199, 300, 404, 500] {
            let resp = Response::from_body(None, Some(status), None, None);
            assert!(!resp.ok(), "status {status} should not be ok");
        }
    }

    #[test]
    fn body_null() {
        // The body property is conceptually null (we don't implement ReadableStream).
        // Verify response_type is correct for a normal response (not the body field).
        let resp = Response::from_body(None, None, None, None);
        assert_eq!(resp.response_type(), "basic");
    }

    #[test]
    fn type_basic_for_normal() {
        let resp = Response::from_body(None, None, None, None);
        assert_eq!(resp.response_type(), "basic");
    }

    #[test]
    fn type_error_for_error_response() {
        let resp = Response::error();
        assert_eq!(resp.response_type(), "error");
    }

    #[test]
    fn body_consumption_single_use() {
        let mut resp = Response::from_body(Some(b"payload".to_vec()), None, None, None);
        let body = resp.consume_body().expect("first consume should succeed");
        assert_eq!(body, b"payload");
        assert!(resp.body_used());

        let err = resp.consume_body().expect_err("second consume should fail");
        assert!(err.contains("already been consumed"));
    }

    #[test]
    fn clone_produces_independent_copy() {
        let resp = Response::new(
            200,
            "OK".to_string(),
            Headers::from_pairs(vec![("x-custom".into(), "value".into())]),
            Some(b"data".to_vec()),
            "http://example.com".to_string(),
            false,
            "basic".to_string(),
        );
        let mut cloned = resp.try_clone().expect("clone should succeed");
        assert_eq!(cloned.status(), resp.status());
        assert_eq!(cloned.url(), resp.url());
        assert_eq!(cloned.headers().get("x-custom"), Some("value".to_string()));
        // Consuming the clone should not affect the original
        let text = cloned.text().expect("text() should succeed");
        assert_eq!(text, "data");
        assert!(!resp.body_used());
        assert!(cloned.body_used());
    }

    #[test]
    fn clone_after_consumption_fails() {
        let mut resp = Response::from_body(Some(b"data".to_vec()), None, None, None);
        let _ = resp.consume_body().unwrap();
        let err = resp
            .try_clone()
            .expect_err("clone after consume should fail");
        assert!(err.contains("consumed"));
    }

    #[test]
    fn blob_returns_blob_with_content_type() {
        let mut headers = Headers::new();
        headers.set("content-type", "text/plain");
        let mut resp = Response::from_body(Some(b"hello".to_vec()), None, None, Some(headers));
        let blob = resp.blob().expect("blob() should succeed");
        assert_eq!(blob.size(), 5);
        assert_eq!(blob.type_(), "text/plain");
        assert_eq!(blob.text(), "hello");
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
            register_response(&ctx).expect("failed to register Response");
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
    fn js_new_response_with_body_and_status() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Response("hello", { status: 200 });
                var text = await r.text();
                globalThis.__result__ = [r.status, r.ok, text];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let status: i32 = result[0].as_int().unwrap();
            assert_eq!(status, 200);
            let ok: bool = result[1].as_bool().unwrap();
            assert!(ok);
            let text: String = result[2].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "hello");
        });
    }

    #[test]
    fn js_response_json_static() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = Response.json({ a: 1 });
                var body = await r.json();
                globalThis.__result__ = [String(body.a), r.headers.get("content-type")];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let a: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(a, "1");
            let ct: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(ct, "application/json");
        });
    }

    #[test]
    fn js_response_error_static() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var r = Response.error();
                        return [r.status, r.type];
                    })()
                    "#,
                )
                .expect("eval failed");

            let status: i32 = result[0].as_int().unwrap();
            assert_eq!(status, 0);
            let rtype: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(rtype, "error");
        });
    }

    #[test]
    fn js_response_redirect_static() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var r = Response.redirect("http://example.com", 301);
                        return [r.status, r.headers.get("location")];
                    })()
                    "#,
                )
                .expect("eval failed");

            let status: i32 = result[0].as_int().unwrap();
            assert_eq!(status, 301);
            let location: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(location, "http://example.com");
        });
    }

    #[test]
    fn js_response_redirect_default_302() {
        with_js_context(|ctx| {
            let result: i32 = ctx
                .eval(
                    r#"
                    (function() {
                        var r = Response.redirect("http://example.com");
                        return r.status;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert_eq!(result, 302);
        });
    }

    #[test]
    fn js_response_redirect_invalid_status() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            Response.redirect("http://example.com", 200);
                            return false;
                        } catch(e) {
                            return e instanceof RangeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);
        });
    }

    #[test]
    fn js_response_body_is_null() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        var r = new Response("hello");
                        return r.body === null;
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(result);
        });
    }

    #[test]
    fn js_second_text_throws() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Response("data");
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
                var r = new Response("data", { status: 201, statusText: "Created" });
                var r2 = r.clone();
                var t1 = await r.text();
                var t2 = await r2.text();
                globalThis.__result__ = [t1, t2, r.bodyUsed, r2.bodyUsed, r2.status, r2.statusText];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let t1: String = result[0].as_string().unwrap().to_string().unwrap();
            let t2: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(t1, "data");
            assert_eq!(t2, "data");
            assert!(result[2].as_bool().unwrap());
            assert!(result[3].as_bool().unwrap());
            let status: i32 = result[4].as_int().unwrap();
            assert_eq!(status, 201);
            let status_text: String = result[5].as_string().unwrap().to_string().unwrap();
            assert_eq!(status_text, "Created");
        });
    }

    #[test]
    fn js_clone_after_consume_throws() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Response("data");
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
    fn js_response_blob_with_content_type() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Response("hello", {
                    headers: { "Content-Type": "text/plain" }
                });
                var blob = await r.blob();
                var text = await blob.text();
                globalThis.__result__ = [blob.size, blob.type, text];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let size: i32 = result[0].as_int().unwrap();
            assert_eq!(size, 5);
            let mime: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(mime, "text/plain");
            let text: String = result[2].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "hello");
        });
    }

    #[test]
    fn js_response_requires_new() {
        with_js_context(|ctx| {
            let result: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            Response("test");
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
    fn js_response_null_body() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var r = new Response(null);
                var text = await r.text();
                globalThis.__result__ = [text, r.bodyUsed];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let text: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "");
            assert!(result[1].as_bool().unwrap());
        });
    }

    #[test]
    fn js_response_with_blob_body() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var blob = new Blob(["response-from-blob"], { type: "text/plain" });
                var r = new Response(blob);
                globalThis.__result__ = [
                    r.headers.get("content-type"),
                    await r.text()
                ];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let ct: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(
                ct, "text/plain",
                "content-type should be derived from blob.type"
            );
            let text: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "response-from-blob");
        });
    }

    #[test]
    fn js_response_status_out_of_range_throws() {
        with_js_context(|ctx| {
            let too_low: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            new Response(null, { status: 100 });
                            return false;
                        } catch(e) {
                            return e instanceof RangeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(too_low, "status 100 should throw RangeError");

            let too_high: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            new Response(null, { status: 600 });
                            return false;
                        } catch(e) {
                            return e instanceof RangeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(too_high, "status 600 should throw RangeError");
        });
    }

    #[test]
    fn js_response_json_status_out_of_range_throws() {
        with_js_context(|ctx| {
            let too_low: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            Response.json({a: 1}, { status: 100 });
                            return false;
                        } catch(e) {
                            return e instanceof RangeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(too_low, "Response.json status 100 should throw RangeError");
        });
    }
}
