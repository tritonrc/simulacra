# S027 JS Agent Capabilities — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fill capability gaps in the QuickJS runtime — web-standard globals, path manipulation, crypto, and fs completions — so agents can perform common operations without tool-call round-trips.

**Architecture:** All changes are in `crates/simulacra-quickjs/`. Tier 1 items are JS polyfills or lightweight Rust host functions registered during runtime init. Tiers 2-3 are native `ModuleDef` implementations following the existing `FsModule`/`ConsoleModule`/`ProcessModule` pattern. Tier 4 extends the existing `fs` global object with new host functions that delegate through the VFS (and `FsProxy` when present).

**Tech Stack:** Rust, rquickjs, `base64` crate (encoding), `uuid` crate (v4), `sha2`/`md5` crates (hashing), `rand` crate (randomness), `std::path::Path` (POSIX path ops)

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/simulacra-quickjs/Cargo.toml` | Modify | Add `base64`, `uuid`, `sha2`, `md5`, `rand` deps |
| `crates/simulacra-quickjs/src/lib.rs` | Modify | Register new globals, modules, extend `FsProxy` trait, update `SIMULACRA_MODULES` |
| `crates/simulacra-quickjs/src/globals.rs` | Create | Tier 1 web-standard globals (atob/btoa, TextEncoder/TextDecoder, URL, etc.) |
| `crates/simulacra-quickjs/src/path_module.rs` | Create | `PathModule` — `ModuleDef` for `simulacra:path` |
| `crates/simulacra-quickjs/src/crypto_module.rs` | Create | `CryptoModule` — `ModuleDef` for `simulacra:crypto` |
| `crates/simulacra-quickjs/src/tests.rs` | Modify | Add tests for all four tiers |

---

### Task 1: Tier 1 Web-standard Globals

**Files:**
- Create: `crates/simulacra-quickjs/src/globals.rs`
- Modify: `crates/simulacra-quickjs/Cargo.toml` (add `base64` dep)
- Modify: `crates/simulacra-quickjs/src/lib.rs` (call globals registration, extend console)

This task adds: `atob`/`btoa`, `TextEncoder`/`TextDecoder`, `URL`/`URLSearchParams`, `structuredClone`, `queueMicrotask`, `performance.now()`, `setTimeout`/`clearTimeout`, `console.error`/`warn`/`info`/`debug`.

- [ ] **Step 1: Add `base64` dependency**

In `crates/simulacra-quickjs/Cargo.toml`, add:

```toml
[dependencies]
base64 = "0.22"
```

- [ ] **Step 2: Create `globals.rs` with Rust-backed host functions**

Create `crates/simulacra-quickjs/src/globals.rs`. This module exports a single `register_globals` function called from `JsRuntime::register_globals`.

```rust
//! Tier 1 web-standard globals for the QuickJS runtime.
//!
//! Pure computation polyfills and lightweight Rust-backed host functions.
//! No I/O, no capability requirements.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

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

// ... individual register_* functions follow ...
```

Key implementation details for each sub-function:

**`atob`/`btoa`** — Rust host functions using `base64::engine::general_purpose::STANDARD`:

```rust
fn register_base64(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    let globals = ctx.globals();

    // btoa: string -> base64. Throws on code points > 255 (Latin1 restriction).
    let btoa_fn = Function::new(ctx.clone(), |input: String| -> rquickjs::Result<String> {
        for ch in input.chars() {
            if ch as u32 > 255 {
                return Err(rquickjs::Error::new_from_js_message(
                    "string", "string",
                    "InvalidCharacterError: btoa failed: string contains characters outside Latin1 range",
                ));
            }
        }
        let bytes: Vec<u8> = input.chars().map(|c| c as u8).collect();
        Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
    }).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals.set("btoa", btoa_fn).map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    // atob: base64 -> string. Throws on invalid base64.
    let atob_fn = Function::new(ctx.clone(), |input: String| -> rquickjs::Result<String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&input)
            .map_err(|e| rquickjs::Error::new_from_js_message(
                "string", "string",
                &format!("InvalidCharacterError: atob failed: {e}"),
            ))?;
        Ok(bytes.into_iter().map(|b| b as char).collect())
    }).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals.set("atob", atob_fn).map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    Ok(())
}
```

**`TextEncoder`/`TextDecoder`** — JS polyfill registered via `ctx.eval()`. The encode/decode operations are straightforward in JS since QuickJS strings are already UTF-16 internally, but for correctness the `encode` function should produce a proper `Uint8Array`. Use a Rust host function for `encode` to guarantee correct UTF-8 byte output:

```rust
fn register_text_codec(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    // Register Rust-backed __simulacra_text_encode and __simulacra_text_decode helpers,
    // then wrap them in JS classes.
    let globals = ctx.globals();

    let encode_fn = Function::new(ctx.clone(), |input: String| -> Vec<u8> {
        input.into_bytes() // Rust strings are UTF-8
    }).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals.set("__simulacra_text_encode", encode_fn)
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    let decode_fn = Function::new(ctx.clone(), |bytes: Vec<u8>| -> rquickjs::Result<String> {
        String::from_utf8(bytes).map_err(|e| {
            rquickjs::Error::new_from_js_message("bytes", "string", &e.to_string())
        })
    }).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    globals.set("__simulacra_text_decode", decode_fn)
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
```

**`URL`/`URLSearchParams`** — Pure JS polyfill. Not a full WHATWG parser; handles http/https URLs, query strings, hash fragments. Registered via `ctx.eval()`:

```rust
fn register_url_polyfill(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    // ~150-line JS polyfill for URL and URLSearchParams.
    // Parses: protocol, host, hostname, port, pathname, search, hash, origin,
    //         username, password, searchParams, href.
    // URLSearchParams: get, set, append, delete, has, toString, entries, keys,
    //                  values, forEach, [Symbol.iterator].
    let polyfill = include_str!("url_polyfill.js");
    ctx.eval::<(), _>(polyfill)
        .map_err(|e| crate::JsError::Runtime(format!("URL polyfill: {e}")))?;
    Ok(())
}
```

Create `crates/simulacra-quickjs/src/url_polyfill.js` with the URL/URLSearchParams implementation. This keeps the JS polyfill out of the Rust source for readability. Use `include_str!` to embed it at compile time.

**`structuredClone`** — One-liner polyfill:

```rust
fn register_structured_clone(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    ctx.eval::<(), _>(
        "globalThis.structuredClone = (obj) => JSON.parse(JSON.stringify(obj));"
    ).map_err(|e| crate::JsError::Runtime(format!("structuredClone polyfill: {e}")))?;
    Ok(())
}
```

**`queueMicrotask`** — One-liner polyfill:

```rust
fn register_queue_microtask(ctx: &rquickjs::Ctx<'_>) -> Result<(), crate::JsError> {
    ctx.eval::<(), _>(
        "globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);"
    ).map_err(|e| crate::JsError::Runtime(format!("queueMicrotask polyfill: {e}")))?;
    Ok(())
}
```

**`performance.now()`** — Rust host function backed by `Instant`:

```rust
fn register_performance(
    ctx: &rquickjs::Ctx<'_>,
    runtime_start: Instant,
) -> Result<(), crate::JsError> {
    let perf = Object::new(ctx.clone()).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    let now_fn = Function::new(ctx.clone(), move || -> f64 {
        runtime_start.elapsed().as_secs_f64() * 1000.0
    }).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    perf.set("now", now_fn).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    ctx.globals().set("performance", perf).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    Ok(())
}
```

**`setTimeout`/`clearTimeout`** — JS polyfill using microtasks. Non-zero delays clamped to 0. ID-based cancellation:

```rust
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
```

**`console.error`/`warn`/`info`/`debug`** — Extend the console object that `register_globals` already creates. These use the same `format_js_value` formatter but write to the stdout buffer with level prefixes. The `JsOutput` struct must be extended to separate stdout/stderr:

```rust
fn register_console_levels(
    ctx: &rquickjs::Ctx<'_>,
    stdout_buf: &Rc<RefCell<String>>,
) -> Result<(), crate::JsError> {
    let globals = ctx.globals();
    let console: Object<'_> = globals.get("console")
        .map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    // console.error — writes to stdout_buf with [ERROR] prefix
    let buf = Rc::clone(stdout_buf);
    let error_fn = Function::new(ctx.clone(),
        move |args: rquickjs::function::Rest<Value<'_>>| {
            let parts: Vec<String> = args.0.iter()
                .map(|v| crate::format_js_value(v, 0, &mut std::collections::HashSet::new()))
                .collect();
            let line = parts.join(" ");
            buf.borrow_mut().push_str(&format!("[ERROR] {line}\n"));
        },
    ).map_err(|e| crate::JsError::Runtime(e.to_string()))?;
    console.set("error", error_fn).map_err(|e| crate::JsError::Runtime(e.to_string()))?;

    // Repeat for warn ([WARN]), info ([INFO]), debug ([DEBUG])
    // ... same pattern with different prefix ...

    Ok(())
}
```

**Design decision — console level metadata:** Rather than introducing a separate stderr buffer and a structured log level field in `JsOutput` (which would change the public API), the pragmatic approach is to prefix level-tagged lines with `[ERROR]`, `[WARN]`, `[INFO]`, `[DEBUG]` in the stdout buffer. The spec says `console.error` writes to "virtual stderr" — we achieve this by tagging the lines. If a separate stderr buffer is needed later, it can be added without breaking existing behavior.

However, if the implementer prefers to match the spec more precisely, an alternative is to add a `stderr: String` field to `JsOutput` and route `console.error`/`console.warn` to it. This is a judgment call for the implementing agent.

- [ ] **Step 3: Create `url_polyfill.js`**

Create `crates/simulacra-quickjs/src/url_polyfill.js` with a ~150-line polyfill implementing `URL` and `URLSearchParams` classes. The polyfill must handle:
- Parsing `protocol`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`, `origin`, `username`, `password`, `href`
- Relative URL resolution via `new URL(relative, base)`
- `searchParams` property returning a `URLSearchParams` instance
- `URLSearchParams` with `get`, `set`, `append`, `delete`, `has`, `toString`, `entries`, `keys`, `values`, `forEach`, `[Symbol.iterator]`

