//! WHATWG-compliant `Headers` implementation.
//!
//! Header names are lowercased on insertion, and iteration order is sorted by
//! name (per the WHATWG Fetch specification).

use rquickjs::Ctx;

/// A WHATWG-compliant Headers collection.
///
/// Stores headers as `(name, value)` pairs with case-insensitive name matching.
/// Names are normalized to lowercase on insertion.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

/// Returns `true` if `name` is a valid WHATWG header name
/// (tchar set: `[A-Za-z0-9!#$%&'*+-.^_`|~]+`, non-empty).
fn is_valid_header_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    })
}

/// Returns `true` if `value` contains no NUL, CR, or LF bytes.
fn is_valid_header_value(value: &str) -> bool {
    !value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
}

/// Strip leading/trailing HTTP whitespace (space, tab, CR, LF) per WHATWG.
fn normalize_header_value(value: &str) -> String {
    value
        .trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n')
        .to_string()
}

impl Headers {
    /// Create an empty `Headers` collection.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Create a `Headers` collection from an iterable of `(name, value)` pairs.
    ///
    /// Names are lowercased on insertion. Invalid names (empty, or containing
    /// non-tchar bytes) and invalid values (containing NUL/CR/LF) are
    /// rejected — the corresponding pair is dropped. Values have leading and
    /// trailing HTTP whitespace stripped.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        let entries = pairs
            .into_iter()
            .filter_map(|(name, value)| {
                if !is_valid_header_name(&name) {
                    return None;
                }
                let normalized = normalize_header_value(&value);
                if !is_valid_header_value(&normalized) {
                    return None;
                }
                Some((name.to_lowercase(), normalized))
            })
            .collect();
        Self { entries }
    }

    /// Returns the comma-joined values for all entries matching `name`,
    /// or `None` if no entries match.
    pub fn get(&self, name: &str) -> Option<String> {
        let lower = name.to_lowercase();
        let values: Vec<&str> = self
            .entries
            .iter()
            .filter(|(n, _)| n == &lower)
            .map(|(_, v)| v.as_str())
            .collect();
        if values.is_empty() {
            None
        } else {
            Some(values.join(", "))
        }
    }

    /// Removes all entries with the given name, then inserts a single entry.
    ///
    /// Silently no-ops if `name` contains invalid tchar bytes or if `value`
    /// contains NUL/CR/LF (after normalization).
    pub fn set(&mut self, name: &str, value: &str) {
        if !is_valid_header_name(name) {
            return;
        }
        let normalized = normalize_header_value(value);
        if !is_valid_header_value(&normalized) {
            return;
        }
        let lower = name.to_lowercase();
        self.entries.retain(|(n, _)| n != &lower);
        self.entries.push((lower, normalized));
    }

    /// Returns `true` if any entry matches `name` (case-insensitive).
    pub fn has(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.entries.iter().any(|(n, _)| n == &lower)
    }

    /// Removes all entries matching `name` (case-insensitive).
    pub fn delete(&mut self, name: &str) {
        let lower = name.to_lowercase();
        self.entries.retain(|(n, _)| n != &lower);
    }

    /// Appends a new entry without removing existing entries for the same name.
    ///
    /// Silently no-ops if `name` contains invalid tchar bytes or if `value`
    /// contains NUL/CR/LF (after normalization).
    pub fn append(&mut self, name: &str, value: &str) {
        if !is_valid_header_name(name) {
            return;
        }
        let normalized = normalize_header_value(value);
        if !is_valid_header_value(&normalized) {
            return;
        }
        self.entries.push((name.to_lowercase(), normalized));
    }

    /// Returns all entries sorted by name.
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut sorted = self.entries.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        sorted
    }

    /// Returns all unique header names, sorted.
    pub fn keys(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.iter().map(|(n, _)| n.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Returns all header values, sorted by their associated name.
    pub fn values(&self) -> Vec<String> {
        let sorted = self.entries();
        sorted.into_iter().map(|(_, v)| v).collect()
    }

    /// Calls `f` for each entry in sorted order.
    pub fn for_each<F: FnMut(&str, &str)>(&self, mut f: F) {
        for (name, value) in self.entries() {
            f(&name, &value);
        }
    }
}

/// Register the `Headers` class as a JS global.
///
/// After calling this, JS code can use:
/// - `new Headers()`
/// - `new Headers({"key": "value"})`
/// - `new Headers([["key", "value"]])`
/// - `new Headers(existingHeaders)`
/// - `.get(name)`, `.set(name, value)`, `.has(name)`, `.delete(name)`,
///   `.append(name, value)`, `.forEach(callback)`, `.keys()`, `.values()`,
///   `.entries()`, and `for...of` iteration.
pub fn register_headers(ctx: &Ctx<'_>) -> Result<(), rquickjs::Error> {
    // We inject the Headers class via a JS wrapper backed by a Rust
    // implementation accessed through closures. This avoids the need for
    // `#[rquickjs::class]` macros which have lifetime constraints that make
    // them tricky to use with mutable internal state.
    //
    // The approach: store the Rust `Headers` state inside a JS object using
    // an opaque JSON-serialized representation, and rebuild/re-store on each
    // mutation. This is simple and correct for the header sizes we deal with.

    ctx.eval::<(), _>(
        r#"
(function() {
    // Internal storage key
    var STORAGE = Symbol("__headers_storage__");

    // WHATWG Fetch: valid header name chars are tchar from RFC 7230:
    //   [A-Za-z0-9!#$%&'*+-.^_`|~]+
    // Values must not contain CR, LF, or NUL. Leading/trailing HTTP
    // whitespace is stripped on the value, per spec.
    var NAME_RE = /^[A-Za-z0-9!#$%&'*+\-.^_`|~]+$/;

    function validateName(name) {
        var str = String(name);
        if (str.length === 0 || !NAME_RE.test(str)) {
            throw new TypeError("Invalid header name: " + JSON.stringify(str));
        }
        return str;
    }

    function validateValue(value) {
        var str = String(value);
        // Strip leading/trailing HTTP whitespace (space, tab, CR, LF)
        str = str.replace(/^[\t\r\n ]+|[\t\r\n ]+$/g, "");
        for (var i = 0; i < str.length; i++) {
            var c = str.charCodeAt(i);
            if (c === 0x00 || c === 0x0A || c === 0x0D) {
                throw new TypeError("Invalid header value: contains NUL/CR/LF");
            }
        }
        return str;
    }

    function normalizeInit(init) {
        var pairs = [];
        if (!init) return pairs;

        // Headers instance (copy constructor): entries() are already validated.
        if (init instanceof Headers) {
            return init.entries();
        }

        // Array of [name, value] pairs
        if (Array.isArray(init)) {
            for (var i = 0; i < init.length; i++) {
                var pair = init[i];
                if (!Array.isArray(pair) || pair.length !== 2) {
                    throw new TypeError("Each header pair must be a [name, value] array");
                }
                var name = validateName(pair[0]);
                var value = validateValue(pair[1]);
                pairs.push([name.toLowerCase(), value]);
            }
            return pairs;
        }

        // Plain object
        if (typeof init === "object") {
            var keys = Object.keys(init);
            for (var j = 0; j < keys.length; j++) {
                var nm = validateName(keys[j]);
                var vl = validateValue(init[keys[j]]);
                pairs.push([nm.toLowerCase(), vl]);
            }
            return pairs;
        }

        return pairs;
    }

    function Headers(init) {
        if (!(this instanceof Headers)) {
            throw new TypeError("Headers constructor requires 'new'");
        }
        this[STORAGE] = normalizeInit(init);
    }

    Headers.prototype.get = function(name) {
        var lower = validateName(name).toLowerCase();
        var values = [];
        var entries = this[STORAGE];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === lower) {
                values.push(entries[i][1]);
            }
        }
        return values.length > 0 ? values.join(", ") : null;
    };

    Headers.prototype.set = function(name, value) {
        var lower = validateName(name).toLowerCase();
        var val = validateValue(value);
        var newEntries = [];
        var entries = this[STORAGE];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] !== lower) {
                newEntries.push(entries[i]);
            }
        }
        newEntries.push([lower, val]);
        this[STORAGE] = newEntries;
    };

    Headers.prototype.has = function(name) {
        var lower = validateName(name).toLowerCase();
        var entries = this[STORAGE];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] === lower) return true;
        }
        return false;
    };

    Headers.prototype.delete = function(name) {
        var lower = validateName(name).toLowerCase();
        var newEntries = [];
        var entries = this[STORAGE];
        for (var i = 0; i < entries.length; i++) {
            if (entries[i][0] !== lower) {
                newEntries.push(entries[i]);
            }
        }
        this[STORAGE] = newEntries;
    };

    Headers.prototype.append = function(name, value) {
        var lower = validateName(name).toLowerCase();
        var val = validateValue(value);
        this[STORAGE].push([lower, val]);
    };

    Headers.prototype.entries = function() {
        var sorted = this[STORAGE].slice().sort(function(a, b) {
            return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0;
        });
        return sorted;
    };

    Headers.prototype.keys = function() {
        var sorted = this.entries();
        var result = [];
        for (var i = 0; i < sorted.length; i++) {
            if (result.length === 0 || result[result.length - 1] !== sorted[i][0]) {
                result.push(sorted[i][0]);
            }
        }
        return result;
    };

    Headers.prototype.values = function() {
        var sorted = this.entries();
        var result = [];
        for (var i = 0; i < sorted.length; i++) {
            result.push(sorted[i][1]);
        }
        return result;
    };

    Headers.prototype.forEach = function(callback, thisArg) {
        var sorted = this.entries();
        for (var i = 0; i < sorted.length; i++) {
            callback.call(thisArg, sorted[i][1], sorted[i][0], this);
        }
    };

    // Symbol.iterator for for...of support
    Headers.prototype[Symbol.iterator] = function() {
        var sorted = this.entries();
        var index = 0;
        return {
            next: function() {
                if (index < sorted.length) {
                    return { value: sorted[index++], done: false };
                }
                return { value: undefined, done: true };
            }
        };
    };

    globalThis.Headers = Headers;
})();
"#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::Value;

    // -----------------------------------------------------------------------
    // Rust-level unit tests for the Headers struct
    // -----------------------------------------------------------------------

    #[test]
    fn new_empty_headers() {
        let h = Headers::new();
        assert!(h.entries.is_empty());
        assert_eq!(h.get("anything"), None);
        assert!(!h.has("anything"));
    }

    #[test]
    fn from_pairs_object_style() {
        // Simulates constructing from key-value pairs like {key: value}
        let h = Headers::from_pairs(vec![
            ("Content-Type".into(), "text/html".into()),
            ("Accept".into(), "application/json".into()),
        ]);
        assert_eq!(h.get("content-type"), Some("text/html".into()));
        assert_eq!(h.get("accept"), Some("application/json".into()));
    }

    #[test]
    fn from_pairs_array_style() {
        // Simulates constructing from [["key", "value"], ...] arrays
        let h = Headers::from_pairs(vec![
            ("X-Custom".into(), "val1".into()),
            ("X-Custom".into(), "val2".into()),
        ]);
        // Both entries are present, get returns comma-joined
        assert_eq!(h.get("x-custom"), Some("val1, val2".into()));
    }

    #[test]
    fn copy_constructor() {
        let original = Headers::from_pairs(vec![
            ("Content-Type".into(), "text/plain".into()),
            ("Accept".into(), "application/json".into()),
        ]);
        let copy = Headers::from_pairs(original.entries());
        assert_eq!(copy.get("content-type"), Some("text/plain".into()));
        assert_eq!(copy.get("accept"), Some("application/json".into()));
    }

    #[test]
    fn get_is_case_insensitive() {
        let h = Headers::from_pairs(vec![("Content-Type".into(), "text/html".into())]);
        assert_eq!(h.get("content-type"), Some("text/html".into()));
        assert_eq!(h.get("CONTENT-TYPE"), Some("text/html".into()));
        assert_eq!(h.get("Content-Type"), Some("text/html".into()));
    }

    #[test]
    fn set_replaces_all() {
        let mut h = Headers::from_pairs(vec![
            ("x-foo".into(), "a".into()),
            ("x-foo".into(), "b".into()),
        ]);
        assert_eq!(h.get("x-foo"), Some("a, b".into()));

        h.set("X-Foo", "c");
        assert_eq!(h.get("x-foo"), Some("c".into()));
        // Only one entry now
        assert_eq!(h.entries().iter().filter(|(n, _)| n == "x-foo").count(), 1);
    }

    #[test]
    fn append_adds_without_replacing() {
        let mut h = Headers::new();
        h.append("X-Custom", "a");
        h.append("X-Custom", "b");
        assert_eq!(h.get("x-custom"), Some("a, b".into()));
        // Both entries exist
        assert_eq!(
            h.entries().iter().filter(|(n, _)| n == "x-custom").count(),
            2
        );
    }

    #[test]
    fn get_returns_comma_joined() {
        let mut h = Headers::new();
        h.append("Accept", "text/html");
        h.append("Accept", "application/json");
        h.append("Accept", "text/plain");
        assert_eq!(
            h.get("accept"),
            Some("text/html, application/json, text/plain".into())
        );
    }

    #[test]
    fn has_and_delete_case_insensitive() {
        let mut h = Headers::from_pairs(vec![("X-Token".into(), "abc".into())]);
        assert!(h.has("x-token"));
        assert!(h.has("X-TOKEN"));
        assert!(h.has("X-Token"));

        h.delete("X-TOKEN");
        assert!(!h.has("x-token"));
        assert_eq!(h.get("x-token"), None);
    }

    #[test]
    fn iteration_sorted_by_name() {
        let h = Headers::from_pairs(vec![
            ("Zebra".into(), "z".into()),
            ("Alpha".into(), "a".into()),
            ("Middle".into(), "m".into()),
        ]);
        let entries = h.entries();
        assert_eq!(entries[0], ("alpha".into(), "a".into()));
        assert_eq!(entries[1], ("middle".into(), "m".into()));
        assert_eq!(entries[2], ("zebra".into(), "z".into()));
    }

    #[test]
    fn keys_values_sorted() {
        let mut h = Headers::new();
        h.append("z-header", "z");
        h.append("a-header", "a");
        h.append("m-header", "m");

        let keys = h.keys();
        assert_eq!(keys, vec!["a-header", "m-header", "z-header"]);

        let values = h.values();
        assert_eq!(values, vec!["a", "m", "z"]);
    }

    #[test]
    fn foreach_sorted() {
        let h = Headers::from_pairs(vec![
            ("Zebra".into(), "z".into()),
            ("Alpha".into(), "a".into()),
        ]);
        let mut visited = Vec::new();
        h.for_each(|name, value| {
            visited.push((name.to_string(), value.to_string()));
        });
        assert_eq!(visited[0], ("alpha".into(), "a".into()));
        assert_eq!(visited[1], ("zebra".into(), "z".into()));
    }

    // -----------------------------------------------------------------------
    // JS integration tests
    // -----------------------------------------------------------------------

    fn with_js_context<F: FnOnce(&Ctx<'_>)>(f: F) {
        let rt = rquickjs::Runtime::new().expect("failed to create runtime");
        let ctx = rquickjs::Context::full(&rt).expect("failed to create context");
        ctx.with(|ctx| {
            register_headers(&ctx).expect("failed to register Headers");
            f(&ctx);
        });
    }

    #[test]
    fn js_headers_basic_operations() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers({"content-type": "application/json"});
                        h.append("x-custom", "a");
                        h.append("x-custom", "b");
                        return [h.get("Content-Type"), h.get("x-custom"), h.has("missing")];
                    })()
                    "#,
                )
                .expect("eval failed");

            assert_eq!(result.len(), 3);
            let ct: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(ct, "application/json");

            let custom: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(custom, "a, b");

            let missing: bool = result[2].as_bool().unwrap();
            assert!(!missing);
        });
    }

    #[test]
    fn js_headers_array_constructor() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers([["a", "1"], ["b", "2"]]);
                        return [h.get("a"), h.get("b"), h.has("c")];
                    })()
                    "#,
                )
                .expect("eval failed");

            assert_eq!(result.len(), 3);
            let a: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(a, "1");
            let b: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(b, "2");
            let c: bool = result[2].as_bool().unwrap();
            assert!(!c);
        });
    }

    #[test]
    fn js_headers_for_of_iteration() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers({"z-header": "z", "a-header": "a"});
                        var collected = [];
                        for (var pair of h) {
                            collected.push(pair[0] + "=" + pair[1]);
                        }
                        return collected;
                    })()
                    "#,
                )
                .expect("eval failed");

            assert_eq!(result.len(), 2);
            let first: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(first, "a-header=a");
            let second: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(second, "z-header=z");
        });
    }

    #[test]
    fn js_headers_copy_constructor() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var original = new Headers({"x-token": "abc123"});
                        var copy = new Headers(original);
                        original.set("x-token", "changed");
                        return [copy.get("x-token"), original.get("x-token")];
                    })()
                    "#,
                )
                .expect("eval failed");

            let copy_val: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(copy_val, "abc123");
            let orig_val: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(orig_val, "changed");
        });
    }

    #[test]
    fn js_headers_set_replaces_all() {
        with_js_context(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers();
                        h.append("x-foo", "a");
                        h.append("x-foo", "b");
                        h.set("x-foo", "c");
                        return h.get("x-foo");
                    })()
                    "#,
                )
                .expect("eval failed");

            assert_eq!(result, "c");
        });
    }

    #[test]
    fn js_headers_delete() {
        with_js_context(|ctx| {
            let result: Value<'_> = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers({"x-del": "remove-me"});
                        h.delete("X-DEL");
                        return h.get("x-del");
                    })()
                    "#,
                )
                .expect("eval failed");

            assert!(result.is_null());
        });
    }

    #[test]
    fn js_headers_foreach() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers({"z": "zv", "a": "av"});
                        var collected = [];
                        h.forEach(function(value, name) {
                            collected.push(name + ":" + value);
                        });
                        return collected;
                    })()
                    "#,
                )
                .expect("eval failed");

            let first: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(first, "a:av");
            let second: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(second, "z:zv");
        });
    }

    #[test]
    fn js_headers_keys_values() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers({"z": "zv", "a": "av", "m": "mv"});
                        var k = h.keys();
                        var v = h.values();
                        return [k.join(","), v.join(",")];
                    })()
                    "#,
                )
                .expect("eval failed");

            let keys: String = result[0].as_string().unwrap().to_string().unwrap();
            assert_eq!(keys, "a,m,z");
            let values: String = result[1].as_string().unwrap().to_string().unwrap();
            assert_eq!(values, "av,mv,zv");
        });
    }

    #[test]
    fn js_headers_invalid_name_throws() {
        with_js_context(|ctx| {
            // Space in name is invalid tchar.
            let invalid_name: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            var h = new Headers();
                            h.set("bad name", "v");
                            return false;
                        } catch(e) {
                            return e instanceof TypeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(invalid_name);

            // Empty name is invalid.
            let empty_name: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            new Headers({"": "v"});
                            return false;
                        } catch(e) {
                            return e instanceof TypeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(empty_name);
        });
    }

    #[test]
    fn js_headers_invalid_value_throws() {
        with_js_context(|ctx| {
            let crlf: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            var h = new Headers();
                            h.set("x-test", "line1\r\nInjected: evil");
                            return false;
                        } catch(e) {
                            return e instanceof TypeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(crlf, "value with CRLF should throw TypeError");

            let nul: bool = ctx
                .eval(
                    r#"
                    (function() {
                        try {
                            var h = new Headers();
                            h.set("x-test", "has\u0000nul");
                            return false;
                        } catch(e) {
                            return e instanceof TypeError;
                        }
                    })()
                    "#,
                )
                .expect("eval failed");
            assert!(nul, "value with NUL should throw TypeError");
        });
    }

    #[test]
    fn js_headers_value_whitespace_trimmed() {
        with_js_context(|ctx| {
            let result: String = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers();
                        h.set("x-pad", "  padded  ");
                        return h.get("x-pad");
                    })()
                    "#,
                )
                .expect("eval failed");
            assert_eq!(result, "padded");
        });
    }

    #[test]
    fn rust_headers_skip_invalid_name() {
        let mut h = Headers::new();
        h.set("bad name", "v");
        assert!(!h.has("bad name"));
        assert!(h.get("bad name").is_none());
    }

    #[test]
    fn rust_headers_skip_invalid_value() {
        let mut h = Headers::new();
        h.set("x-test", "line1\r\nInjected: evil");
        assert!(!h.has("x-test"));
    }

    #[test]
    fn rust_headers_value_trimmed() {
        let mut h = Headers::new();
        h.set("x-pad", "  padded  ");
        assert_eq!(h.get("x-pad"), Some("padded".to_string()));
    }

    #[test]
    fn js_headers_empty_constructor() {
        with_js_context(|ctx| {
            let result: Vec<Value<'_>> = ctx
                .eval(
                    r#"
                    (function() {
                        var h = new Headers();
                        return [h.has("any"), h.get("any")];
                    })()
                    "#,
                )
                .expect("eval failed");

            let has: bool = result[0].as_bool().unwrap();
            assert!(!has);
            assert!(result[1].is_null());
        });
    }
}
