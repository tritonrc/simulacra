//! QuickJS sandbox runtime backed by a virtual filesystem.
//!
//! Provides [`JsRuntime`] which wraps a QuickJS engine and exposes
//! `console.log`, `fs.readFileSync`, and `fs.writeFileSync` as Rust
//! host functions that route through a mediated [`FsProxy`].
//!
//! ESM modules are supported via `simulacra:` prefixed imports for built-in
//! standard library modules.

mod crypto_module;
mod globals;
mod path_module;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::context::EvalOptions;
use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::{CatchResultExt, Context, Function, Module, Object, Runtime, Type, Value};
use simulacra_types::VirtualFs;

/// Maximum nesting depth for recursive value formatting.
const FORMAT_MAX_DEPTH: usize = 4;
/// Maximum number of items to display in arrays/objects before truncating.
const FORMAT_MAX_ITEMS: usize = 100;
/// Error reported by JS filesystem host functions when no capability-checking
/// proxy has been installed.
const FS_PROXY_REQUIRED_MESSAGE: &str = "fs proxy not configured for mediated filesystem access";

fn fs_proxy_required_error() -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("FsProxy", "configured FsProxy", FS_PROXY_REQUIRED_MESSAGE)
}

/// Recursively format a JS value for `console.log` output, matching Node.js style.
///
/// Top-level strings are printed bare; nested strings are single-quoted.
/// Arrays use `[ ... ]`, objects use `{ key: val, ... }`.
/// Circular references produce `[Circular]`.
/// Depth beyond `FORMAT_MAX_DEPTH` produces `[Object]` or `[Array]`.
fn format_js_value(value: &Value<'_>, depth: usize, seen: &mut HashSet<usize>) -> String {
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
                    // Format and strip trailing ".0" for whole numbers
                    let s = format!("{n}");
                    s
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

/// Trait for fetching remote module source text over the network.
///
/// The implementation is responsible for capability checks, HTTP fetching,
/// and error handling. The runtime calls this for `http://` and `https://`
/// module specifiers.
pub trait ModuleFetcher {
    /// Fetch the source text of a remote module.
    ///
    /// Returns `Ok(source)` with the JS source text on success, or
    /// `Err(message)` with a human-readable error message on failure.
    fn fetch(&self, url: &str) -> Result<String, String>;
}

/// Trait for proxying filesystem operations through a capability-checking layer.
///
/// Filesystem host APIs on [`JsRuntime`] require this proxy so the embedding
/// layer can apply capabilities, budgets, journaling, and observability.
pub trait FsProxy: Send + Sync {
    /// Read a file, checking capabilities first.
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;
    /// Write a file, checking capabilities first.
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String>;
    /// Append to a file, checking write capabilities first.
    ///
    /// The default keeps simple test proxies working. Production embedders
    /// should override this when read and write capability checks differ.
    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        let existing = match self.read_file(path) {
            Ok(bytes) => bytes,
            Err(e) if e.contains("not found") || e.contains("No such file") => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut combined = existing;
        combined.extend_from_slice(data);
        self.write_file(path, &combined)
    }
    /// List directory entries, checking capabilities first.
    fn list_dir(&self, path: &str) -> Result<Vec<String>, String>;
    /// Get file/directory metadata, checking capabilities first.
    /// Returns (is_file, is_directory, size).
    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String>;
    /// Remove a file, checking capabilities first.
    fn remove(&self, path: &str) -> Result<(), String>;
    /// Rename/move a file, checking capabilities first.
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
    /// Check if a path exists, checking capabilities first.
    fn exists(&self, path: &str) -> Result<bool, String>;
    /// Create a directory, checking capabilities first.
    fn mkdir(&self, path: &str) -> Result<(), String>;
}

/// Default execution timeout for JS evaluation (5 seconds).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Output captured from a JS evaluation.
#[derive(Debug, Clone, Default)]
pub struct JsOutput {
    /// All text written via `console.log`, including trailing newlines.
    pub stdout: String,
    /// The stringified return value of the evaluated expression, if any.
    pub result: Option<String>,
    /// Exit code if `process.exit(code)` was called, otherwise `None`.
    pub exit_code: Option<i32>,
}

/// Errors from the QuickJS runtime.
#[derive(Debug, thiserror::Error)]
pub enum JsError {
    /// Error initialising or interacting with the QuickJS runtime.
    #[error("runtime error: {0}")]
    Runtime(String),
    /// An uncaught JS exception.
    #[error("execution error: {0}")]
    Execution(String),
}

// ---------------------------------------------------------------------------
// ESM module resolver + loader
// ---------------------------------------------------------------------------

const SIMULACRA_MODULES: &[&str] = &["fs", "console", "process", "path", "crypto"];

// ---------------------------------------------------------------------------
// Native ModuleDef implementations for simulacra: built-in modules
// ---------------------------------------------------------------------------

/// Native module definition for `simulacra:fs`.
///
/// Exports: `readFile`, `writeFile`, `existsSync`, `mkdirSync`,
/// `readdirSync`, `statSync`, `unlinkSync`, `renameSync`, `appendFileSync`, `default`.
/// Delegates to the global `fs` object's host functions.
struct FsModule;

impl ModuleDef for FsModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("readFile")?;
        decl.declare("writeFile")?;
        decl.declare("existsSync")?;
        decl.declare("mkdirSync")?;
        decl.declare("readdirSync")?;
        decl.declare("statSync")?;
        decl.declare("unlinkSync")?;
        decl.declare("renameSync")?;
        decl.declare("appendFileSync")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &rquickjs::Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let fs_global: Object<'js> = globals.get("fs")?;

        let read_fn: Function<'js> = fs_global.get("readFileSync")?;
        exports.export("readFile", read_fn.clone())?;

        let write_fn: Function<'js> = fs_global.get("writeFileSync")?;
        exports.export("writeFile", write_fn.clone())?;

        let exists_fn: Function<'js> = fs_global.get("existsSync")?;
        exports.export("existsSync", exists_fn.clone())?;

        let mkdir_fn: Function<'js> = fs_global.get("mkdirSync")?;
        exports.export("mkdirSync", mkdir_fn.clone())?;

        let readdir_fn: Function<'js> = fs_global.get("readdirSync")?;
        exports.export("readdirSync", readdir_fn.clone())?;

        let stat_fn: Function<'js> = fs_global.get("statSync")?;
        exports.export("statSync", stat_fn.clone())?;

        let unlink_fn: Function<'js> = fs_global.get("unlinkSync")?;
        exports.export("unlinkSync", unlink_fn.clone())?;

        let rename_fn: Function<'js> = fs_global.get("renameSync")?;
        exports.export("renameSync", rename_fn.clone())?;

        let append_fn: Function<'js> = fs_global.get("appendFileSync")?;
        exports.export("appendFileSync", append_fn.clone())?;

        // default export: object with all methods
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("readFile", read_fn)?;
        default_obj.set("writeFile", write_fn)?;
        default_obj.set("existsSync", exists_fn)?;
        default_obj.set("mkdirSync", mkdir_fn)?;
        default_obj.set("readdirSync", readdir_fn)?;
        default_obj.set("statSync", stat_fn)?;
        default_obj.set("unlinkSync", unlink_fn)?;
        default_obj.set("renameSync", rename_fn)?;
        default_obj.set("appendFileSync", append_fn)?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

/// Native module definition for `simulacra:console`.
///
/// Exports: `log`, `default`.
/// Delegates to the global `console` object.
struct ConsoleModule;

impl ModuleDef for ConsoleModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("log")?;
        decl.declare("error")?;
        decl.declare("warn")?;
        decl.declare("info")?;
        decl.declare("debug")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &rquickjs::Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let console_global: Object<'js> = globals.get("console")?;

        let log_fn: Function<'js> = console_global.get("log")?;
        exports.export("log", log_fn.clone())?;

        let error_fn: Function<'js> = console_global.get("error")?;
        exports.export("error", error_fn.clone())?;

        let warn_fn: Function<'js> = console_global.get("warn")?;
        exports.export("warn", warn_fn.clone())?;

        let info_fn: Function<'js> = console_global.get("info")?;
        exports.export("info", info_fn.clone())?;

        let debug_fn: Function<'js> = console_global.get("debug")?;
        exports.export("debug", debug_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("log", log_fn)?;
        default_obj.set("error", error_fn)?;
        default_obj.set("warn", warn_fn)?;
        default_obj.set("info", info_fn)?;
        default_obj.set("debug", debug_fn)?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

/// Native module definition for `simulacra:process`.
///
/// Exports: `env`, `cwd`, `exit`, `default`.
/// Delegates to the global `process` object.
struct ProcessModule;

impl ModuleDef for ProcessModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("env")?;
        decl.declare("cwd")?;
        decl.declare("exit")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &rquickjs::Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let process_global: Object<'js> = globals.get("process")?;

        let env_obj: Object<'js> = process_global.get("env")?;
        exports.export("env", env_obj.clone())?;

        let cwd_fn: Function<'js> = process_global.get("cwd")?;
        exports.export("cwd", cwd_fn.clone())?;

        let exit_fn: Function<'js> = process_global.get("exit")?;
        exports.export("exit", exit_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("env", env_obj)?;
        default_obj.set("cwd", cwd_fn)?;
        default_obj.set("exit", exit_fn)?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

/// Shared state between the resolver and loader for tracking fetched remote modules.
type FetchedUrls = Rc<RefCell<HashSet<String>>>;

/// Resolves `simulacra:*` specifiers; rejects bare specifiers.
struct SimulacraResolver {
    fetched_urls: FetchedUrls,
}

impl rquickjs::loader::Resolver for SimulacraResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &rquickjs::Ctx<'js>,
        base: &str,
        name: &str,
    ) -> rquickjs::Result<String> {
        // simulacra: built-in modules
        if let Some(mod_name) = name.strip_prefix("simulacra:") {
            if SIMULACRA_MODULES.contains(&mod_name) {
                return Ok(name.to_string());
            }
            let available = SIMULACRA_MODULES.join(", ");
            return Err(rquickjs::Error::new_resolving_message(
                base,
                name,
                format!("Unknown simulacra module: '{mod_name}'. Available: {available}."),
            ));
        }

        // Remote modules — pass through as-is. Network capability enforcement
        // happens in the host ModuleFetcher, not in the resolver.
        if is_remote_module_url(name) {
            // If already fetched, emit a cache hit event.
            // rquickjs will not call the loader again for a cached module,
            // so we emit the observability event here in the resolver.
            if self.fetched_urls.borrow().contains(name) {
                tracing::info!(
                    simulacra.module.cache = "hit",
                    simulacra.module.url = %name,
                    "module cache hit"
                );
            }
            return Ok(name.to_string());
        }

        // Absolute paths — if the base is a remote URL, resolve against the origin
        if name.starts_with('/') {
            if is_remote_module_url(base) {
                // Extract origin (e.g. "https://esm.sh") from the base URL
                if let Some(origin) = extract_url_origin(base) {
                    return Ok(format!("{origin}{name}"));
                }
            }
            return Ok(name.to_string());
        }

        // Relative imports — resolve against base
        if name.starts_with("./") || name.starts_with("../") {
            // If the base is a remote URL, resolve relative to it
            if is_remote_module_url(base) {
                let base_dir = base.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(base);
                let resolved = resolve_relative(base_dir, name);
                return Ok(resolved);
            }
            let base_dir = base.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
            let resolved = resolve_relative(base_dir, name);
            return Ok(resolved);
        }

        // Bare specifiers are rejected
        let message = format!(
            "Bare specifier '{name}' is not allowed. \
             Use 'simulacra:' for built-in modules or 'http(s)://' for remote modules."
        );
        tracing::error!(
            specifier = %name,
            reason = %message,
            "module resolution failed"
        );
        Err(rquickjs::Error::new_resolving_message(base, name, message))
    }
}

fn is_remote_module_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Extract the origin (scheme + host) from a URL.
/// e.g. "https://esm.sh/foo/bar" → "https://esm.sh"
fn extract_url_origin(url: &str) -> Option<String> {
    let after_scheme = url
        .strip_prefix("https://")
        .or(url.strip_prefix("http://"))?;
    let scheme = if url.starts_with("https://") {
        "https://"
    } else {
        "http://"
    };
    let host = after_scheme.split('/').next()?;
    Some(format!("{scheme}{host}"))
}

/// Resolve a relative path against a base directory.
fn resolve_relative(base_dir: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = if base_dir.is_empty() {
        vec![]
    } else {
        base_dir.split('/').collect()
    };
    for segment in relative.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Loads `simulacra:*` modules as native `ModuleDef` implementations,
/// VFS-resident modules by reading source through the filesystem proxy,
/// and remote `http://` / `https://` modules via a [`ModuleFetcher`].
struct SimulacraLoader {
    fetcher: Option<Rc<dyn ModuleFetcher>>,
    /// Optional capability-checking FS proxy for VFS-resident module reads.
    fs_proxy: Option<Arc<dyn FsProxy>>,
    /// Source cache for remote modules across eval calls on the same
    /// JsRuntime wrapper. This avoids preserving JS global/module state while
    /// still avoiding repeated network fetches for identical URLs.
    source_cache: Arc<Mutex<HashMap<String, String>>>,
    /// URLs that have already been fetched in this runtime's lifetime.
    /// Shared with `SimulacraResolver` so it can emit cache hit events.
    fetched_urls: FetchedUrls,
}

impl rquickjs::loader::Loader for SimulacraLoader {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
        // Built-in simulacra: modules are pre-registered via
        // Module::declare_def / Module::evaluate_def in JsRuntime::eval.
        // If the loader is called for them, it means they weren't pre-registered.
        match name {
            "simulacra:fs" | "simulacra:console" | "simulacra:process" | "simulacra:path"
            | "simulacra:crypto" => {
                return Err(rquickjs::Error::new_loading_message(
                    name,
                    format!("Built-in module '{name}' should have been pre-registered"),
                ));
            }
            _ => {}
        }

        // Non-simulacra modules: remote URLs, VFS paths, etc.
        let source = if is_remote_module_url(name) {
            if let Some(source) = self
                .source_cache
                .lock()
                .map_err(|e| {
                    rquickjs::Error::new_loading_message(
                        name,
                        format!("module source cache mutex poisoned: {e}"),
                    )
                })?
                .get(name)
                .cloned()
            {
                tracing::info!(
                    simulacra.module.cache = "hit",
                    simulacra.module.url = %name,
                    "module cache hit"
                );
                self.fetched_urls.borrow_mut().insert(name.to_string());
                return Module::declare(ctx.clone(), name, source);
            }

            // Remote module — fetch via the ModuleFetcher
            let _span = tracing::info_span!(
                "module_fetch",
                simulacra.operation.name = "module_fetch",
                simulacra.module.url = %name,
            )
            .entered();

            // Note: we don't short-circuit on fetched_urls here.
            // QuickJS may call the loader again across eval() boundaries
            // even for previously-fetched modules, so we always re-fetch.

            let fetcher = self.fetcher.as_ref().ok_or_else(|| {
                rquickjs::Error::new_loading_message(
                    name,
                    format!("No module fetcher configured for remote module: '{name}'"),
                )
            })?;

            match fetcher.fetch(name) {
                Ok(source) => {
                    self.fetched_urls.borrow_mut().insert(name.to_string());
                    self.source_cache
                        .lock()
                        .map_err(|e| {
                            rquickjs::Error::new_loading_message(
                                name,
                                format!("module source cache mutex poisoned: {e}"),
                            )
                        })?
                        .insert(name.to_string(), source.clone());
                    tracing::info!(simulacra.module.fetches = 1u64, "remote module fetched");
                    source
                }
                Err(msg) => {
                    return Err(rquickjs::Error::new_loading_message(name, msg));
                }
            }
        } else if name.starts_with('/') {
            // VFS-resident module — route through FsProxy so module loads are
            // subject to the same mediation as ordinary fs reads.
            let proxy = self.fs_proxy.as_ref().ok_or_else(|| {
                rquickjs::Error::new_loading_message(name, FS_PROXY_REQUIRED_MESSAGE)
            })?;
            let data = proxy.read_file(name).map_err(|e| {
                rquickjs::Error::new_loading_message(
                    name,
                    format!("Failed to load VFS module '{name}': {e}"),
                )
            })?;
            String::from_utf8(data).map_err(|e| {
                rquickjs::Error::new_loading_message(
                    name,
                    format!("VFS module '{name}' is not valid UTF-8: {e}"),
                )
            })?
        } else {
            return Err(rquickjs::Error::new_loading_message(
                name,
                format!("No loader for module: '{name}'"),
            ));
        };

        Module::declare(ctx.clone(), name, source)
    }
}

/// Extract the inner error message from rquickjs module loading errors.
///
/// rquickjs wraps loader errors as:
///   `Error: Error loading module '<name>': <inner>\n`
/// or resolver errors as:
///   `Error: Error resolving module '<name>' from '<base>': <inner>\n`
/// This function extracts `<inner>` (trimmed) when the pattern matches.
fn extract_module_loading_error(msg: &str) -> Option<String> {
    let trimmed = msg.trim();
    if let Some(rest) = trimmed.strip_prefix("Error: Error loading module '") {
        // Find the closing "': " after the module name
        if let Some(pos) = rest.find("': ") {
            let inner = &rest[pos + 3..];
            return Some(inner.to_string());
        }
    }
    if let Some(rest) = trimmed.strip_prefix("Error: Error resolving module '") {
        // Find the closing "': " after the module name
        if let Some(pos) = rest.find("': ") {
            let inner = &rest[pos + 3..];
            return Some(inner.to_string());
        }
    }
    None
}

/// Wrap ESM module code so the last expression is captured in a global.
///
/// ESM modules don't return the last expression's value. We find the last
/// non-import/export line that looks like an expression and assign it to
/// `globalThis.__simulacraResult__`.
fn wrap_module_for_result(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();

    // Find the last non-empty, non-import, non-export line
    let last_expr_idx = lines.iter().rposition(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty()
            && !trimmed.starts_with("import ")
            && !trimmed.starts_with("export ")
            && !trimmed.starts_with("//")
    });

    match last_expr_idx {
        Some(idx) => {
            let mut result = Vec::with_capacity(lines.len());
            for (i, line) in lines.iter().enumerate() {
                if i == idx {
                    let trimmed = line.trim().trim_end_matches(';');
                    result.push(format!("globalThis.__simulacraResult__ = ({trimmed});"));
                } else {
                    result.push(line.to_string());
                }
            }
            result.join("\n")
        }
        None => code.to_string(),
    }
}

