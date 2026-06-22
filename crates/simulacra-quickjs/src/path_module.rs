//! Native module definition for `simulacra:path`.
//!
//! POSIX-only path manipulation. All functions map to `std::path::Path`
//! operations with POSIX normalization.

use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::{Ctx, Function, Object};
use std::path::{Path, PathBuf};

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
        let join_fn = Function::new(
            ctx.clone(),
            |args: rquickjs::function::Rest<String>| -> String {
                if args.0.is_empty() {
                    return ".".to_string();
                }
                let mut result = PathBuf::new();
                for seg in &args.0 {
                    result.push(seg);
                }
                normalize_posix(&result.to_string_lossy())
            },
        )?;
        exports.export("join", join_fn.clone())?;

        // resolve(...segments) -> string (absolute against /workspace)
        let resolve_fn = Function::new(
            ctx.clone(),
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
            },
        )?;
        exports.export("resolve", resolve_fn.clone())?;

        // dirname(p) -> string
        let dirname_fn = Function::new(ctx.clone(), |p: String| -> String {
            Path::new(&p)
                .parent()
                .map(|p| {
                    let s = p.to_string_lossy().to_string();
                    if s.is_empty() { ".".to_string() } else { s }
                })
                .unwrap_or_else(|| ".".to_string())
        })?;
        exports.export("dirname", dirname_fn.clone())?;

        // basename(p, ext?) -> string
        let basename_fn = Function::new(
            ctx.clone(),
            |p: String, ext: rquickjs::function::Opt<String>| -> String {
                let base = Path::new(&p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Some(ext) = ext.0
                    && let Some(stripped) = base.strip_suffix(&ext)
                {
                    return stripped.to_string();
                }
                base
            },
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
        let normalize_fn =
            Function::new(ctx.clone(), |p: String| -> String { normalize_posix(&p) })?;
        exports.export("normalize", normalize_fn.clone())?;

        // isAbsolute(p) -> bool
        let is_absolute_fn =
            Function::new(ctx.clone(), |p: String| -> bool { p.starts_with('/') })?;
        exports.export("isAbsolute", is_absolute_fn.clone())?;

        // relative(from, to) -> string
        let relative_fn = Function::new(ctx.clone(), |from: String, to: String| -> String {
            compute_relative(&from, &to)
        })?;
        exports.export("relative", relative_fn.clone())?;

        // parse(p) -> { root, dir, base, ext, name }
        // Implemented as a Rust helper + JS wrapper to avoid rquickjs lifetime issues
        // with returning Object from closures.
        let parse_helper = Function::new(ctx.clone(), |p: String| -> Vec<String> {
            let path = Path::new(&p);
            let root = if p.starts_with('/') {
                "/".to_string()
            } else {
                String::new()
            };
            let dir = path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let name = path
                .file_stem()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            vec![root, dir, base, ext, name]
        })?;
        ctx.globals().set("__simulacra_path_parse", parse_helper)?;
        ctx.eval::<(), _>(
            r#"globalThis.__simulacra_path_parse_wrap = (p) => {
                const parts = __simulacra_path_parse(p);
                return { root: parts[0], dir: parts[1], base: parts[2], ext: parts[3], name: parts[4] };
            };"#,
        )?;
        let parse_fn: Function<'js> = ctx.globals().get("__simulacra_path_parse_wrap")?;
        exports.export("parse", parse_fn.clone())?;

        // format(obj) -> string
        let format_fn =
            Function::new(ctx.clone(), |obj: Object<'_>| -> rquickjs::Result<String> {
                let dir: String = obj.get::<_, String>("dir").unwrap_or_default();
                let base: String = obj.get::<_, String>("base").unwrap_or_default();
                let root: String = obj.get::<_, String>("root").unwrap_or_default();
                let name: String = obj.get::<_, String>("name").unwrap_or_default();
                let ext: String = obj.get::<_, String>("ext").unwrap_or_default();

                let effective_base = if !base.is_empty() {
                    base
                } else {
                    format!("{name}{ext}")
                };
                if !dir.is_empty() {
                    Ok(format!("{dir}/{effective_base}"))
                } else {
                    Ok(format!("{root}{effective_base}"))
                }
            })?;
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
                if is_absolute || parts.last().is_some_and(|s| *s != "..") {
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

    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_parts.len() - common;
    let downs = &to_parts[common..];

    let mut result: Vec<&str> = vec![".."; ups];
    result.extend_from_slice(downs);

    if result.is_empty() {
        ".".to_string()
    } else {
        result.join("/")
    }
}