Use a regex-based approach for URL parsing (not a full WHATWG parser). Test coverage will validate correctness for http/https URLs.

- [ ] **Step 4: Wire into `lib.rs`**

In `crates/simulacra-quickjs/src/lib.rs`:

1. Add `mod globals;` at the top.
2. Add `runtime_start: Instant` field to the `JsRuntime` struct, set it to `Instant::now()` in `build()`.
3. In `register_globals`, after registering `console.log`, call:

```rust
globals::register_web_globals(&ctx, &stdout_buf, self.runtime_start)?;
```

4. Update the `ConsoleModule` `ModuleDef` to export `error`, `warn`, `info`, `debug` in addition to `log`.

- [ ] **Step 5: Add tests for Tier 1**

Add to `crates/simulacra-quickjs/src/tests.rs`:

```rust
// --- Tier 1: atob/btoa ---
#[test]
fn btoa_encodes_hello() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"btoa("hello")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("aGVsbG8="));
}

#[test]
fn atob_decodes_hello() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"atob("aGVsbG8=")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("hello"));
}

#[test]
fn btoa_throws_on_non_latin1() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"btoa("\u{1F600}")"#);
    assert!(result.is_err());
}

#[test]
fn atob_throws_on_invalid_base64() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"atob("not valid!!!")"#);
    assert!(result.is_err());
}

#[test]
fn btoa_atob_roundtrip() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"atob(btoa("hello world 123"))"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("hello world 123"));
}

// --- TextEncoder / TextDecoder ---
#[test]
fn text_encoder_encodes_hello() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const enc = new TextEncoder();
        const bytes = enc.encode("hello");
        JSON.stringify(Array.from(bytes))
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("[104,101,108,108,111]"));
}

#[test]
fn text_encoder_decoder_roundtrip() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const enc = new TextEncoder();
        const dec = new TextDecoder();
        dec.decode(enc.encode("hello world"))
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("hello world"));
}

// --- URL / URLSearchParams ---
#[test]
fn url_parses_components() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const u = new URL("https://example.com:8080/path?q=1#frag");
        JSON.stringify({
            protocol: u.protocol,
            hostname: u.hostname,
            port: u.port,
            pathname: u.pathname,
            search: u.search,
            hash: u.hash,
        })
    "#).unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["protocol"], "https:");
    assert_eq!(val["hostname"], "example.com");
    assert_eq!(val["port"], "8080");
    assert_eq!(val["pathname"], "/path");
    assert_eq!(val["search"], "?q=1");
    assert_eq!(val["hash"], "#frag");
}

#[test]
fn url_resolves_relative() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"new URL("/path", "https://base.com").href"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("https://base.com/path"));
}

#[test]
fn url_search_params_get_set() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const p = new URLSearchParams("a=1&b=2");
        p.set("c", "3");
        p.get("a") + "," + p.get("c") + "," + p.has("b")
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("1,3,true"));
}

// --- structuredClone ---
#[test]
fn structured_clone_deep_copies() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const orig = { a: { b: 1 } };
        const clone = structuredClone(orig);
        clone.a.b = 99;
        orig.a.b
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("1"));
}

// --- queueMicrotask ---
#[test]
fn queue_microtask_executes() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        let ran = false;
        queueMicrotask(() => { ran = true; });
        ran
    "#).unwrap();
    // Note: microtask may or may not have run synchronously depending on
    // QuickJS microtask queue draining. Test via async:
    let out = rt.eval(r#"
        (async () => {
            let ran = false;
            queueMicrotask(() => { ran = true; });
            await Promise.resolve();
            return ran;
        })()
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- performance.now ---
#[test]
fn performance_now_returns_number() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval("typeof performance.now()").unwrap();
    assert_eq!(out.result.as_deref(), Some("number"));
}

#[test]
fn performance_now_non_decreasing() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const a = performance.now();
        const b = performance.now();
        b >= a
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- setTimeout / clearTimeout ---
#[test]
fn set_timeout_executes() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        (async () => {
            let ran = false;
            setTimeout(() => { ran = true; }, 0);
            await Promise.resolve();
            return ran;
        })()
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn set_timeout_returns_numeric_id() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval("typeof setTimeout(() => {}, 0)").unwrap();
    assert_eq!(out.result.as_deref(), Some("number"));
}

#[test]
fn clear_timeout_cancels() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        (async () => {
            let ran = false;
            const id = setTimeout(() => { ran = true; }, 0);
            clearTimeout(id);
            await Promise.resolve();
            return ran;
        })()
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("false"));
}

#[test]
fn set_timeout_nonzero_clamps_to_zero() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        (async () => {
            let ran = false;
            setTimeout(() => { ran = true; }, 100);
            await Promise.resolve();
            return ran;
        })()
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- console levels ---
#[test]
fn console_error_writes_error_level() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"console.error("boom")"#).unwrap();
    assert!(out.stdout.contains("ERROR"));
    assert!(out.stdout.contains("boom"));
}

#[test]
fn console_warn_writes_warn_level() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"console.warn("careful")"#).unwrap();
    assert!(out.stdout.contains("WARN"));
    assert!(out.stdout.contains("careful"));
}

#[test]
fn console_info_writes_info_level() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"console.info("fyi")"#).unwrap();
    assert!(out.stdout.contains("INFO"));
    assert!(out.stdout.contains("fyi"));
}

#[test]
fn console_debug_writes_debug_level() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"console.debug("trace")"#).unwrap();
    assert!(out.stdout.contains("DEBUG"));
    assert!(out.stdout.contains("trace"));
}
```