// ---------------------------------------------------------------------------
// JsRuntime
// ---------------------------------------------------------------------------

/// A minimal QuickJS sandbox with mediated host functions.
pub struct JsRuntime {
    /// Maximum wall-clock time allowed for a single JS evaluation.
    timeout: Duration,
    /// Host-controlled environment variables exposed via `process.env`.
    env: HashMap<String, String>,
    /// Optional fetcher for remote ESM module source.
    module_fetcher: Option<Rc<dyn ModuleFetcher>>,
    /// Remote module source cache shared by fresh eval contexts owned by this
    /// wrapper. JS module instances are not shared across eval calls.
    module_source_cache: Arc<Mutex<HashMap<String, String>>>,
    /// Optional proxy for fs operations (capability checking).
    fs_proxy: Option<Arc<dyn FsProxy>>,
    /// Optional proxy for HTTP fetch operations (capability checking).
    fetch_proxy: Option<Arc<dyn simulacra_fetch::FetchProxy>>,
    /// Timestamp when the runtime was created, for `performance.now()`.
    runtime_start: Instant,
}

impl JsRuntime {
    /// Create a new runtime with the default timeout and no env vars.
    ///
    /// Host functions (`console`, `fs`, `process`) are registered lazily on
    /// each [`eval`](Self::eval) call so the output buffer is fresh.
    pub fn new(vfs: Arc<dyn VirtualFs>) -> Result<Self, JsError> {
        Self::with_timeout(vfs, DEFAULT_TIMEOUT)
    }

