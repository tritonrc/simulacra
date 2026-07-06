use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rquickjs::Module;
use rquickjs::loader::ImportAttributes;

use crate::{FS_PROXY_REQUIRED_MESSAGE, FsProxy};

pub(crate) const SIMULACRA_MODULES: &[&str] = &["fs", "console", "process", "path", "crypto"];

pub(crate) type RemoteUrlSet = Rc<RefCell<HashSet<String>>>;

pub(crate) struct PrefetchedRemoteModules {
    pub(crate) allowed: HashSet<String>,
    pub(crate) fetched: HashSet<String>,
    pub(crate) sources: HashMap<String, String>,
}

/// Resolves `simulacra:*` specifiers and Node-like aliases for built-in modules;
/// rejects other bare specifiers.
pub(crate) struct SimulacraResolver;

impl rquickjs::loader::Resolver for SimulacraResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &rquickjs::Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
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

        // Keep this branch here as well as in resolve_module_specifier() because
        // the static-import prefetch path calls the helper directly.
        if SIMULACRA_MODULES.contains(&name) {
            return Ok(format!("simulacra:{name}"));
        }

        resolve_module_specifier(base, name).map_err(|message| {
            tracing::error!(
                specifier = %name,
                reason = %message,
                "module resolution failed"
            );
            rquickjs::Error::new_resolving_message(base, name, message)
        })
    }
}

pub(crate) fn is_remote_module_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

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

pub(crate) fn resolve_module_specifier(base: &str, name: &str) -> Result<String, String> {
    if SIMULACRA_MODULES.contains(&name) {
        return Ok(format!("simulacra:{name}"));
    }

    if is_remote_module_url(name) {
        return Ok(name.to_string());
    }

    if name.starts_with('/') {
        if is_remote_module_url(base)
            && let Some(origin) = extract_url_origin(base)
        {
            return Ok(format!("{origin}{name}"));
        }
        return Ok(name.to_string());
    }

    if name.starts_with("./") || name.starts_with("../") {
        if is_remote_module_url(base) {
            let base_dir = base.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(base);
            return Ok(resolve_relative(base_dir, name));
        }
        let base_dir = base.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        return Ok(resolve_relative(base_dir, name));
    }

    let builtins = SIMULACRA_MODULES.join(", ");
    Err(format!(
        "Bare specifier '{name}' is not allowed. \
         Built-in module aliases are available for: {builtins}. \
         Use 'simulacra:' for built-in modules or 'http(s)://' for remote modules."
    ))
}

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

pub(crate) fn static_import_specifiers(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut specifiers = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                index = skip_quoted(bytes, index).unwrap_or(bytes.len());
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            byte if is_ident_start(byte) => {
                let start = index;
                index += 1;
                while index < bytes.len() && is_ident_continue(bytes[index]) {
                    index += 1;
                }
                let ident = &source[start..index];
                if ident == "import" {
                    if let Some((specifier, next)) = parse_import_declaration(source, index) {
                        specifiers.push(specifier);
                        index = next;
                    }
                } else if ident == "export"
                    && let Some((specifier, next)) = parse_export_declaration(source, index)
                {
                    specifiers.push(specifier);
                    index = next;
                }
            }
            _ => index += 1,
        }
    }

    specifiers
}

fn parse_import_declaration(source: &str, after_import: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let index = skip_ws_and_comments(bytes, after_import);
    match bytes.get(index).copied() {
        Some(b'(') => None,
        Some(b'\'') | Some(b'"') => read_quoted_specifier(source, index),
        _ => find_from_specifier(source, index),
    }
}

fn parse_export_declaration(source: &str, after_export: usize) -> Option<(String, usize)> {
    find_from_specifier(source, after_export)
}

fn find_from_specifier(source: &str, start: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut index = start;

    while index < bytes.len() {
        match bytes[index] {
            b';' => return None,
            b'\'' | b'"' | b'`' => {
                index = skip_quoted(bytes, index).unwrap_or(bytes.len());
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            byte if is_ident_start(byte) => {
                let ident_start = index;
                index += 1;
                while index < bytes.len() && is_ident_continue(bytes[index]) {
                    index += 1;
                }
                if &source[ident_start..index] == "from" {
                    index = skip_ws_and_comments(bytes, index);
                    return read_quoted_specifier(source, index);
                }
            }
            _ => index += 1,
        }
    }

    None
}

fn read_quoted_specifier(source: &str, quote_index: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let quote = *bytes.get(quote_index)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut index = quote_index + 1;
    let start = index;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return Some((source[start..index].to_string(), index + 1)),
            _ => index += 1,
        }
    }
    None
}

fn skip_quoted(bytes: &[u8], quote_index: usize) -> Option<usize> {
    let quote = *bytes.get(quote_index)?;
    let mut index = quote_index + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

fn skip_ws_and_comments(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) == Some(&b'/') && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes.get(index) == Some(&b'/') && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        return index;
    }
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

fn is_ident_continue(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

/// Loads `simulacra:*` modules as native `ModuleDef` implementations,
/// VFS-resident modules by reading source through the filesystem proxy,
/// and remote `http://` / `https://` modules via a [`crate::ModuleFetcher`].
pub(crate) struct SimulacraLoader {
    pub(crate) fs_proxy: Option<Arc<dyn FsProxy>>,
    pub(crate) source_cache: Arc<Mutex<HashMap<String, String>>>,
    pub(crate) allowed_remote_urls: RemoteUrlSet,
    pub(crate) fetched_remote_urls: RemoteUrlSet,
}

impl rquickjs::loader::Loader for SimulacraLoader {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
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

        let source = if is_remote_module_url(name) {
            if !self.allowed_remote_urls.borrow().contains(name) {
                return Err(rquickjs::Error::new_loading_message(
                    name,
                    format!(
                        "Remote module '{name}' was not statically prefetched; dynamic remote imports are not allowed"
                    ),
                ));
            }

            let source = self
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
                .ok_or_else(|| {
                    rquickjs::Error::new_loading_message(
                        name,
                        format!(
                            "Prefetched remote module '{name}' was missing from the source cache"
                        ),
                    )
                })?;

            if !self.fetched_remote_urls.borrow().contains(name) {
                tracing::info!(
                    simulacra.module.cache = "hit",
                    simulacra.module.url = %name,
                    "module cache hit"
                );
            }
            source
        } else if name.starts_with('/') {
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

pub(crate) fn extract_module_loading_error(msg: &str) -> Option<String> {
    let trimmed = msg.trim();
    if let Some(rest) = trimmed.strip_prefix("Error: Error loading module '")
        && let Some(pos) = rest.find("': ")
    {
        return Some(rest[pos + 3..].to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("Error: Error resolving module '")
        && let Some(pos) = rest.find("': ")
    {
        return Some(rest[pos + 3..].to_string());
    }
    None
}

pub(crate) fn wrap_module_for_result(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
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
