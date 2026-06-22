//! WHATWG-compliant `Blob` implementation.
//!
//! Stores binary data with an associated MIME type. Supports construction from
//! heterogeneous parts (strings, byte arrays, other blobs), slicing, and
//! conversion to text/bytes.

use rquickjs::Ctx;

/// The types of parts that can compose a `Blob`.
#[derive(Debug, Clone)]
pub enum BlobPart {
    /// A UTF-8 string part.
    String(String),
    /// A raw byte buffer part (e.g., from an ArrayBuffer or TypedArray).
    Bytes(Vec<u8>),
    /// Another blob whose bytes are included inline.
    Blob(Blob),
}

/// A WHATWG-compliant Blob.
///
/// Holds an immutable byte buffer and a MIME type string. Constructed from a
/// sequence of [`BlobPart`]s that are concatenated in order.
#[derive(Debug, Clone)]
pub struct Blob {
    data: Vec<u8>,
    mime_type: String,
}

impl Blob {
    /// Create a new `Blob` from parts and an optional MIME type.
    ///
    /// Parts are concatenated in order. The MIME type is lowercased; if `None`
    /// or empty, it defaults to the empty string.
    pub fn new(parts: Vec<BlobPart>, mime_type: Option<&str>) -> Self {
        let mut data = Vec::new();
        for part in parts {
            match part {
                BlobPart::String(s) => data.extend_from_slice(s.as_bytes()),
                BlobPart::Bytes(b) => data.extend_from_slice(&b),
                BlobPart::Blob(blob) => data.extend_from_slice(&blob.data),
            }
        }
        let mime_type = mime_type.map(|s| s.to_lowercase()).unwrap_or_default();
        Self { data, mime_type }
    }

    /// Byte length of the blob's data.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// The MIME type string, lowercased. Empty string if not provided.
    pub fn type_(&self) -> &str {
        &self.mime_type
    }

    /// Decode the blob's bytes as UTF-8 text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    /// Return a clone of the blob's raw bytes (simulates `arrayBuffer()`).
    pub fn array_buffer(&self) -> Vec<u8> {
        self.data.clone()
    }

    /// Return a clone of the blob's raw bytes (simulates `bytes()`).
    pub fn bytes_(&self) -> Vec<u8> {
        self.data.clone()
    }

    /// Return a new `Blob` representing a byte sub-range.
    ///
    /// Negative indices are relative to the end. Out-of-range values are
    /// clamped. The optional `content_type` sets the new blob's type.
    pub fn slice(&self, start: Option<i64>, end: Option<i64>, content_type: Option<&str>) -> Blob {
        let len = self.data.len() as i64;

        let resolve = |val: i64| -> usize {
            let resolved = if val < 0 {
                (len + val).max(0)
            } else {
                val.min(len)
            };
            resolved as usize
        };

        let s = resolve(start.unwrap_or(0));
        let e = resolve(end.unwrap_or(len));

        let slice_data = if s < e {
            self.data[s..e].to_vec()
        } else {
            Vec::new()
        };

        let mime = content_type.map(|s| s.to_lowercase()).unwrap_or_default();

        Blob {
            data: slice_data,
            mime_type: mime,
        }
    }
}

