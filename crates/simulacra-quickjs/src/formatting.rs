use std::collections::HashSet;

use rquickjs::{Type, Value};

/// Maximum nesting depth for recursive value formatting.
const FORMAT_MAX_DEPTH: usize = 4;
/// Maximum number of items to display in arrays/objects before truncating.
const FORMAT_MAX_ITEMS: usize = 100;

/// Recursively format a JS value for `console.log` output, matching Node.js style.
///
/// Top-level strings are printed bare; nested strings are single-quoted.
/// Arrays use `[ ... ]`, objects use `{ key: val, ... }`.
/// Circular references produce `[Circular]`.
/// Depth beyond `FORMAT_MAX_DEPTH` produces `[Object]` or `[Array]`.
pub(crate) fn format_js_value(
    value: &Value<'_>,
    depth: usize,
    seen: &mut HashSet<usize>,
) -> String {
    format_value_inner(value, depth, seen, depth == 0)
}

fn format_value_inner(
    value: &Value<'_>,
    depth: usize,
    seen: &mut HashSet<usize>,
    top_level: bool,
) -> String {
    match value.type_of() {
        Type::Null => "null".to_string(),
        Type::Undefined | Type::Uninitialized => "undefined".to_string(),
        Type::Bool => match value.as_bool() {
            Some(b) => b.to_string(),
            None => "bool".to_string(),
        },
        Type::Int => match value.as_int() {
            Some(n) => n.to_string(),
            None => "int".to_string(),
        },
        Type::Float => match value.as_float() {
            Some(n) => {
                if n.is_nan() {
                    "NaN".to_string()
                } else if n.is_infinite() {
                    if n.is_sign_positive() {
                        "Infinity".to_string()
                    } else {
                        "-Infinity".to_string()
                    }
                } else {
                    format!("{n}")
                }
            }
            None => "float".to_string(),
        },
        Type::String => match value.as_string().and_then(|s| s.to_string().ok()) {
            Some(s) => {
                if top_level {
                    s
                } else {
                    format!("'{s}'")
                }
            }
            None => "string".to_string(),
        },
        Type::Symbol => {
            let desc = value
                .as_symbol()
                .and_then(|s| s.description().ok())
                .and_then(|v| v.as_string().and_then(|s| s.to_string().ok()));
            match desc {
                Some(d) => format!("Symbol({d})"),
                None => "Symbol()".to_string(),
            }
        }
        Type::Array => {
            let obj = match value.as_object() {
                Some(o) => o,
                None => return "[Array]".to_string(),
            };
            let ptr = unsafe { value.as_raw().u.ptr as usize };
            if seen.contains(&ptr) {
                return "[Circular]".to_string();
            }
            if depth >= FORMAT_MAX_DEPTH {
                return "[Array]".to_string();
            }
            seen.insert(ptr);
            let len: i32 = obj.get("length").unwrap_or(0);
            let mut items = Vec::new();
            let display_count = (len as usize).min(FORMAT_MAX_ITEMS);
            for i in 0..display_count {
                if let Ok(elem) = obj.get::<_, Value>(i as u32) {
                    items.push(format_value_inner(&elem, depth + 1, seen, false));
                } else {
                    items.push("undefined".to_string());
                }
            }
            if (len as usize) > FORMAT_MAX_ITEMS {
                items.push(format!(
                    "... {} more items",
                    len as usize - FORMAT_MAX_ITEMS
                ));
            }
            seen.remove(&ptr);
            format!("[ {} ]", items.join(", "))
        }
        Type::Function | Type::Constructor => {
            if let Some(obj) = value.as_object() {
                let name: String = obj.get("name").unwrap_or_default();
                if name.is_empty() {
                    "[Function (anonymous)]".to_string()
                } else {
                    format!("[Function: {name}]")
                }
            } else {
                "[Function (anonymous)]".to_string()
            }
        }
        Type::Object | Type::Exception => {
            let obj = match value.as_object() {
                Some(o) => o,
                None => return "[Object]".to_string(),
            };
            let ptr = unsafe { value.as_raw().u.ptr as usize };
            if seen.contains(&ptr) {
                return "[Circular]".to_string();
            }
            if depth >= FORMAT_MAX_DEPTH {
                return "[Object]".to_string();
            }
            seen.insert(ptr);
            let keys: Vec<String> = obj.keys::<String>().filter_map(|k| k.ok()).collect();
            let mut items = Vec::new();
            let display_count = keys.len().min(FORMAT_MAX_ITEMS);
            for key in &keys[..display_count] {
                if let Ok(val) = obj.get::<_, Value>(key.as_str()) {
                    let formatted = format_value_inner(&val, depth + 1, seen, false);
                    items.push(format!("{key}: {formatted}"));
                }
            }
            if keys.len() > FORMAT_MAX_ITEMS {
                items.push(format!("... {} more items", keys.len() - FORMAT_MAX_ITEMS));
            }
            seen.remove(&ptr);
            if items.is_empty() {
                "{}".to_string()
            } else {
                format!("{{ {} }}", items.join(", "))
            }
        }
        _ => format!("{value:?}"),
    }
}
