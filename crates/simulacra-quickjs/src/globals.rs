//! Tier 1 web-standard globals for the QuickJS runtime.
//!
//! Pure computation polyfills and lightweight Rust-backed host functions.
//! No I/O, no capability requirements.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use crate::formatting::format_js_value;
use base64::Engine as _;
use rquickjs::{Function, Object, Value};

/// Register all Tier 1 web-standard globals on the given JS context.
///
/// `stdout_buf` is the shared buffer for console output.
/// `runtime_start` is the `Instant` when the runtime was created (for `performance.now()`).
pub fn register_web_globals(
    ctx: &rquickjs::Ctx<'_>,
    stdout_buf: &Rc<RefCell<String>>,
    runtime_start: Instant,
) -> Result<(), crate::JsError> {
    register_base64(ctx)?;
    register_text_codec(ctx)?;
    register_structured_clone(ctx)?;
    register_queue_microtask(ctx)?;
    register_performance(ctx, runtime_start)?;
    register_timers(ctx)?;
    register_console_levels(ctx, stdout_buf)?;
    register_url_polyfill(ctx)?;
    Ok(())
}

fn register_base64(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    let globals = ctx.globals();

    // btoa: string -> base64. Throws on code points > 255 (Latin1 restriction).
    let btoa_fn = Function::new(ctx.clone(), |input: String| -> rquickjs::Result<String> {
        for ch in input.chars() {
            if ch as u32 > 255 {
                return Err(rquickjs::Error::new_from_js_message(
                    "string",
                    "string",
                    "InvalidCharacterError: btoa failed: string contains characters outside Latin1 range",
                ));
            }
        }
        let bytes: Vec<u8> = input.chars().map(|c| c as u8).collect();
        Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
    })
    .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals
        .set("btoa", btoa_fn)
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    // atob: base64 -> string. Throws on invalid base64.
    let atob_fn = Function::new(ctx.clone(), |input: String| -> rquickjs::Result<String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&input)
            .map_err(|e| {
                rquickjs::Error::new_from_js_message(
                    "string",
                    "string",
                    &format!("InvalidCharacterError: atob failed: {e}"),
                )
            })?;
        Ok(bytes.into_iter().map(|b| b as char).collect())
    })
    .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals
        .set("atob", atob_fn)
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    Ok(())
}

fn register_text_codec(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    let globals = ctx.globals();

    let encode_fn = Function::new(ctx.clone(), |input: String| -> Vec<u8> {
        input.into_bytes() // Rust strings are UTF-8
    })
    .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals
        .set("__simulacra_text_encode", encode_fn)
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    let decode_fn = Function::new(ctx.clone(), |bytes: Vec<u8>| -> rquickjs::Result<String> {
        String::from_utf8(bytes)
            .map_err(|e| rquickjs::Error::new_from_js_message("bytes", "string", &e.to_string()))
    })
    .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals
        .set("__simulacra_text_decode", decode_fn)
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    let polyfill = r#"
        globalThis.TextEncoder = class TextEncoder {
            get encoding() { return 'utf-8'; }
            encode(str) {
                const bytes = __simulacra_text_encode(String(str));
                return new Uint8Array(bytes);
            }
        };
        globalThis.TextDecoder = class TextDecoder {
            #encoding;
            constructor(encoding) {
                this.#encoding = (encoding || 'utf-8').toLowerCase();
                if (this.#encoding !== 'utf-8' && this.#encoding !== 'utf8') {
                    throw new RangeError(`TextDecoder: unsupported encoding '${this.#encoding}'`);
                }
            }
            get encoding() { return 'utf-8'; }
            decode(input) {
                if (!input) return '';
                return __simulacra_text_decode(Array.from(new Uint8Array(input.buffer || input)));
            }
        };
    "#;
    ctx.eval::<(), _>(polyfill)
        .map_err(|e| crate::JsError::Runtime(format!("TextEncoder/TextDecoder polyfill: {e}")))?;

    Ok(())
}

fn register_url_polyfill(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    let polyfill = include_str!("url_polyfill.js");
    ctx.eval::<(), _>(polyfill)
        .map_err(|e| crate::JsError::Runtime(format!("URL polyfill: {e}")))?;
    Ok(())
}

fn register_structured_clone(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    ctx.eval::<(), _>("globalThis.structuredClone = (obj) => JSON.parse(JSON.stringify(obj));")
        .map_err(|e| crate::JsError::Runtime(format!("structuredClone polyfill: {e}")))?;
    Ok(())
}

fn register_queue_microtask(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    ctx.eval::<(), _>("globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);")
        .map_err(|e| crate::JsError::Runtime(format!("queueMicrotask polyfill: {e}")))?;
    Ok(())
}

fn register_performance(
    ctx: &rquickjs::Ctx<'_>,
    runtime_start: Instant,
) -> Result<(), crate::JsError> {
    let perf = Object::new(ctx.clone()).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    let now_fn = Function::new(ctx.clone(), move || -> f64 {
        runtime_start.elapsed().as_secs_f64() * 1000.0
    })
    .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    perf.set("now", now_fn)
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    ctx.globals()
        .set("performance", perf)
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    Ok(())
}

fn register_timers(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    let polyfill = r#"
        (() => {
            let __nextTimerId = 1;
            const __cancelledTimers = new Set();
            globalThis.setTimeout = (fn, delay) => {
                const id = __nextTimerId++;
                Promise.resolve().then(() => {
                    if (!__cancelledTimers.has(id)) fn();
                    __cancelledTimers.delete(id);
                });
                return id;
            };
            globalThis.clearTimeout = (id) => {
                __cancelledTimers.add(id);
            };
        })();
    "#;
    ctx.eval::<(), _>(polyfill)
        .map_err(|e| crate::JsError::Runtime(format!("setTimeout polyfill: {e}")))?;
    Ok(())
}

fn register_console_levels(
    ctx: &rquickjs::Ctx<'_>,
    stdout_buf: &Rc<RefCell<String>>,
) -> Result<(), crate::JsError> {
    let globals = ctx.globals();
    let console: Object<'_> = globals
        .get("console")
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    for (name, prefix) in [
        ("error", "[ERROR]"),
        ("warn", "[WARN]"),
        ("info", "[INFO]"),
        ("debug", "[DEBUG]"),
    ] {
        let buf = Rc::clone(stdout_buf);
        let prefix = prefix.to_string();
        let level_fn = Function::new(
            ctx.clone(),
            move |args: rquickjs::function::Rest<Value<'_>>| {
                let parts: Vec<String> = args
                    .0
                    .iter()
                    .map(|v| format_js_value(v, 0, &mut HashSet::new()))
                    .collect();
                let line = parts.join(" ");
                buf.borrow_mut().push_str(&format!("{prefix} {line}\n"));
            },
        )
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
        console
            .set(name, level_fn)
            .map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    }

    Ok(())
}