- [ ] **Step 6: Run tests to verify they fail**

```bash
cargo test -p simulacra-quickjs -- tier1
```

Expected: FAIL — globals not registered yet.

- [ ] **Step 7: Implement until tests pass**

Fill in the full `globals.rs` implementation and wire it into `lib.rs`.

```bash
cargo test -p simulacra-quickjs
```

Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/simulacra-quickjs/
git commit -m "feat(quickjs): Tier 1 web-standard globals (atob/btoa, TextEncoder, URL, performance, timers, console levels) [S027]"
```

---

### Task 2: `simulacra:path` Module

**Files:**
- Create: `crates/simulacra-quickjs/src/path_module.rs`
- Modify: `crates/simulacra-quickjs/src/lib.rs` (register module, update `SIMULACRA_MODULES`)

Implements all `simulacra:path` functions as a native `ModuleDef` backed by `std::path::Path`.

- [ ] **Step 1: Create `path_module.rs`**

Create `crates/simulacra-quickjs/src/path_module.rs`:

```rust
//! Native module definition for `simulacra:path`.
//!
//! POSIX-only path manipulation. All functions map to `std::path::Path`
//! operations with POSIX normalization.

use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::{Ctx, Function, Object, Value};
use std::path::{Component, Path, PathBuf};

pub struct PathModule;

impl ModuleDef for PathModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("join")?;
        decl.declare("resolve")?;
        decl.declare("dirname")?;
        decl.declare("basename")?;
        decl.declare("extname")?;
        decl.declare("normalize")?;
        decl.declare("isAbsolute")?;
        decl.declare("relative")?;
        decl.declare("parse")?;
        decl.declare("format")?;
        decl.declare("sep")?;
        decl.declare("delimiter")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        // join(...segments) -> string
        let join_fn = Function::new(ctx.clone(),
            |args: rquickjs::function::Rest<String>| -> String {
                if args.0.is_empty() {
                    return ".".to_string();
                }
                let mut result = PathBuf::new();
                for seg in &args.0 {
                    result.push(seg);
                }
                normalize_posix(&result.to_string_lossy())
            }
        )?;
        exports.export("join", join_fn.clone())?;

        // resolve(...segments) -> string (absolute against /workspace)
        let resolve_fn = Function::new(ctx.clone(),
            |args: rquickjs::function::Rest<String>| -> String {
                let mut result = PathBuf::from("/workspace"); // VFS cwd
                for seg in &args.0 {
                    if seg.starts_with('/') {
                        result = PathBuf::from(seg);
                    } else {
                        result.push(seg);
                    }
                }
                normalize_posix(&result.to_string_lossy())
            }
        )?;
        exports.export("resolve", resolve_fn.clone())?;

        // dirname(p) -> string
        let dirname_fn = Function::new(ctx.clone(), |p: String| -> String {
            Path::new(&p)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string())
        })?;
        exports.export("dirname", dirname_fn.clone())?;

        // basename(p, ext?) -> string
        let basename_fn = Function::new(ctx.clone(),
            |p: String, ext: rquickjs::function::Opt<String>| -> String {
                let base = Path::new(&p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Some(ext) = ext.0 {
                    if let Some(stripped) = base.strip_suffix(&ext) {
                        return stripped.to_string();
                    }
                }
                base
            }
        )?;
        exports.export("basename", basename_fn.clone())?;

        // extname(p) -> string
        let extname_fn = Function::new(ctx.clone(), |p: String| -> String {
            Path::new(&p)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default()
        })?;
        exports.export("extname", extname_fn.clone())?;

        // normalize(p) -> string
        let normalize_fn = Function::new(ctx.clone(), |p: String| -> String {
            normalize_posix(&p)
        })?;
        exports.export("normalize", normalize_fn.clone())?;

        // isAbsolute(p) -> bool
        let is_absolute_fn = Function::new(ctx.clone(), |p: String| -> bool {
            p.starts_with('/')
        })?;
        exports.export("isAbsolute", is_absolute_fn.clone())?;

        // relative(from, to) -> string
        let relative_fn = Function::new(ctx.clone(), |from: String, to: String| -> String {
            compute_relative(&from, &to)
        })?;
        exports.export("relative", relative_fn.clone())?;

        // parse(p) -> { root, dir, base, ext, name }
        let parse_fn = Function::new(ctx.clone(),
            move |ctx: Ctx<'_>, p: String| -> rquickjs::Result<Object<'_>> {
                let path = Path::new(&p);
                let obj = Object::new(ctx)?;
                let root = if p.starts_with('/') { "/" } else { "" };
                let dir = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let base = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                let name = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                obj.set("root", root)?;
                obj.set("dir", dir)?;
                obj.set("base", base)?;
                obj.set("ext", ext)?;
                obj.set("name", name)?;
                Ok(obj)
            }
        )?;
        exports.export("parse", parse_fn.clone())?;

        // format(obj) -> string
        // If obj.dir is set, uses dir + sep + base.
        // Otherwise uses root + base.
        let format_fn = Function::new(ctx.clone(),
            |obj: Object<'_>| -> rquickjs::Result<String> {
                let dir: String = obj.get::<_, String>("dir").unwrap_or_default();
                let base: String = obj.get::<_, String>("base").unwrap_or_default();
                let root: String = obj.get::<_, String>("root").unwrap_or_default();
                let name: String = obj.get::<_, String>("name").unwrap_or_default();
                let ext: String = obj.get::<_, String>("ext").unwrap_or_default();

                let effective_base = if !base.is_empty() { base } else { format!("{name}{ext}") };
                if !dir.is_empty() {
                    Ok(format!("{dir}/{effective_base}"))
                } else {
                    Ok(format!("{root}{effective_base}"))
                }
            }
        )?;
        exports.export("format", format_fn.clone())?;

        // Constants
        exports.export("sep", "/")?;
        exports.export("delimiter", ":")?;

        // Default export: object with all functions + constants
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("join", join_fn)?;
        default_obj.set("resolve", resolve_fn)?;
        default_obj.set("dirname", dirname_fn)?;
        default_obj.set("basename", basename_fn)?;
        default_obj.set("extname", extname_fn)?;
        default_obj.set("normalize", normalize_fn)?;
        default_obj.set("isAbsolute", is_absolute_fn)?;
        default_obj.set("relative", relative_fn)?;
        default_obj.set("parse", parse_fn)?;
        default_obj.set("format", format_fn)?;
        default_obj.set("sep", "/")?;
        default_obj.set("delimiter", ":")?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