    /// Create a new runtime with a custom execution timeout.
    pub fn with_timeout(vfs: Arc<dyn VirtualFs>, timeout: Duration) -> Result<Self, JsError> {
        Self::build(vfs, timeout, None, None, None)
    }

    /// Create a new runtime with a remote module fetcher.
    pub fn with_fetcher(
        vfs: Arc<dyn VirtualFs>,
        fetcher: Box<dyn ModuleFetcher>,
    ) -> Result<Self, JsError> {
        Self::build(vfs, DEFAULT_TIMEOUT, Some(fetcher), None, None)
    }

    /// Create a new runtime with a custom timeout and a remote module fetcher.
    pub fn with_timeout_and_fetcher(
        vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Box<dyn ModuleFetcher>,
    ) -> Result<Self, JsError> {
        Self::build(vfs, timeout, Some(fetcher), None, None)
    }

    /// Create a new runtime with a custom timeout, optional fetcher, and optional fs proxy.
    pub fn with_options(
        vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Option<Box<dyn ModuleFetcher>>,
        fs_proxy: Option<Arc<dyn FsProxy>>,
    ) -> Result<Self, JsError> {
        Self::build(vfs, timeout, fetcher, fs_proxy, None)
    }

    /// Create a new runtime with all optional components.
    pub fn with_all_options(
        vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Option<Box<dyn ModuleFetcher>>,
        fs_proxy: Option<Arc<dyn FsProxy>>,
        fetch_proxy: Option<Arc<dyn simulacra_fetch::FetchProxy>>,
    ) -> Result<Self, JsError> {
        Self::build(vfs, timeout, fetcher, fs_proxy, fetch_proxy)
    }

