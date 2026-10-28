use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt::{self, Write};
use std::rc::Rc;

use rquickjs::function::Rest;
use rquickjs::{Ctx, Function, Type, Value};

/// Maximum nesting depth for recursive value formatting.
const FORMAT_MAX_DEPTH: usize = 4;
/// Maximum number of items to display in arrays/objects before truncating.
const FORMAT_MAX_ITEMS: usize = 100;
const TRUNCATION_MARKER: &str = "[output truncated]\n";

/// Console output shared by every console method, optionally capped in bytes.
#[derive(Default)]
pub(crate) struct Capture {
    text: RefCell<String>,
    cap: Option<usize>,
    truncated: Cell<bool>,
}

pub(crate) type SharedCapture = Rc<Capture>;

impl Capture {
    pub(crate) fn new(cap: Option<usize>) -> SharedCapture {
        Rc::new(Self {
            cap,
            ..Self::default()
        })
    }

    fn room(&self) -> usize {
        self.cap
            .map_or(usize::MAX, |c| c.saturating_sub(self.text.borrow().len()))
    }

    /// The captured text, followed by a marker when the cap cut it short.
    pub(crate) fn output(&self) -> String {
        let mut text = self.text.borrow().clone();
        if self.truncated.get() {
            text.push_str(TRUNCATION_MARKER);
        }
        text
    }
}

impl Write for &Capture {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.truncated.get() {
            return Err(fmt::Error);
        }
        let mut text = self.text.borrow_mut();
        let room = self
            .cap
            .map_or(usize::MAX, |c| c.saturating_sub(text.len()));
        if s.len() <= room {
            text.push_str(s);
            return Ok(());
        }
        let mut end = room;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&s[..end]);
        self.truncated.set(true);
        Err(fmt::Error)
    }
}

/// A JS string's text, sliced inside the engine to about the room left in the
/// sink first, so a huge string never crosses into Rust whole. `slice` is the
/// intrinsic captured at setup, so script patches to `String.prototype` can't
/// swap it.
fn bounded_text<'js>(
    out: &Capture,
    slice: &Function<'js>,
    s: &rquickjs::String<'js>,
) -> Option<String> {
    let room = out.room();
    if room == usize::MAX {
        return s.to_string().ok();
    }
    let cut: rquickjs::String<'js> = slice.call((s.clone(), 0, room.saturating_add(1))).ok()?;
    cut.to_string().ok()
}

fn write_line<'js>(
    out: &mut &Capture,
    prefix: &str,
    slice: &Function<'js>,
    args: &[Value<'js>],
) -> fmt::Result {
    out.write_str(prefix)?;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.write_char(' ')?;
        }
        write_value(out, slice, arg, 0, &mut HashSet::new(), true)?;
    }
    out.write_char('\n')
}

/// A console method that writes `prefix`, the space-joined arguments and a
/// newline into `capture`, stopping as soon as the capture is full.
pub(crate) fn console_method<'js>(
    ctx: &Ctx<'js>,
    capture: SharedCapture,
    prefix: String,
) -> rquickjs::Result<Function<'js>> {
    let native = Function::new(ctx.clone(), move |args: Rest<Value<'_>>| {
        if let Some((slice, args)) = args.0.split_first()
            && let Some(slice) = slice.as_function()
        {
            let mut out = &*capture;
            let _ = write_line(&mut out, &prefix, slice, args);
        }
    })?;
    let slice: Function<'_> = ctx.eval("Function.prototype.call.bind(String.prototype.slice)")?;
    let wrap: Function<'_> =
        ctx.eval("(native, slice) => (...args) => { native(slice, ...args); }")?;
    wrap.call((native, slice))
}