/// Normalize a POSIX path: collapse `.`, `..`, duplicate slashes.
fn normalize_posix(p: &str) -> String {
    let is_absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in p.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if is_absolute || parts.last().map_or(false, |s| *s != "..") {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if is_absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Compute relative path from `from` to `to`.
fn compute_relative(from: &str, to: &str) -> String {
    let from_norm = normalize_posix(from);
    let to_norm = normalize_posix(to);
    let from_parts: Vec<&str> = from_norm.split('/').filter(|s| !s.is_empty()).collect();
    let to_parts: Vec<&str> = to_norm.split('/').filter(|s| !s.is_empty()).collect();

    let common = from_parts.iter().zip(&to_parts).take_while(|(a, b)| a == b).count();
    let ups = from_parts.len() - common;
    let downs = &to_parts[common..];

    let mut result: Vec<&str> = Vec::new();
    for _ in 0..ups {
        result.push("..");
    }
    result.extend_from_slice(downs);

    if result.is_empty() {
        ".".to_string()
    } else {
        result.join("/")
    }
}
```

- [ ] **Step 2: Wire into `lib.rs`**

1. Add `mod path_module;` to `lib.rs`.
2. Add `"path"` to the `SIMULACRA_MODULES` array.
3. In `register_native_modules`, add:

```rust
let (_module, promise) =
    Module::evaluate_def::<path_module::PathModule, _>(ctx.clone(), "simulacra:path")
        .map_err(|e| JsError::Runtime(format!("failed to register simulacra:path: {e}")))?;
let _: () = promise
    .finish()
    .map_err(|e| JsError::Runtime(format!("failed to evaluate simulacra:path: {e}")))?;
tracing::debug!("simulacra:path module loaded");
```

4. In the `SimulacraLoader::load` match, add `"simulacra:path"` to the built-in short-circuit.

- [ ] **Step 3: Add tests for simulacra:path**

Add to `crates/simulacra-quickjs/src/tests.rs`:

```rust
// --- Tier 2: simulacra:path ---
#[test]
fn path_join_basic() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.join("a", "b", "c")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("a/b/c"));
}

#[test]
fn path_join_resolves_dotdot() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.join("/a", "b", "..", "c")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/c"));
}

#[test]
fn path_resolve_absolute() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.resolve("a", "b")"#).unwrap();
    let result = out.result.unwrap();
    assert!(result.starts_with('/'), "resolve should produce absolute path, got: {result}");
}

#[test]
fn path_dirname() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.dirname("/a/b/c")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/b"));
}

#[test]
fn path_basename() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.basename("/a/b/c.txt")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("c.txt"));
}

#[test]
fn path_basename_strips_ext() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.basename("/a/b/c.txt", ".txt")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("c"));
}

#[test]
fn path_extname() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.extname("file.tar.gz")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some(".gz"));
}

#[test]
fn path_normalize() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.normalize("/a//b/../c")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/c"));
}

#[test]
fn path_is_absolute() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import path from 'simulacra:path';
        JSON.stringify([path.isAbsolute("/a"), path.isAbsolute("a")])
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("[true,false]"));
}

#[test]
fn path_relative() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import path from 'simulacra:path'; path.relative("/a/b", "/a/c")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("../c"));
}

#[test]
fn path_parse_and_format() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import path from 'simulacra:path';
        const parsed = path.parse("/a/b/c.txt");
        JSON.stringify(parsed)
    "#).unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["root"], "/");
    assert_eq!(val["dir"], "/a/b");
    assert_eq!(val["base"], "c.txt");
    assert_eq!(val["ext"], ".txt");
    assert_eq!(val["name"], "c");
}

#[test]
fn path_format_inverse_of_parse() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import path from 'simulacra:path';
        path.format({ dir: "/a/b", base: "c.txt" })
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/b/c.txt"));
}

#[test]
fn path_sep_and_delimiter() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import path from 'simulacra:path';
        path.sep + "," + path.delimiter
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("/,:"));
}