    /// Internal builder that records host configuration. A fresh QuickJS
    /// runtime/context is created for each eval so globals and monkey patches
    /// cannot leak between tool calls.
    fn build(
        _vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Option<Box<dyn ModuleFetcher>>,
        fs_proxy: Option<Arc<dyn FsProxy>>,
        fetch_proxy: Option<Arc<dyn simulacra_fetch::FetchProxy>>,
    ) -> Result<Self, JsError> {
        Ok(Self {
            timeout,
            env: HashMap::new(),
            module_fetcher: fetcher.map(|f| Rc::from(f) as Rc<dyn ModuleFetcher>),
            module_source_cache: Arc::new(Mutex::new(HashMap::new())),
            fs_proxy,
            fetch_proxy,
            runtime_start: Instant::now(),
        })
    }

    /// Create a new runtime with host-controlled environment variables.
    pub fn with_env(
        vfs: Arc<dyn VirtualFs>,
        env: HashMap<String, String>,
    ) -> Result<Self, JsError> {
        let mut runtime = Self::with_timeout(vfs, DEFAULT_TIMEOUT)?;
        runtime.env = env;
        Ok(runtime)
    }

    /// Create a fresh QuickJS engine for one eval call and install the module
    /// loader against this wrapper's mediated host operations.
    fn fresh_engine(&self) -> Result<(Runtime, Context), JsError> {
        let rt = Runtime::new().map_err(|e| JsError::Runtime(e.to_string()))?;
        let ctx = Context::full(&rt).map_err(|e| JsError::Runtime(e.to_string()))?;

        let fetched_urls: FetchedUrls = Rc::new(RefCell::new(HashSet::new()));
        rt.set_loader(
            SimulacraResolver {
                fetched_urls: Rc::clone(&fetched_urls),
            },
            SimulacraLoader {
                fetcher: self.module_fetcher.clone(),
                fs_proxy: self.fs_proxy.clone(),
                source_cache: Arc::clone(&self.module_source_cache),
                fetched_urls,
            },
        );

        Ok((rt, ctx))
    }