/// Recursively write a JS value for `console.log` output, matching Node.js style.
///
/// Top-level strings are printed bare; nested strings are single-quoted.
/// Arrays use `[ ... ]`, objects use `{ key: val, ... }`.
/// Circular references produce `[Circular]`.
/// Depth beyond `FORMAT_MAX_DEPTH` produces `[Object]` or `[Array]`.
/// Output goes straight to `out`, so a failing writer stops the walk.
fn write_value<'js>(
    out: &mut &Capture,
    slice: &Function<'js>,
    value: &Value<'js>,
    depth: usize,
    seen: &mut HashSet<usize>,
    top_level: bool,
) -> fmt::Result {
    match value.type_of() {
        Type::Null => out.write_str("null"),
        Type::Undefined | Type::Uninitialized => out.write_str("undefined"),
        Type::Bool => match value.as_bool() {
            Some(b) => write!(out, "{b}"),
            None => out.write_str("bool"),
        },
        Type::Int => match value.as_int() {
            Some(n) => write!(out, "{n}"),
            None => out.write_str("int"),
        },
        Type::Float => match value.as_float() {
            Some(n) if n.is_nan() => out.write_str("NaN"),
            Some(n) if n.is_infinite() && n.is_sign_positive() => out.write_str("Infinity"),
            Some(n) if n.is_infinite() => out.write_str("-Infinity"),
            Some(n) => write!(out, "{n}"),
            None => out.write_str("float"),
        },
        Type::String => match value.as_string().and_then(|s| bounded_text(out, slice, s)) {
            Some(s) if top_level => out.write_str(&s),
            Some(s) => write!(out, "'{s}'"),
            None => out.write_str("string"),
        },
        Type::Symbol => {
            let desc = value
                .as_symbol()
                .and_then(|s| s.description().ok())
                .and_then(|v| v.as_string().and_then(|s| bounded_text(out, slice, s)));
            match desc {
                Some(d) => write!(out, "Symbol({d})"),
                None => out.write_str("Symbol()"),
            }
        }
        Type::Array => {
            let Some(obj) = value.as_object() else {
                return out.write_str("[Array]");
            };
            let ptr = unsafe { value.as_raw().u.ptr as usize };
            if seen.contains(&ptr) {
                return out.write_str("[Circular]");
            }
            if depth >= FORMAT_MAX_DEPTH {
                return out.write_str("[Array]");
            }
            seen.insert(ptr);
            let len: i32 = obj.get("length").unwrap_or(0);
            let display_count = (len as usize).min(FORMAT_MAX_ITEMS);
            out.write_str("[ ")?;
            for i in 0..display_count {
                if i > 0 {
                    out.write_str(", ")?;
                }
                match obj.get::<_, Value>(i as u32) {
                    Ok(elem) => write_value(out, slice, &elem, depth + 1, seen, false)?,
                    Err(_) => out.write_str("undefined")?,
                }
            }
            if (len as usize) > FORMAT_MAX_ITEMS {
                if display_count > 0 {
                    out.write_str(", ")?;
                }
                write!(out, "... {} more items", len as usize - FORMAT_MAX_ITEMS)?;
            }
            seen.remove(&ptr);
            out.write_str(" ]")
        }
        Type::Function | Type::Constructor => {
            let name = value
                .as_object()
                .and_then(|obj| obj.get::<_, rquickjs::String>("name").ok())
                .and_then(|n| bounded_text(out, slice, &n))
                .unwrap_or_default();
            if name.is_empty() {
                out.write_str("[Function (anonymous)]")
            } else {
                write!(out, "[Function: {name}]")
            }
        }
        Type::Object | Type::Exception => {
            let Some(obj) = value.as_object() else {
                return out.write_str("[Object]");
            };
            let ptr = unsafe { value.as_raw().u.ptr as usize };
            if seen.contains(&ptr) {
                return out.write_str("[Circular]");
            }
            if depth >= FORMAT_MAX_DEPTH {
                return out.write_str("[Object]");
            }
            seen.insert(ptr);
            let mut keys = obj.keys::<rquickjs::String>();
            let total = keys.len();
            if total == 0 {
                seen.remove(&ptr);
                return out.write_str("{}");
            }
            out.write_str("{ ")?;
            let mut first = true;
            for key in keys.by_ref().take(FORMAT_MAX_ITEMS).filter_map(Result::ok) {
                if let Ok(val) = obj.get::<_, Value>(key.clone()) {
                    if !first {
                        out.write_str(", ")?;
                    }
                    first = false;
                    let name = bounded_text(out, slice, &key).unwrap_or_default();
                    write!(out, "{name}: ")?;
                    write_value(out, slice, &val, depth + 1, seen, false)?;
                }
            }
            if total > FORMAT_MAX_ITEMS {
                if !first {
                    out.write_str(", ")?;
                }
                write!(out, "... {} more items", total - FORMAT_MAX_ITEMS)?;
            }
            seen.remove(&ptr);
            out.write_str(" }")
        }
        _ => write!(out, "{value:?}"),
    }
}
