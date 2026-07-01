use crate::mount::detect_project_root;
use crate::mount::path_resolution::{expand_tilde, resolve_mount_source};

#[test]
fn detect_project_root_config_based_uses_parent_of_config_path() {
    // S020 behavior 1: project root is the parent directory of the resolved config path
    let result = detect_project_root("/home/user/project/simulacra.toml", false).unwrap();
    assert_eq!(result, std::path::PathBuf::from("/home/user/project"));
}

#[test]
fn detect_project_root_absolute_config_path_uses_parent() {
    // S020 behavior 2: absolute config path → parent
    let result = detect_project_root("/tmp/myproject/simulacra.toml", false).unwrap();
    assert_eq!(result, std::path::PathBuf::from("/tmp/myproject"));
}

#[test]
fn detect_project_root_config_at_filesystem_root_returns_error() {
    // Edge case: config path with no parent (just a filename at root-ish level)
    // "/simulacra.toml" should have parent "/"
    let result = detect_project_root("/simulacra.toml", false).unwrap();
    assert_eq!(result, std::path::PathBuf::from("/"));
}

#[test]
fn detect_project_root_adhoc_mode_uses_cwd() {
    // S020 behavior 5: ad-hoc mode uses current working directory
    let cwd = std::env::current_dir().unwrap();
    let result = detect_project_root("unused", true).unwrap();
    // On macOS, /var -> /private/var, so strip_private_prefix may adjust
    // Just verify it's a valid directory
    assert!(
        result.is_absolute(),
        "ad-hoc project root should be absolute"
    );
    // The result should match cwd (possibly with /private stripped on macOS)
    #[cfg(target_os = "macos")]
    {
        let cwd_str = cwd.to_string_lossy();
        let result_str = result.to_string_lossy();
        // Either they match directly, or one has /private prefix stripped
        assert!(
            cwd_str == result_str
                || cwd_str.starts_with("/private") && result_str == cwd_str.replace("/private", "")
                || result_str.starts_with("/private")
                    && cwd_str == result_str.replace("/private", ""),
            "ad-hoc root {result_str} should correspond to cwd {cwd_str}"
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert_eq!(result, cwd);
    }
}

#[test]
fn resolve_mount_source_absolute_path_returned_directly() {
    // S020 assertion: absolute source paths used directly
    let root = std::path::Path::new("/project");
    let result = resolve_mount_source("/usr/share/data", root);
    assert_eq!(result, std::path::PathBuf::from("/usr/share/data"));
}

#[test]
fn resolve_mount_source_relative_path_joined_with_project_root() {
    // S020 assertion: relative source paths resolved against project root
    let root = std::path::Path::new("/home/user/project");
    let result = resolve_mount_source("prompts", root);
    assert_eq!(
        result,
        std::path::PathBuf::from("/home/user/project/prompts")
    );
}

#[test]
fn resolve_mount_source_relative_nested_path() {
    let root = std::path::Path::new("/project");
    let result = resolve_mount_source("a/b/c", root);
    assert_eq!(result, std::path::PathBuf::from("/project/a/b/c"));
}

#[cfg(unix)]
#[test]
fn expand_tilde_replaces_with_home() {
    // S020 assertion: tilde expansion on Unix
    let home = std::env::var("HOME").unwrap();
    let result = expand_tilde("~/simulacra-skills");
    assert_eq!(result, format!("{home}/simulacra-skills"));
}

#[cfg(unix)]
#[test]
fn expand_tilde_lone_tilde() {
    let home = std::env::var("HOME").unwrap();
    let result = expand_tilde("~");
    assert_eq!(result, home);
}

#[test]
fn expand_tilde_no_tilde_returns_unchanged() {
    let result = expand_tilde("/absolute/path");
    assert_eq!(result, "/absolute/path");
}

#[test]
fn expand_tilde_tilde_in_middle_returns_unchanged() {
    // Only leading ~ is expanded
    let result = expand_tilde("/path/with/~tilde");
    assert_eq!(result, "/path/with/~tilde");
}

#[cfg(unix)]
#[test]
fn resolve_mount_source_tilde_expanded_then_treated_as_absolute() {
    // After tilde expansion, the path should be absolute
    let root = std::path::Path::new("/project");
    let result = resolve_mount_source("~/data", root);
    let home = std::env::var("HOME").unwrap();
    assert_eq!(result, std::path::PathBuf::from(format!("{home}/data")));
    // Should NOT be joined with project root since it's absolute after expansion
    assert!(
        !result.starts_with("/project"),
        "tilde-expanded path should not be relative to project root"
    );
}

// ===========================================================================
// V3: copy_host_dir_to_vfs — recursive copy, limits, symlinks
// ===========================================================================