    /// Pre-register native simulacra: modules via declare_def/evaluate_def.
    ///
    /// This uses Module::declare_def::<FsModule>, Module::evaluate_def::<FsModule>,
    /// Module::declare_def::<ConsoleModule>, Module::evaluate_def::<ConsoleModule>,
    /// Module::declare_def::<ProcessModule>, and Module::evaluate_def::<ProcessModule>
    /// to register the built-in modules natively rather than as synthetic JS source strings.
    fn register_native_modules(ctx: &rquickjs::Ctx<'_>) -> Result<(), JsError> {
        // Use evaluate_def which internally calls declare_def + eval.
        // We need to call finish() on the promise to ensure evaluation completes.
        let (_module, promise) =
            Module::evaluate_def::<FsModule, _>(ctx.clone(), "simulacra:fs")
                .map_err(|e| JsError::Runtime(format!("failed to register simulacra:fs: {e}")))?;
        let _: () = promise
            .finish()
            .map_err(|e| JsError::Runtime(format!("failed to evaluate simulacra:fs: {e}")))?;

        let (_module, promise) =
            Module::evaluate_def::<ConsoleModule, _>(ctx.clone(), "simulacra:console").map_err(
                |e| JsError::Runtime(format!("failed to register simulacra:console: {e}")),
            )?;
        let _: () = promise
            .finish()
            .map_err(|e| JsError::Runtime(format!("failed to evaluate simulacra:console: {e}")))?;

        let (_module, promise) =
            Module::evaluate_def::<ProcessModule, _>(ctx.clone(), "simulacra:process").map_err(
                |e| JsError::Runtime(format!("failed to register simulacra:process: {e}")),
            )?;
        let _: () = promise
            .finish()
            .map_err(|e| JsError::Runtime(format!("failed to evaluate simulacra:process: {e}")))?;

        let (_module, promise) =
            Module::evaluate_def::<path_module::PathModule, _>(ctx.clone(), "simulacra:path")
                .map_err(|e| JsError::Runtime(format!("failed to register simulacra:path: {e}")))?;
        let _: () = promise
            .finish()
            .map_err(|e| JsError::Runtime(format!("failed to evaluate simulacra:path: {e}")))?;
        tracing::debug!("simulacra:path module loaded");

        let (_module, promise) =
            Module::evaluate_def::<crypto_module::CryptoModule, _>(ctx.clone(), "simulacra:crypto")
                .map_err(|e| {
                    JsError::Runtime(format!("failed to register simulacra:crypto: {e}"))
                })?;
        let _: () = promise
            .finish()
            .map_err(|e| JsError::Runtime(format!("failed to evaluate simulacra:crypto: {e}")))?;
        tracing::debug!("simulacra:crypto module loaded");

        Ok(())
    }

