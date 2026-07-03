//! QuickJS sandbox runtime backed by a virtual filesystem.
//!
//! Provides [`JsRuntime`] which wraps a QuickJS engine and exposes
//! `console.log`, `fs.readFileSync`, and `fs.writeFileSync` as Rust
//! host functions that route through a mediated [`FsProxy`].
//!
//! ESM modules are supported via `simulacra:` prefixed imports for built-in
//! standard library modules.

mod crypto_module;
mod formatting;
mod globals;
mod host_api;
mod module_loading;
mod native_modules;
mod path_module;
mod runtime;

use rquickjs::Error;

pub use host_api::{JsError, JsHostApiProfile, JsOutput, install_workflow_api_restrictions};
pub use runtime::JsRuntime;

/// Error reported by JS filesystem host functions when no capability-checking
/// proxy has been installed.
const FS_PROXY_REQUIRED_MESSAGE: &str = "fs proxy not configured for mediated filesystem access";

fn fs_proxy_required_error() -> Error {
    Error::new_from_js_message("FsProxy", "configured FsProxy", FS_PROXY_REQUIRED_MESSAGE)
}

/// Trait for fetching remote module source text over the network.
///
/// The implementation is responsible for capability checks, HTTP fetching,
/// and error handling. The runtime calls this for `http://` and `https://`
/// module specifiers.
pub trait ModuleFetcher: Send + Sync {
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

#[cfg(test)]
mod tests;
