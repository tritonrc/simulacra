use super::support::*;

#[test]
fn path_join_basic() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.join("a", "b", "c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("a/b/c"));
}

#[test]
fn path_join_resolves_dotdot() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.join("/a", "b", "..", "c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/c"));
}

#[test]
fn path_resolve_absolute() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.resolve("a", "b")
    "#,
        )
        .unwrap();
    let result = out.result.unwrap();
    assert!(
        result.starts_with('/'),
        "resolve should produce absolute path, got: {result}"
    );
}

#[test]
fn path_dirname() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.dirname("/a/b/c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/b"));
}

#[test]
fn path_basename() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.basename("/a/b/c.txt")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("c.txt"));
}

#[test]
fn path_basename_strips_ext() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.basename("/a/b/c.txt", ".txt")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("c"));
}

#[test]
fn path_extname() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.extname("file.tar.gz")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some(".gz"));
}

#[test]
fn path_normalize() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.normalize("/a//b/../c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/c"));
}

#[test]
fn path_is_absolute() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        JSON.stringify([path.isAbsolute("/a"), path.isAbsolute("a")])
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("[true,false]"));
}

#[test]
fn path_relative() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.relative("/a/b", "/a/c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("../c"));
}

#[test]
fn path_parse_and_format() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        const parsed = path.parse("/a/b/c.txt");
        JSON.stringify(parsed)
    "#,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["root"], "/");
    assert_eq!(val["dir"], "/a/b");
    assert_eq!(val["base"], "c.txt");
    assert_eq!(val["ext"], ".txt");
    assert_eq!(val["name"], "c");
}

#[test]
fn path_format_inverse_of_parse() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.format({ dir: "/a/b", base: "c.txt" })
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/b/c.txt"));
}

#[test]
fn path_sep_and_delimiter() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.sep + "," + path.delimiter
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/,:"));
}

#[test]
fn path_named_import() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import { join } from 'simulacra:path';
        join("a", "b")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("a/b"));
}

// ---------------------------------------------------------------------------
// simulacra:crypto tests
// ---------------------------------------------------------------------------

#[test]
fn crypto_random_uuid_format() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        const u = crypto.randomUUID();
        /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u)
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_random_uuid_unique() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.randomUUID() !== crypto.randomUUID()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_random_bytes_length() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.randomBytes(16).length
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("16"));
}

#[test]
fn crypto_random_bytes_zero() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.randomBytes(0).length
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("0"));
}

#[test]
fn crypto_sha256_hex() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("sha256").update("hello").digest("hex")
    "#,
        )
        .unwrap();
    assert_eq!(
        out.result.as_deref(),
        Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
    );
}

#[test]
fn crypto_sha512_base64() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("sha512").update("data").digest("base64")
    "#,
        )
        .unwrap();
    let result = out.result.unwrap();
    assert!(!result.is_empty());
    assert!(
        result
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    );
}

#[test]
fn crypto_md5_hex() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("md5").update("test").digest("hex")
    "#,
        )
        .unwrap();
    assert_eq!(
        out.result.as_deref(),
        Some("098f6bcd4621d373cade4e832627b4f6")
    );
}

#[test]
fn crypto_create_hash_unknown_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(
        r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("unknown")
    "#,
    );
    assert!(result.is_err());
}

#[test]
fn crypto_hash_update_chainable() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        const h1 = crypto.createHash("sha256").update("a").update("b").digest("hex");
        const h2 = crypto.createHash("sha256").update("ab").digest("hex");
        h1 === h2
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_get_random_values() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        const arr = new Uint8Array(8);
        const result = crypto.getRandomValues(arr);
        result === arr && result.length === 8
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_get_random_values_exceeds_limit() {
    let (rt, _) = make_runtime();
    let result = rt.eval(
        r#"
        import crypto from 'simulacra:crypto';
        crypto.getRandomValues(new Uint8Array(65537))
    "#,
    );
    assert!(result.is_err());
}

#[test]
fn crypto_named_import() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import { randomUUID } from 'simulacra:crypto';
        typeof randomUUID()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("string"));
}
