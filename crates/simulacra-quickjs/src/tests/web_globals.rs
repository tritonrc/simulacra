use super::support::*;

#[test]
fn btoa_encodes_hello() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"btoa("hello")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("aGVsbG8="));
}

#[test]
fn atob_decodes_hello() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"atob("aGVsbG8=")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("hello"));
}

#[test]
fn btoa_throws_on_non_latin1() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"btoa("\u{1F600}")"#);
    assert!(result.is_err());
}

#[test]
fn atob_throws_on_invalid_base64() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"atob("not valid!!!")"#);
    assert!(result.is_err());
}

#[test]
fn btoa_atob_roundtrip() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"atob(btoa("hello world 123"))"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("hello world 123"));
}

// --- TextEncoder / TextDecoder ---

#[test]
fn text_encoder_encodes_hello() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const enc = new TextEncoder();
        const bytes = enc.encode("hello");
        JSON.stringify(Array.from(bytes))
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("[104,101,108,108,111]"));
}

#[test]
fn text_encoder_decoder_roundtrip() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const enc = new TextEncoder();
        const dec = new TextDecoder();
        dec.decode(enc.encode("hello world"))
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("hello world"));
}

// --- URL / URLSearchParams ---

#[test]
fn url_parses_components() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const u = new URL("https://example.com:8080/path?q=1#frag");
        JSON.stringify({
            protocol: u.protocol,
            hostname: u.hostname,
            port: u.port,
            pathname: u.pathname,
            search: u.search,
            hash: u.hash,
        })
    "#,
        )
        .unwrap();
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
    let (rt, _) = make_runtime();
    let out = rt
        .eval(r#"new URL("/path", "https://base.com").href"#)
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("https://base.com/path"));
}

#[test]
fn url_search_params_get_set() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const p = new URLSearchParams("a=1&b=2");
        p.set("c", "3");
        p.get("a") + "," + p.get("c") + "," + p.has("b")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("1,3,true"));
}

// --- structuredClone ---

#[test]
fn structured_clone_deep_copies() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const orig = { a: { b: 1 } };
        const clone = structuredClone(orig);
        clone.a.b = 99;
        orig.a.b
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("1"));
}

// --- queueMicrotask ---

#[test]
fn queue_microtask_executes() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            queueMicrotask(() => { ran = true; });
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- performance.now ---

#[test]
fn performance_now_returns_number() {
    let (rt, _) = make_runtime();
    let out = rt.eval("typeof performance.now()").unwrap();
    assert_eq!(out.result.as_deref(), Some("number"));
}

#[test]
fn performance_now_non_decreasing() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const a = performance.now();
        const b = performance.now();
        b >= a
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- setTimeout / clearTimeout ---

#[test]
fn set_timeout_executes() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            setTimeout(() => { ran = true; }, 0);
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn set_timeout_returns_numeric_id() {
    let (rt, _) = make_runtime();
    let out = rt.eval("typeof setTimeout(() => {}, 0)").unwrap();
    assert_eq!(out.result.as_deref(), Some("number"));
}

#[test]
fn clear_timeout_cancels() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            const id = setTimeout(() => { ran = true; }, 0);
            clearTimeout(id);
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("false"));
}

#[test]
fn set_timeout_nonzero_clamps_to_zero() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            setTimeout(() => { ran = true; }, 100);
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- console levels ---

#[test]
fn console_error_writes_error_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.error("boom")"#).unwrap();
    assert!(out.stdout.contains("[ERROR]"));
    assert!(out.stdout.contains("boom"));
}

#[test]
fn console_warn_writes_warn_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.warn("careful")"#).unwrap();
    assert!(out.stdout.contains("[WARN]"));
    assert!(out.stdout.contains("careful"));
}

#[test]
fn console_info_writes_info_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.info("fyi")"#).unwrap();
    assert!(out.stdout.contains("[INFO]"));
    assert!(out.stdout.contains("fyi"));
}

#[test]
fn console_debug_writes_debug_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.debug("trace")"#).unwrap();
    assert!(out.stdout.contains("[DEBUG]"));
    assert!(out.stdout.contains("trace"));
}