    /// Register all host globals (`console`, `fs`, `process`) and return
    /// shared cells for stdout capture and exit code interception.
    #[allow(clippy::type_complexity)]
    fn register_globals(
        &self,
        ctx: &rquickjs::Ctx<'_>,
    ) -> Result<(Rc<RefCell<String>>, Rc<RefCell<Option<i32>>>), JsError> {
        let stdout_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let exit_code_cell: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));

        // --- console object ---
        let console = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;

        let buf = Rc::clone(&stdout_buf);
        let log_fn = Function::new(
            ctx.clone(),
            move |args: rquickjs::function::Rest<rquickjs::Value<'_>>| {
                let parts: Vec<String> = args
                    .0
                    .iter()
                    .map(|v| format_js_value(v, 0, &mut HashSet::new()))
                    .collect();
                let line = parts.join(" ");
                buf.borrow_mut().push_str(&line);
                buf.borrow_mut().push('\n');
            },
        )
        .map_err(|e| JsError::Runtime(e.to_string()))?;

        console
            .set("log", log_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let globals = ctx.globals();
        globals
            .set("console", console)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // --- fs object ---
        let fs = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;

        let read_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String| -> rquickjs::Result<String> {
                    let data = proxy.read_file(&path).map_err(|e| {
                        rquickjs::Error::new_from_js_message("string", "string", &e)
                    })?;
                    String::from_utf8(data).map_err(|e| {
                        rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
                    })
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<String> { Err(fs_proxy_required_error()) },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };

        fs.set("readFileSync", read_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let write_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String, data: String| -> rquickjs::Result<()> {
                    proxy
                        .write_file(&path, data.as_bytes())
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String, _data: String| -> rquickjs::Result<()> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };

        fs.set("writeFileSync", write_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // existsSync — check if a path exists (via proxy when available)
        let exists_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<bool> {
                proxy
                    .exists(&path)
                    .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<bool> { Err(fs_proxy_required_error()) },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };

        fs.set("existsSync", exists_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // mkdirSync — create a directory (via proxy when available)
        let mkdir_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<()> {
                proxy
                    .mkdir(&path)
                    .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(ctx.clone(), move |_path: String| -> rquickjs::Result<()> {
                Err(fs_proxy_required_error())
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };

        fs.set("mkdirSync", mkdir_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // readdirSync(path) -> string[]
        let readdir_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String| -> rquickjs::Result<Vec<String>> {
                    proxy
                        .list_dir(&path)
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<Vec<String>> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("readdirSync", readdir_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // statSync(path) -> { isFile, isDirectory, size }
        // Rust helper returns a Vec<String>, JS wrapper converts to an object.
        let stat_helper_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String| -> rquickjs::Result<Vec<String>> {
                    let (is_file, is_dir, size) = proxy.stat(&path).map_err(|e| {
                        rquickjs::Error::new_from_js_message("string", "string", &e)
                    })?;
                    Ok(vec![
                        is_file.to_string(),
                        is_dir.to_string(),
                        size.to_string(),
                    ])
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<Vec<String>> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        ctx.globals()
            .set("__simulacra_fs_stat", stat_helper_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;
        ctx.eval::<(), _>(
            r#"globalThis.__simulacra_fs_statSync = function(path) {
                const parts = __simulacra_fs_stat(path);
                return {
                    isFile: parts[0] === 'true',
                    isDirectory: parts[1] === 'true',
                    size: Number(parts[2])
                };
            };"#,
        )
        .map_err(|e| JsError::Runtime(format!("statSync wrapper: {e}")))?;
        let stat_fn: Function<'_> = ctx
            .globals()
            .get("__simulacra_fs_statSync")
            .map_err(|e| JsError::Runtime(e.to_string()))?;
        fs.set("statSync", stat_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // unlinkSync(path) -> void
        let unlink_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<()> {
                proxy
                    .remove(&path)
                    .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(ctx.clone(), move |_path: String| -> rquickjs::Result<()> {
                Err(fs_proxy_required_error())
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("unlinkSync", unlink_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // renameSync(oldPath, newPath) -> void
        let rename_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |old_path: String, new_path: String| -> rquickjs::Result<()> {
                    proxy
                        .rename(&old_path, &new_path)
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_old_path: String, _new_path: String| -> rquickjs::Result<()> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("renameSync", rename_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // appendFileSync(path, data) -> void
        let append_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String, data: String| -> rquickjs::Result<()> {
                    proxy
                        .append_file(&path, data.as_bytes())
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String, _data: String| -> rquickjs::Result<()> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("appendFileSync", append_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        globals
            .set("fs", fs)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // --- process object ---
        let process = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;

        let env_obj = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;
        for (key, value) in &self.env {
            env_obj
                .set(key.as_str(), value.as_str())
                .map_err(|e| JsError::Runtime(e.to_string()))?;
        }
        process
            .set("env", env_obj)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let cwd_fn = Function::new(ctx.clone(), || -> String { "/workspace".to_string() })
            .map_err(|e| JsError::Runtime(e.to_string()))?;
        process
            .set("cwd", cwd_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let exit_code_writer = Rc::clone(&exit_code_cell);
        let exit_fn = Function::new(
            ctx.clone(),
            move |code: rquickjs::function::Opt<i32>| -> rquickjs::Result<()> {
                *exit_code_writer.borrow_mut() = Some(code.0.unwrap_or(0));
                Err(rquickjs::Error::new_from_js_message(
                    "string",
                    "string",
                    "__SIMULACRA_PROCESS_EXIT__",
                ))
            },
        )
        .map_err(|e| JsError::Runtime(e.to_string()))?;
        process
            .set("exit", exit_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        globals
            .set("process", process)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        // --- fetch global (WHATWG Fetch API via simulacra-fetch) ---
        if let Some(ref fetch_proxy) = self.fetch_proxy {
            simulacra_fetch::register_globals(ctx, Arc::clone(fetch_proxy))
                .map_err(|e| JsError::Runtime(e.to_string()))?;
        }

        // --- Tier 1 web-standard globals ---
        globals::register_web_globals(ctx, &stdout_buf, self.runtime_start)?;

        Ok((stdout_buf, exit_code_cell))
    }

    /// Extract a result value from a JS value.
    fn extract_result(val: &rquickjs::Value<'_>) -> Option<String> {
        if val.is_undefined() || val.is_null() {
            None
        } else if let Some(s) = val.as_string() {
            Some(s.to_string().unwrap_or_else(|_| format!("{val:?}")))
        } else if let Some(n) = val.as_int() {
            Some(n.to_string())
        } else if let Some(n) = val.as_float() {
            Some(n.to_string())
        } else if let Some(b) = val.as_bool() {
            Some(b.to_string())
        } else {
            Some(format!("{val:?}"))
        }
    }

    /// Handle an error from JS evaluation, checking for process.exit sentinel.
    fn handle_error(
        caught: rquickjs::CaughtError<'_>,
        stdout_buf: &Rc<RefCell<String>>,
        exit_code_cell: &Rc<RefCell<Option<i32>>>,
    ) -> Result<JsOutput, JsError> {
        let exit_code = exit_code_cell.borrow_mut().take();
        if exit_code.is_some() {
            return Ok(JsOutput {
                stdout: stdout_buf.borrow().clone(),
                result: None,
                exit_code,
            });
        }

        let msg = format!("{caught}");
        // Strip rquickjs module loading wrapper to surface the inner error message.
        // rquickjs wraps loader errors as: "Error: Error loading module '<name>': <inner>\n"
        let msg = extract_module_loading_error(&msg).unwrap_or(msg);
        tracing::error!(exception.message = %msg, "uncaught JS exception");
        Err(JsError::Execution(msg))
    }

    /// Evaluate `code` and return captured output.
    ///
    /// If the code contains `import` statements, it is automatically
    /// evaluated as an ESM module. Otherwise it runs as a plain script.
    pub fn eval(&self, code: &str) -> Result<JsOutput, JsError> {
        let code = code.to_string();

        let span = tracing::info_span!(
            "js_execute",
            simulacra.operation.name = "js_execute",
            simulacra.js.module = "<eval>",
        );
        let _guard = span.enter();

        let deadline = Instant::now() + self.timeout;
        let (rt, ctx) = self.fresh_engine()?;
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));

        // Detect whether the code uses ESM imports.
        let is_module = code.contains("import ") || code.contains("export ");

        ctx.with(|ctx| {
            let (stdout_buf, exit_code_cell) = self.register_globals(&ctx)?;

            // Pre-register native simulacra: modules so imports resolve without
            // hitting the loader. Uses declare_def + evaluate_def pattern.
            Self::register_native_modules(&ctx)?;

            if is_module {
                self.eval_as_module(&ctx, &code, &stdout_buf, &exit_code_cell)
            } else {
                self.eval_as_script(&ctx, &code, &stdout_buf, &exit_code_cell)
            }
        })
    }

    fn eval_as_script(
        &self,
        ctx: &rquickjs::Ctx<'_>,
        code: &str,
        stdout_buf: &Rc<RefCell<String>>,
        exit_code_cell: &Rc<RefCell<Option<i32>>>,
    ) -> Result<JsOutput, JsError> {
        let res: rquickjs::Result<rquickjs::Value<'_>> = ctx.eval(code.to_string());

        match res.catch(ctx) {
            Ok(val) => {
                // If the eval result is a Promise (e.g. from an async IIFE),
                // drain the microtask queue so the Promise resolves before we
                // extract the result. Without this, `(async () => { ... })()`
                // returns `"Promise(0x...)"` instead of the resolved value.
                let resolved = if let Some(promise) = val.as_promise() {
                    match promise.finish::<Value<'_>>().catch(ctx) {
                        Ok(v) => v,
                        Err(caught) => {
                            return Self::handle_error(caught, stdout_buf, exit_code_cell);
                        }
                    }
                } else {
                    val
                };

                let exit_code = exit_code_cell.borrow_mut().take();
                Ok(JsOutput {
                    stdout: stdout_buf.borrow().clone(),
                    result: Self::extract_result(&resolved),
                    exit_code,
                })
            }
            Err(caught) => Self::handle_error(caught, stdout_buf, exit_code_cell),
        }
    }

    fn eval_as_module(
        &self,
        ctx: &rquickjs::Ctx<'_>,
        code: &str,
        stdout_buf: &Rc<RefCell<String>>,
        exit_code_cell: &Rc<RefCell<Option<i32>>>,
    ) -> Result<JsOutput, JsError> {
        // ESM modules don't return the last expression's value. To capture it,
        // we wrap the code: the last non-import/export statement is assigned
        // to `globalThis.__simulacraResult__`, which we read back after evaluation.
        let wrapped = wrap_module_for_result(code);

        let mut opts = EvalOptions::default();
        opts.global = false; // JS_EVAL_TYPE_MODULE
        opts.promise = true;
        let res: rquickjs::Result<rquickjs::Promise<'_>> = ctx.eval_with_options(wrapped, opts);

        match res.catch(ctx) {
            Ok(promise) => match promise.finish::<rquickjs::Value<'_>>().catch(ctx) {
                Ok(_) => {
                    let exit_code = exit_code_cell.borrow_mut().take();
                    // Read the captured result from globalThis.__simulacraResult__
                    let result_val: rquickjs::Result<rquickjs::Value<'_>> =
                        ctx.eval("globalThis.__simulacraResult__");
                    let result = result_val.ok().and_then(|v| Self::extract_result(&v));
                    Ok(JsOutput {
                        stdout: stdout_buf.borrow().clone(),
                        result,
                        exit_code,
                    })
                }
                Err(caught) => Self::handle_error(caught, stdout_buf, exit_code_cell),
            },
            Err(caught) => Self::handle_error(caught, stdout_buf, exit_code_cell),
        }
    }
}

#[cfg(test)]
mod tests;