#[test]
fn path_named_import() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"import { join } from 'simulacra:path'; join("a", "b")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("a/b"));
}
```

- [ ] **Step 4: Run tests, implement, iterate**

```bash
cargo test -p simulacra-quickjs
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-quickjs/
git commit -m "feat(quickjs): simulacra:path native module with POSIX path operations [S027]"
```

---

### Task 3: `simulacra:crypto` Module

**Files:**
- Create: `crates/simulacra-quickjs/src/crypto_module.rs`
- Modify: `crates/simulacra-quickjs/Cargo.toml` (add `uuid`, `sha2`, `md5`, `rand` deps)
- Modify: `crates/simulacra-quickjs/src/lib.rs` (register module, update `SIMULACRA_MODULES`)

- [ ] **Step 1: Add dependencies**

In `crates/simulacra-quickjs/Cargo.toml`, add:

```toml
[dependencies]
uuid = { version = "1", features = ["v4"] }
sha2 = "0.10"
md-5 = "0.10"
rand = "0.8"
```

Note: The `md5` crate is deprecated; use `md-5` from RustCrypto (same family as `sha2`). Check existing workspace deps first — some of these may already be in `Cargo.toml` workspace dependencies.

- [ ] **Step 2: Create `crypto_module.rs`**

Create `crates/simulacra-quickjs/src/crypto_module.rs`:

```rust
//! Native module definition for `simulacra:crypto`.
//!
//! Provides randomness and hashing backed by Rust crates.

use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::{Ctx, Function, Object};

pub struct CryptoModule;

impl ModuleDef for CryptoModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("randomUUID")?;
        decl.declare("randomBytes")?;
        decl.declare("createHash")?;
        decl.declare("getRandomValues")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        // randomUUID() -> string
        let random_uuid_fn = Function::new(ctx.clone(), || -> String {
            uuid::Uuid::new_v4().to_string()
        })?;
        exports.export("randomUUID", random_uuid_fn.clone())?;

        // randomBytes(n) -> Uint8Array (as Vec<u8>, rquickjs converts)
        let random_bytes_fn = Function::new(ctx.clone(), |n: usize| -> Vec<u8> {
            use rand::RngCore;
            let mut buf = vec![0u8; n];
            rand::thread_rng().fill_bytes(&mut buf);
            buf
        })?;
        exports.export("randomBytes", random_bytes_fn.clone())?;

        // getRandomValues(typedArray) -> fills and returns
        // This is tricky because we need to modify the typed array in place.
        // Implemented as a JS wrapper over a Rust fill function.
        let fill_random_fn = Function::new(ctx.clone(), |n: usize| -> rquickjs::Result<Vec<u8>> {
            if n > 65536 {
                return Err(rquickjs::Error::new_from_js_message(
                    "number", "number",
                    "QuotaExceededError: getRandomValues: array exceeds 65536 bytes",
                ));
            }
            use rand::RngCore;
            let mut buf = vec![0u8; n];
            rand::thread_rng().fill_bytes(&mut buf);
            Ok(buf)
        })?;

        // createHash(algo) -> Hash object
        // The Hash object wraps accumulated data and computes the digest on demand.
        // Since rquickjs doesn't easily allow stateful Rust objects as JS objects,
        // we implement the Hash as a JS closure over a Rust-backed digest function.
        let create_hash_fn = Function::new(ctx.clone(),
            |ctx: Ctx<'_>, algo: String| -> rquickjs::Result<Object<'_>> {
                // Validate algorithm upfront
                match algo.as_str() {
                    "sha256" | "sha512" | "md5" => {}
                    _ => {
                        return Err(rquickjs::Error::new_from_js_message(
                            "string", "string",
                            &format!("Error: unsupported hash algorithm: '{algo}'"),
                        ));
                    }
                }

                // We'll accumulate data in a JS array of strings, then
                // compute the digest in Rust when digest() is called.
                // This avoids needing to hold Rust state in a JS object.
                let hash_obj = Object::new(ctx.clone())?;
                let data_key = "__data";
                let algo_key = "__algo";
                hash_obj.set(data_key, rquickjs::Array::new(ctx.clone())?)?;
                hash_obj.set(algo_key, algo)?;

                // Rust-backed digest function: takes (algo, data_parts, encoding) -> result
                let digest_impl: Function<'_> = ctx.globals().get("__simulacra_crypto_digest")?;
                hash_obj.set("__digest_impl", digest_impl)?;

                // update(data) -> this (JS function that appends to __data)
                ctx.eval::<(), _>(r#"
                    // Attach update and digest methods to the most recently created hash
                    // We use a global to pass the object reference.
                "#)?;

                // Actually, simpler approach: define update/digest as JS that
                // references `this`. Register them as methods on hash_obj.
                let update_src = r#"
                    (function(data) {
                        this.__data.push(String(data));
                        return this;
                    })
                "#;
                let update_fn: Function<'_> = ctx.eval(update_src)?;
                hash_obj.set("update", update_fn)?;

                let digest_src = r#"
                    (function(encoding) {
                        const combined = this.__data.join('');
                        return this.__digest_impl(this.__algo, combined, encoding || 'hex');
                    })
                "#;
                let digest_fn: Function<'_> = ctx.eval(digest_src)?;
                hash_obj.set("digest", digest_fn)?;

                Ok(hash_obj)
            }
        )?;
        exports.export("createHash", create_hash_fn.clone())?;

        // Default export
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("randomUUID", random_uuid_fn)?;
        default_obj.set("randomBytes", random_bytes_fn)?;
        default_obj.set("createHash", create_hash_fn)?;
        // getRandomValues is set via JS wrapper below
        exports.export("default", default_obj)?;

        Ok(())
    }
}
```

**Important implementation note:** The `createHash` pattern above is a sketch. The `update().digest()` chaining with `this` context is tricky in rquickjs. The implementing agent should consider these alternatives:

1. **Pure JS accumulator + Rust digest:** Register a global `__simulacra_crypto_digest(algo, data, encoding)` Rust function. The Hash object is pure JS: `update(data)` appends to an internal array, `digest(encoding)` joins the array and calls the Rust function. This is the simplest approach.

2. **Rust opaque type via `Class`:** Use rquickjs `Class` to define a `Hash` class with Rust state. More complex but cleaner.

Approach 1 is recommended. The `__simulacra_crypto_digest` function:

```rust
// Register in globals.rs or crypto_module.rs during init:
let digest_fn = Function::new(ctx.clone(),
    |algo: String, data: String, encoding: String| -> rquickjs::Result<rquickjs::Value<'_>> {
        use sha2::{Sha256, Sha512, Digest};
        use md5::Md5;

        let hash_bytes = match algo.as_str() {
            "sha256" => {
                let mut hasher = Sha256::new();
                hasher.update(data.as_bytes());
                hasher.finalize().to_vec()
            }
            "sha512" => {
                let mut hasher = Sha512::new();
                hasher.update(data.as_bytes());
                hasher.finalize().to_vec()
            }
            "md5" => {
                let mut hasher = Md5::new();
                hasher.update(data.as_bytes());
                hasher.finalize().to_vec()
            }
            _ => return Err(rquickjs::Error::new_from_js_message(
                "string", "string",
                &format!("unsupported algorithm: {algo}"),
            )),
        };

        match encoding.as_str() {
            "hex" => Ok(rquickjs::String::from_str(ctx.clone(), &hex::encode(&hash_bytes))?.into()),
            "base64" => Ok(rquickjs::String::from_str(ctx.clone(), &base64::engine::general_purpose::STANDARD.encode(&hash_bytes))?.into()),
            _ => {
                // Return as Uint8Array
                // ... convert hash_bytes to JS Uint8Array ...
            }
        }
    }
)?;
globals.set("__simulacra_crypto_digest", digest_fn)?;
```

The `getRandomValues` function should be a JS wrapper:

```javascript
globalThis.__simulacra_getRandomValues = function(typedArray) {
    const bytes = __simulacra_fill_random(typedArray.byteLength);
    const view = new Uint8Array(typedArray.buffer);
    for (let i = 0; i < bytes.length; i++) view[i] = bytes[i];
    return typedArray;
};
```

- [ ] **Step 3: Wire into `lib.rs`**

1. Add `mod crypto_module;` to `lib.rs`.
2. Add `"crypto"` to the `SIMULACRA_MODULES` array.
3. Register `__simulacra_crypto_digest` and `__simulacra_fill_random` as global Rust functions in `register_globals`.
4. In `register_native_modules`, add `simulacra:crypto` registration (same pattern as `simulacra:path`).
5. In the `SimulacraLoader::load` match, add `"simulacra:crypto"` to the built-in short-circuit.

- [ ] **Step 4: Add tests for simulacra:crypto**

Add to `crates/simulacra-quickjs/src/tests.rs`:

```rust
// --- Tier 3: simulacra:crypto ---
#[test]
fn crypto_random_uuid_format() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        const u = crypto.randomUUID();
        /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u)
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_random_uuid_unique() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.randomUUID() !== crypto.randomUUID()
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_random_bytes_length() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.randomBytes(16).length
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("16"));
}