/// Register the `Blob` class as a JS global.
///
/// After calling this, JS code can use:
/// - `new Blob(["hello"], { type: "text/plain" })`
/// - `new Blob([blob1, "extra"])`
/// - `new Blob([arrayBuffer])`
/// - `.size`, `.type` (read-only properties)
/// - `.text()` -> Promise\<string\>
/// - `.arrayBuffer()` -> Promise\<ArrayBuffer\>
/// - `.bytes()` -> Promise\<Uint8Array\>
/// - `.slice(start, end, contentType)` -> Blob
pub fn register_blob(ctx: &Ctx<'_>) -> Result<(), rquickjs::Error> {
    ctx.eval::<(), _>(
        r#"
(function() {
    var STORAGE = Symbol("__blob_data__");
    var MIME = Symbol("__blob_type__");

    function concatArrays(arrays) {
        var totalLen = 0;
        for (var i = 0; i < arrays.length; i++) {
            totalLen += arrays[i].length;
        }
        var result = new Uint8Array(totalLen);
        var offset = 0;
        for (var j = 0; j < arrays.length; j++) {
            // arrays[j] is already a Uint8Array (produced by partToBytes).
            result.set(arrays[j], offset);
            offset += arrays[j].length;
        }
        return result;
    }

    function partToBytes(part) {
        if (typeof part === "string") {
            // Encode string as UTF-8 bytes
            var bytes = [];
            for (var i = 0; i < part.length; i++) {
                var code = part.charCodeAt(i);
                if (code < 0x80) {
                    bytes.push(code);
                } else if (code < 0x800) {
                    bytes.push(0xC0 | (code >> 6));
                    bytes.push(0x80 | (code & 0x3F));
                } else if (code >= 0xD800 && code <= 0xDBFF) {
                    // Surrogate pair
                    var next = part.charCodeAt(++i);
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
        if (part instanceof Blob) {
            // Copy the blob's bytes so later mutations of the source don't
            // affect the new Blob.
            var src = new Uint8Array(part[STORAGE]);
            var out = new Uint8Array(src.length);
            out.set(src);
            return out;
        }
        if (part instanceof ArrayBuffer) {
            return new Uint8Array(part);
        }
        if (part instanceof Uint8Array) {
            // Honor byteOffset/byteLength so TypedArray views into a larger
            // backing buffer copy only the view's bytes.
            return new Uint8Array(part.buffer, part.byteOffset, part.byteLength).slice();
        }
        if (part && part.buffer instanceof ArrayBuffer
            && typeof part.byteOffset === "number"
            && typeof part.byteLength === "number") {
            // Generic TypedArray view (Int16Array, Float32Array, etc.): copy
            // the view's byte range, treating it as raw bytes.
            return new Uint8Array(part.buffer, part.byteOffset, part.byteLength).slice();
        }
        // Fallback: try to treat as string
        return partToBytes(String(part));
    }

    function Blob(parts, options) {
        if (!(this instanceof Blob)) {
            throw new TypeError("Blob constructor requires 'new'");
        }
        var allParts = [];
        if (parts && Array.isArray(parts)) {
            for (var i = 0; i < parts.length; i++) {
                allParts.push(partToBytes(parts[i]));
            }
        }

        if (allParts.length === 0) {
            this[STORAGE] = new Uint8Array(0).buffer;
        } else if (allParts.length === 1) {
            this[STORAGE] = allParts[0].buffer;
        } else {
            this[STORAGE] = concatArrays(allParts).buffer;
        }

        var type = "";
        if (options && typeof options === "object" && typeof options.type === "string") {
            type = options.type.toLowerCase();
        }
        this[MIME] = type;
    }

    Object.defineProperty(Blob.prototype, "size", {
        get: function() {
            return new Uint8Array(this[STORAGE]).length;
        },
        enumerable: true,
        configurable: true
    });

    Object.defineProperty(Blob.prototype, "type", {
        get: function() {
            return this[MIME];
        },
        enumerable: true,
        configurable: true
    });

    Blob.prototype.text = function() {
        var bytes = new Uint8Array(this[STORAGE]);
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
        return Promise.resolve(result);
    };

    Blob.prototype.arrayBuffer = function() {
        // Return a copy of the internal buffer
        var src = new Uint8Array(this[STORAGE]);
        var copy = new ArrayBuffer(src.length);
        var dst = new Uint8Array(copy);
        dst.set(src);
        return Promise.resolve(copy);
    };

    Blob.prototype.bytes = function() {
        var src = new Uint8Array(this[STORAGE]);
        var copy = new Uint8Array(src.length);
        copy.set(src);
        return Promise.resolve(copy);
    };

    // Internal synchronous accessor used by Request/Response body extraction.
    // Not part of the WHATWG spec. Returns a fresh Uint8Array view of the
    // Blob's bytes (a copy, so callers can't mutate internal storage).
    Object.defineProperty(Blob.prototype, "__bytes_sync__", {
        value: function() {
            var src = new Uint8Array(this[STORAGE]);
            var copy = new Uint8Array(src.length);
            copy.set(src);
            return copy;
        },
        enumerable: false,
        configurable: true,
        writable: false
    });

    Blob.prototype.slice = function(start, end, contentType) {
        var bytes = new Uint8Array(this[STORAGE]);
        var len = bytes.length;

        var s = (start === undefined || start === null) ? 0 : (start < 0 ? Math.max(len + start, 0) : Math.min(start, len));
        var e = (end === undefined || end === null) ? len : (end < 0 ? Math.max(len + end, 0) : Math.min(end, len));

        var sliced;
        if (s < e) {
            sliced = bytes.slice(s, e);
        } else {
            sliced = new Uint8Array(0);
        }

        var newBlob = new Blob();
        newBlob[STORAGE] = sliced.buffer;
        newBlob[MIME] = (contentType !== undefined && contentType !== null) ? String(contentType).toLowerCase() : "";
        return newBlob;
    };

    globalThis.Blob = Blob;
})();
"#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{FromJs, Promise, Value};

    // -----------------------------------------------------------------------
    // Rust-level unit tests for the Blob struct
    // -----------------------------------------------------------------------

    #[test]
    fn new_from_string() {
        let blob = Blob::new(vec![BlobPart::String("hello".into())], None);
        assert_eq!(blob.size(), 5);
        assert_eq!(blob.type_(), "");
    }

    #[test]
    fn new_with_type() {
        let blob = Blob::new(vec![BlobPart::String("hello".into())], Some("Text/Plain"));
        assert_eq!(blob.type_(), "text/plain");
    }

    #[test]
    fn concatenates_string_parts() {
        let blob = Blob::new(
            vec![
                BlobPart::String("hello".into()),
                BlobPart::String(" ".into()),
                BlobPart::String("world".into()),
            ],
            None,
        );
        assert_eq!(blob.size(), 11);
        assert_eq!(blob.text(), "hello world");
    }

    #[test]
    fn concatenates_blob_parts() {
        let inner = Blob::new(vec![BlobPart::String("inner".into())], None);
        let blob = Blob::new(
            vec![
                BlobPart::String("before-".into()),
                BlobPart::Blob(inner),
                BlobPart::String("-after".into()),
            ],
            None,
        );
        assert_eq!(blob.text(), "before-inner-after");
    }

    #[test]
    fn slice_returns_sub_blob() {
        let blob = Blob::new(vec![BlobPart::String("hello".into())], Some("text/plain"));
        let sliced = blob.slice(Some(1), Some(3), None);
        assert_eq!(sliced.text(), "el");
        assert_eq!(sliced.size(), 2);
        // slice without content_type gets empty type
        assert_eq!(sliced.type_(), "");
    }

    #[test]
    fn slice_with_content_type() {
        let blob = Blob::new(vec![BlobPart::String("hello".into())], Some("text/plain"));
        let sliced = blob.slice(Some(0), Some(5), Some("Application/JSON"));
        assert_eq!(sliced.type_(), "application/json");
        assert_eq!(sliced.text(), "hello");
    }

    #[test]
    fn text_returns_utf8() {
        let blob = Blob::new(vec![BlobPart::String("hello world".into())], None);
        assert_eq!(blob.text(), "hello world");
    }

    #[test]
    fn array_buffer_returns_bytes() {
        let blob = Blob::new(vec![BlobPart::String("abc".into())], None);
        assert_eq!(blob.array_buffer(), vec![0x61, 0x62, 0x63]);
    }

    #[test]
    fn bytes_returns_copy() {
        let blob = Blob::new(vec![BlobPart::String("abc".into())], None);
        let bytes = blob.bytes_();
        assert_eq!(bytes, vec![0x61, 0x62, 0x63]);
        // Verify it's a copy — mutating the returned vec shouldn't affect the blob
        assert_eq!(blob.size(), 3);
    }

    #[test]
    fn from_byte_vec_parts() {
        let blob = Blob::new(
            vec![BlobPart::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])],
            Some("application/octet-stream"),
        );
        assert_eq!(blob.size(), 4);
        assert_eq!(blob.array_buffer(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(blob.type_(), "application/octet-stream");
    }

    #[test]
    fn slice_negative_indices() {
        let blob = Blob::new(vec![BlobPart::String("hello".into())], None);
        // slice(-3) should give last 3 bytes: "llo"
        let sliced = blob.slice(Some(-3), None, None);
        assert_eq!(sliced.text(), "llo");
    }

    #[test]
    fn slice_out_of_range_clamped() {
        let blob = Blob::new(vec![BlobPart::String("hi".into())], None);
        let sliced = blob.slice(Some(-100), Some(100), None);
        assert_eq!(sliced.text(), "hi");
    }

    #[test]
    fn slice_start_greater_than_end_gives_empty() {
        let blob = Blob::new(vec![BlobPart::String("hello".into())], None);
        let sliced = blob.slice(Some(3), Some(1), None);
        assert_eq!(sliced.size(), 0);
        assert_eq!(sliced.text(), "");
    }

    #[test]
    fn empty_blob() {
        let blob = Blob::new(vec![], None);
        assert_eq!(blob.size(), 0);
        assert_eq!(blob.type_(), "");
        assert_eq!(blob.text(), "");
        assert!(blob.array_buffer().is_empty());
    }

    // -----------------------------------------------------------------------
    // JS integration tests
    // -----------------------------------------------------------------------

    fn with_js_context<F: FnOnce(&Ctx<'_>)>(f: F) {
        let rt = rquickjs::Runtime::new().expect("failed to create runtime");
        let ctx = rquickjs::Context::full(&rt).expect("failed to create context");
        ctx.with(|ctx| {
            register_blob(&ctx).expect("failed to register Blob");
            f(&ctx);
        });
    }

    /// Evaluate JS code that uses top-level `await` and resolve all promises.
    ///
    /// The code should assign the final result to `globalThis.__result__`.
    fn eval_async<'js>(ctx: &Ctx<'js>, code: &str) -> Value<'js> {
        // eval_promise sets JS_EVAL_FLAG_ASYNC which enables top-level await
        let promise: Promise<'js> = ctx.eval_promise(code).expect("eval_promise failed");
        promise
            .finish::<Value<'_>>()
            .expect("promise resolution failed");
        ctx.eval("globalThis.__result__")
            .expect("failed to read __result__")
    }

    #[test]
    fn js_blob_construct_string_with_type() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var b = new Blob(["hello"], { type: "Text/Plain" });
                        return [b.size, b.type];
                    })()
                    "#,
                )
                .expect("eval failed");

            let size: i32 = result[0].as_int().unwrap();
            assert_eq!(size, 5);
            let mime: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(mime, "text/plain");
        });
    }

    #[test]
    fn js_blob_text_returns_content() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var b = new Blob(["hello world"]);
                globalThis.__result__ = await b.text();
                "#,
            );
            let result: String = val.as_string().unwrap().to_string().unwrap();
            assert_eq!(result, "hello world");
        });
    }

    #[test]
    fn js_blob_slice_text() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var b = new Blob(["hello"]);
                var s = b.slice(1, 3);
                globalThis.__result__ = await s.text();
                "#,
            );
            let result: String = val.as_string().unwrap().to_string().unwrap();
            assert_eq!(result, "el");
        });
    }

    #[test]
    fn js_blob_from_arraybuffer() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var buf = new ArrayBuffer(3);
                var view = new Uint8Array(buf);
                view[0] = 65;
                view[1] = 66;
                view[2] = 67;
                var b = new Blob([buf]);
                var text = await b.text();
                globalThis.__result__ = [b.size, text];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let size: i32 = result[0].as_int().unwrap();
            assert_eq!(size, 3);
            let text: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "ABC");
        });
    }

    #[test]
    fn js_blob_empty() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var b = new Blob();
                        return [b.size, b.type];
                    })()
                    "#,
                )
                .expect("eval failed");

            let size: i32 = result[0].as_int().unwrap();
            assert_eq!(size, 0);
            let mime: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(mime, "");
        });
    }

    #[test]
    fn js_blob_concatenate_parts() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var b = new Blob(["hello", " ", "world"]);
                globalThis.__result__ = await b.text();
                "#,
            );
            let result: String = val.as_string().unwrap().to_string().unwrap();
            assert_eq!(result, "hello world");
        });
    }

    #[test]
    fn js_blob_concatenate_blob_parts() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var b1 = new Blob(["hello"]);
                var b2 = new Blob([b1, " world"]);
                globalThis.__result__ = await b2.text();
                "#,
            );
            let result: String = val.as_string().unwrap().to_string().unwrap();
            assert_eq!(result, "hello world");
        });
    }

    #[test]
    fn js_blob_arraybuffer_returns_bytes() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var b = new Blob(["ABC"]);
                var buf = await b.arrayBuffer();
                var view = new Uint8Array(buf);
                globalThis.__result__ = [view[0], view[1], view[2]];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            assert_eq!(result.len(), 3);
            assert_eq!(result[0].as_int().unwrap(), 65);
            assert_eq!(result[1].as_int().unwrap(), 66);
            assert_eq!(result[2].as_int().unwrap(), 67);
        });
    }

    #[test]
    fn js_blob_bytes_returns_uint8array() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                var b = new Blob(["AB"]);
                var u8 = await b.bytes();
                globalThis.__result__ = [u8[0], u8[1], u8.length];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            assert_eq!(result[0].as_int().unwrap(), 65);
            assert_eq!(result[1].as_int().unwrap(), 66);
            assert_eq!(result[2].as_int().unwrap(), 2);
        });
    }

    #[test]
    fn js_blob_slice_with_content_type() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var b = new Blob(["hello"], { type: "text/plain" });
                        var s = b.slice(0, 5, "Application/JSON");
                        return [s.type, s.size];
                    })()
                    "#,
                )
                .expect("eval failed");

            let mime: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(mime, "application/json");
            let size: i32 = result[1].as_int().unwrap();
            assert_eq!(size, 5);
        });
    }

    #[test]
    fn js_blob_typedarray_view_respects_byte_offset() {
        with_js_context(|ctx| {
            let val = eval_async(
                ctx,
                r#"
                // Create a 6-byte buffer, then a view into bytes [2..5) (3 bytes).
                var buf = new ArrayBuffer(6);
                var full = new Uint8Array(buf);
                full[0] = 0xAA; full[1] = 0xBB;
                full[2] = 65;   full[3] = 66;   full[4] = 67;
                full[5] = 0xCC;
                // Subarray view with byteOffset=2, byteLength=3
                var view = new Uint8Array(buf, 2, 3);
                var b = new Blob([view]);
                var text = await b.text();
                globalThis.__result__ = [b.size, text];
                "#,
            );
            let result: Vec<Value<'_>> = Vec::from_js(ctx, val).unwrap();
            let size: i32 = result[0].as_int().unwrap();
            assert_eq!(
                size, 3,
                "Blob should only contain the view's 3 bytes, not the full buffer"
            );
            let text: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(text, "ABC");
        });
    }

    #[test]
    fn js_blob_requires_new() {
        with_js_context(|ctx| {
            let result: Result<Value<'_>, _> = ctx.eval(
                r#"
                (function() {
                    try {
                        Blob(["test"]);
                        return false;
                    } catch(e) {
                        return e instanceof TypeError;
                    }
                })()
                "#,
            );
            let val = result.expect("eval failed");
            assert!(val.as_bool().unwrap());
        });
    }
}
