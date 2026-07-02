use std::path::{Path, PathBuf};

/// Expand tilde prefix in a path string. Unix only: replaces leading `~/` or
/// lone `~` with the user's home directory. On non-Unix platforms this is a
/// no-op and returns the path unchanged.
pub(crate) fn expand_tilde(path: &str) -> String {
    #[cfg(unix)]
    {
        if (path == "~" || path.starts_with("~/"))
            && let Ok(home) = std::env::var("HOME")
        {
            return path.replacen('~', &home, 1);
        }
        path.to_string()
    }
    #[cfg(not(unix))]
    {
        path.to_string()
    }
}

/// Resolve a mount source path against the project root.
/// - Absolute paths are used directly.
/// - Tilde-prefixed paths are expanded first.
/// - Relative paths are joined with the project root.
/// - Environment variables are NOT expanded.
pub(crate) fn resolve_mount_source(source: &str, project_root: &Path) -> PathBuf {
    let expanded = expand_tilde(source);
    let path = Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(&expanded)
    }
}