#[test]
fn crypto_random_bytes_zero() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.randomBytes(0).length
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("0"));
}

#[test]
fn crypto_sha256_hex() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("sha256").update("hello").digest("hex")
    "#).unwrap();
    // SHA-256 of "hello"
    assert_eq!(out.result.as_deref(), Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"));
}

#[test]
fn crypto_sha512_base64() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("sha512").update("data").digest("base64")
    "#).unwrap();
    // Verify it's a non-empty base64 string
    let result = out.result.unwrap();
    assert!(!result.is_empty());
    assert!(result.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
}

#[test]
fn crypto_md5_hex() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("md5").update("test").digest("hex")
    "#).unwrap();
    // MD5 of "test"
    assert_eq!(out.result.as_deref(), Some("098f6bcd4621d373cade4e832627b4f6"));
}

#[test]
fn crypto_create_hash_unknown_throws() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("unknown")
    "#);
    assert!(result.is_err());
}

#[test]
fn crypto_hash_update_chainable() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        const h1 = crypto.createHash("sha256").update("a").update("b").digest("hex");
        const h2 = crypto.createHash("sha256").update("ab").digest("hex");
        h1 === h2
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_get_random_values() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        const arr = new Uint8Array(8);
        const result = crypto.getRandomValues(arr);
        result === arr && result.length === 8
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_get_random_values_exceeds_limit() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        crypto.getRandomValues(new Uint8Array(65537))
    "#);
    assert!(result.is_err());
}

#[test]
fn crypto_named_import() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import { randomUUID } from 'simulacra:crypto';
        typeof randomUUID()
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("string"));
}
```

- [ ] **Step 5: Run tests, implement, iterate**

```bash
cargo test -p simulacra-quickjs
```

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-quickjs/
git commit -m "feat(quickjs): simulacra:crypto native module with UUID, hashing, random bytes [S027]"
```

---

### Task 4: fs Module Completions

**Files:**
- Modify: `crates/simulacra-quickjs/src/lib.rs` (add host functions to `fs` global, extend `FsProxy` trait)
- Modify: `crates/simulacra-quickjs/src/tests.rs`

This task adds `readdirSync`, `statSync`, `unlinkSync`, `renameSync`, `appendFileSync` to the existing `fs` global object. Each function delegates through `FsProxy` when present, otherwise through the VFS directly.

**Key design decisions:**

1. **`FsProxy` extension:** The current `FsProxy` trait only has `read_file` and `write_file`. The new fs operations need additional trait methods for capability enforcement. We extend `FsProxy`:

```rust
pub trait FsProxy: Send + Sync {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String>;
    // New methods for S027:
    fn list_dir(&self, path: &str) -> Result<Vec<String>, String>;
    fn metadata(&self, path: &str) -> Result<(bool, bool, u64), String>; // (is_file, is_dir, size)
    fn remove(&self, path: &str) -> Result<(), String>;
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
}
```

This is a breaking change to `FsProxy`. All existing implementations must add the new methods. Check existing implementations (likely `AgentCell` in `simulacra-agent` or similar).

2. **`rename` in VFS:** The `VirtualFs` trait does NOT have a `rename` method. This means `renameSync` must be implemented as read + write + delete at the VFS level, or a `rename` method must be added to `VirtualFs`. Adding `rename` to `VirtualFs` is cleaner but touches `simulacra-types` and `simulacra-vfs`. The pragmatic approach: implement as read + write + delete for now, which uses existing VFS operations.

3. **`appendFileSync`:** Implemented as read (if exists) + write. Uses `FsProxy::read_file` + `FsProxy::write_file`.

- [ ] **Step 1: Extend `FsProxy` trait**

In `crates/simulacra-quickjs/src/lib.rs`, extend the `FsProxy` trait:

```rust
pub trait FsProxy: Send + Sync {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String>;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, String>;
    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String>;
    fn remove(&self, path: &str) -> Result<(), String>;
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
}
```

- [ ] **Step 2: Add fs host functions in `register_globals`**

After the existing `fs.set("mkdirSync", ...)`, add new functions. Each function checks for `fs_proxy` first, falls back to VFS:

```rust
// readdirSync(path) -> string[]
let readdir_fn = if let Some(ref proxy) = self.fs_proxy {
    let proxy = Arc::clone(proxy);
    Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<Vec<String>> {
        proxy.list_dir(&path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e)
        })
    }).map_err(|e| JsError::Runtime(e.to_string()))?
} else {
    let vfs = Arc::clone(&vfs);
    Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<Vec<String>> {
        vfs.list_dir(&path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
        })
    }).map_err(|e| JsError::Runtime(e.to_string()))?
};
fs.set("readdirSync", readdir_fn).map_err(|e| JsError::Runtime(e.to_string()))?;

// statSync(path) -> { isFile, isDirectory, size }
let stat_fn = if let Some(ref proxy) = self.fs_proxy {
    let proxy = Arc::clone(proxy);
    Function::new(ctx.clone(), move |ctx: Ctx<'_>, path: String| -> rquickjs::Result<Object<'_>> {
        let (is_file, is_dir, size) = proxy.stat(&path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e)
        })?;
        let obj = Object::new(ctx)?;
        obj.set("isFile", is_file)?;
        obj.set("isDirectory", is_dir)?;
        obj.set("size", size as f64)?;
        Ok(obj)
    }).map_err(|e| JsError::Runtime(e.to_string()))?
} else {
    let vfs = Arc::clone(&vfs);
    Function::new(ctx.clone(), move |ctx: Ctx<'_>, path: String| -> rquickjs::Result<Object<'_>> {
        let meta = vfs.metadata(&path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
        })?;
        let obj = Object::new(ctx)?;
        obj.set("isFile", meta.is_file)?;
        obj.set("isDirectory", meta.is_dir)?;
        obj.set("size", meta.size as f64)?;
        Ok(obj)
    }).map_err(|e| JsError::Runtime(e.to_string()))?
};
fs.set("statSync", stat_fn).map_err(|e| JsError::Runtime(e.to_string()))?;

// unlinkSync(path) — deletes a file
let unlink_fn = if let Some(ref proxy) = self.fs_proxy {
    let proxy = Arc::clone(proxy);
    Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<()> {
        proxy.remove(&path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e)
        })
    }).map_err(|e| JsError::Runtime(e.to_string()))?
} else {
    let vfs = Arc::clone(&vfs);
    Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<()> {
        vfs.remove(&path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
        })
    }).map_err(|e| JsError::Runtime(e.to_string()))?
};
fs.set("unlinkSync", unlink_fn).map_err(|e| JsError::Runtime(e.to_string()))?;

// renameSync(old, new) — move/rename
let rename_fn = if let Some(ref proxy) = self.fs_proxy {
    let proxy = Arc::clone(proxy);
    Function::new(ctx.clone(), move |old: String, new: String| -> rquickjs::Result<()> {
        proxy.rename(&old, &new).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e)
        })
    }).map_err(|e| JsError::Runtime(e.to_string()))?
} else {
    let vfs = Arc::clone(&vfs);
    Function::new(ctx.clone(), move |old_path: String, new_path: String| -> rquickjs::Result<()> {
        // VFS doesn't have rename — implement as read + write + delete
        let data = vfs.read(&old_path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
        })?;
        // Create parent dirs for new path
        if let Some(parent) = std::path::Path::new(&new_path).parent() {
            let parent_str = parent.to_string_lossy();
            if !parent_str.is_empty() && parent_str != "/" {
                let _ = vfs.mkdir(&parent_str);
            }
        }
        vfs.write(&new_path, &data).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
        })?;
        vfs.remove(&old_path).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
        })?;
        Ok(())
    }).map_err(|e| JsError::Runtime(e.to_string()))?
};
fs.set("renameSync", rename_fn).map_err(|e| JsError::Runtime(e.to_string()))?;

// appendFileSync(path, data) — append to file (create if absent)
let append_fn = if let Some(ref proxy) = self.fs_proxy {
    let proxy = Arc::clone(proxy);
    Function::new(ctx.clone(), move |path: String, data: String| -> rquickjs::Result<()> {
        let existing = proxy.read_file(&path).unwrap_or_default();
        let mut combined = existing;
        combined.extend_from_slice(data.as_bytes());
        proxy.write_file(&path, &combined).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e)
        })
    }).map_err(|e| JsError::Runtime(e.to_string()))?
} else {
    let vfs = Arc::clone(&vfs);
    Function::new(ctx.clone(), move |path: String, data: String| -> rquickjs::Result<()> {
        let existing = vfs.read(&path).unwrap_or_default();
        let mut combined = existing;
        combined.extend_from_slice(data.as_bytes());
        vfs.write(&path, &combined).map_err(|e| {
            rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
        })
    }).map_err(|e| JsError::Runtime(e.to_string()))?
};
fs.set("appendFileSync", append_fn).map_err(|e| JsError::Runtime(e.to_string()))?;
```

- [ ] **Step 3: Update `FsModule` ModuleDef**

Update the `FsModule` `declare` and `evaluate` to export the new functions (`readdirSync`, `statSync`, `unlinkSync`, `renameSync`, `appendFileSync`) so they are available via `import { readdirSync } from 'simulacra:fs'`.

- [ ] **Step 4: Update existing `FsProxy` implementations**

Search for all implementations of `FsProxy` in the codebase and add the new methods. This will likely be in `simulacra-agent` or `simulacra-runtime`. These implementations should delegate to the underlying VFS with capability checks.

```bash
grep -r "impl FsProxy" crates/
```

- [ ] **Step 5: Add tests for Tier 4**

Add to `crates/simulacra-quickjs/src/tests.rs`:

```rust
// --- Tier 4: fs completions ---
#[test]
fn fs_readdir_sync() {
    let vfs = make_vfs();
    vfs.write("/workspace/a.txt", b"a").unwrap();
    vfs.write("/workspace/b.txt", b"b").unwrap();
    vfs.mkdir("/workspace/subdir").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let out = rt.eval(r#"JSON.stringify(fs.readdirSync("/workspace").sort())"#).unwrap();
    let result: Vec<String> = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert!(result.contains(&"a.txt".to_string()));
    assert!(result.contains(&"b.txt".to_string()));
    assert!(result.contains(&"subdir".to_string()));
}

#[test]
fn fs_readdir_sync_excludes_dots() {
    let vfs = make_vfs();
    vfs.write("/workspace/file.txt", b"data").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let out = rt.eval(r#"
        const entries = fs.readdirSync("/workspace");
        entries.includes(".") || entries.includes("..")
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("false"));
}

#[test]
fn fs_readdir_sync_nonexistent_throws() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"fs.readdirSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_readdir_sync_on_file_throws() {
    let vfs = make_vfs();
    vfs.write("/workspace/file.txt", b"data").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let result = rt.eval(r#"fs.readdirSync("/workspace/file.txt")"#);
    assert!(result.is_err());
}

#[test]
fn fs_stat_sync_file() {
    let vfs = make_vfs();
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let out = rt.eval(r#"
        const s = fs.statSync("/workspace/file.txt");
        JSON.stringify({ isFile: s.isFile, isDirectory: s.isDirectory, size: s.size })
    "#).unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["isFile"], true);
    assert_eq!(val["isDirectory"], false);
    assert_eq!(val["size"], 5);
}

#[test]
fn fs_stat_sync_directory() {
    let vfs = make_vfs();
    vfs.mkdir("/workspace/dir").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let out = rt.eval(r#"
        const s = fs.statSync("/workspace/dir");
        JSON.stringify({ isFile: s.isFile, isDirectory: s.isDirectory })
    "#).unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["isFile"], false);
    assert_eq!(val["isDirectory"], true);
}

#[test]
fn fs_stat_sync_nonexistent_throws() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"fs.statSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_unlink_sync_deletes_file() {
    let vfs = make_vfs();
    vfs.write("/workspace/file.txt", b"data").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    rt.eval(r#"fs.unlinkSync("/workspace/file.txt")"#).unwrap();
    assert!(!vfs.exists("/workspace/file.txt"));
}

#[test]
fn fs_unlink_sync_nonexistent_throws() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"fs.unlinkSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_rename_sync_moves_file() {
    let vfs = make_vfs();
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    rt.eval(r#"fs.renameSync("/workspace/a.txt", "/workspace/b.txt")"#).unwrap();
    assert!(!vfs.exists("/workspace/a.txt"));
    assert!(vfs.exists("/workspace/b.txt"));
    assert_eq!(vfs.read("/workspace/b.txt").unwrap(), b"data");
}

#[test]
fn fs_rename_sync_nonexistent_throws() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let result = rt.eval(r#"fs.renameSync("/nonexistent", "/workspace/b.txt")"#);
    assert!(result.is_err());
}

#[test]
fn fs_rename_sync_creates_parent_dirs() {
    let vfs = make_vfs();
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    rt.eval(r#"fs.renameSync("/workspace/a.txt", "/workspace/sub/dir/b.txt")"#).unwrap();
    assert!(vfs.exists("/workspace/sub/dir/b.txt"));
}

#[test]
fn fs_append_file_sync_appends() {
    let vfs = make_vfs();
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    rt.eval(r#"fs.appendFileSync("/workspace/file.txt", " world")"#).unwrap();
    assert_eq!(vfs.read("/workspace/file.txt").unwrap(), b"hello world");
}

#[test]
fn fs_append_file_sync_creates_file() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    rt.eval(r#"fs.appendFileSync("/workspace/new.txt", "created")"#).unwrap();
    assert_eq!(vfs.read("/workspace/new.txt").unwrap(), b"created");
}
```

- [ ] **Step 6: Run tests, implement, iterate**

```bash
cargo test -p simulacra-quickjs
```

Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-quickjs/
git commit -m "feat(quickjs): fs completions (readdirSync, statSync, unlinkSync, renameSync, appendFileSync) [S027]"
```

---

### Task 5: Integration Tests + Mechanical Gate

**Files:**
- Modify: `crates/simulacra-quickjs/src/tests.rs` (integration tests)

- [ ] **Step 1: Add cross-tier integration tests**

These tests verify that the different tiers work together in a single eval context:

```rust
#[test]
fn integration_base64_encode_file_content() {
    let vfs = make_vfs();
    vfs.write("/workspace/data.txt", b"secret payload").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let out = rt.eval(r#"
        const content = fs.readFileSync("/workspace/data.txt");
        const encoded = btoa(content);
        const decoded = atob(encoded);
        decoded
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("secret payload"));
}

#[test]
fn integration_path_and_fs_operations() {
    let vfs = make_vfs();
    vfs.write("/workspace/src/main.js", b"hello").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let out = rt.eval(r#"
        import path from 'simulacra:path';
        const dir = path.dirname("/workspace/src/main.js");
        const entries = fs.readdirSync(dir);
        entries.includes("main.js")
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn integration_crypto_hash_file_content() {
    let vfs = make_vfs();
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        const content = fs.readFileSync("/workspace/file.txt");
        crypto.createHash("sha256").update(content).digest("hex")
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"));
}

#[test]
fn integration_url_parse_and_fetch_params() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const u = new URL("https://api.example.com/v1/data?key=abc&format=json");
        const params = u.searchParams;
        params.get("key") + ":" + u.pathname
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("abc:/v1/data"));
}

#[test]
fn integration_text_encoder_with_crypto() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        import crypto from 'simulacra:crypto';
        const enc = new TextEncoder();
        const bytes = enc.encode("hello");
        bytes.length
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("5"));
}

#[test]
fn integration_performance_timing() {
    let vfs = make_vfs();
    let rt = JsRuntime::new(vfs).unwrap();
    let out = rt.eval(r#"
        const start = performance.now();
        let sum = 0;
        for (let i = 0; i < 10000; i++) sum += i;
        const elapsed = performance.now() - start;
        elapsed >= 0
    "#).unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}
```

- [ ] **Step 2: Run the full mechanical gate**

```bash
cargo build --workspace 2>&1
cargo test --workspace 2>&1
cargo clippy --workspace --all-targets -- -D warnings 2>&1
cargo fmt --all -- --check 2>&1
```

All four must pass.

- [ ] **Step 3: Fix any failures**

Address compiler warnings, clippy lints, formatting issues. Re-run until clean.

- [ ] **Step 4: Final commit**

```bash
git add crates/simulacra-quickjs/
git commit -m "test(quickjs): integration tests and mechanical gate for S027 [S027]"
```

---

## Self-Review Checklist

| Check | Status |
|-------|--------|
| Every spec assertion has a corresponding test? | Verify via assertion count (59 assertions in spec, each should map to at least one test) |
| No new I/O surfaces introduced? | Tier 1: pure computation. Tier 2-3: pure computation. Tier 4: VFS-backed via existing FsProxy. |
| No new capability types required? | Tier 4 reuses existing fs:read/fs:write via FsProxy |
| `FsProxy` trait extension is backwards-compatible? | BREAKING: existing impls must add new methods. Search for all `impl FsProxy` |
| `SIMULACRA_MODULES` array updated? | Must include `"path"` and `"crypto"` |
| `SimulacraLoader::load` short-circuits for new modules? | `"simulacra:path"` and `"simulacra:crypto"` added to the match |
| `FsModule` exports updated? | Must export new fs functions for `simulacra:fs` named imports |
| `ConsoleModule` exports updated? | Must export `error`, `warn`, `info`, `debug` |
| VFS `rename` gap addressed? | Implemented as read+write+delete at VFS level |
| `JsOutput` public API unchanged? | Console levels write to stdout with prefix tags (no struct change) |
| All deps already in workspace or trivially addable? | Check `base64`, `uuid`, `sha2`, `md-5`, `rand` in root `Cargo.toml` |
| Observability assertions met? | No new spans needed (S010). `tracing::debug!` on module load. |
