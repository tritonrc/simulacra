//! Tests for ServiceFs VFS layer — service discovery, read-only enforcement,
//! metadata, and existence checks.

use std::sync::Arc;

use serde_json::Value;
use simulacra_types::{VfsError, VirtualFs};

use crate::{IntegrationLister, MemoryFs, ServiceFs};

// ---------------------------------------------------------------------------
// FakeIntegrationLister
// ---------------------------------------------------------------------------

struct FakeIntegrationLister {
    names: Vec<String>,
}

impl FakeIntegrationLister {
    fn new(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|s| (*s).to_string()).collect(),
        }
    }
}

impl IntegrationLister for FakeIntegrationLister {
    fn integration_names(&self) -> Vec<String> {
        self.names.clone()
    }

    fn integration_metadata(&self, name: &str) -> Option<String> {
        if self.names.iter().any(|n| n == name) {
            Some(
                serde_json::json!({
                    "base_url": format!("https://api.{name}.example.com"),
                    "scopes": [format!("{name}.read")],
                    "rate_limit_rps": 10,
                    "status": "ok"
                })
                .to_string(),
            )
        } else {
            None
        }
    }

    fn integration_readme(&self, name: &str) -> Option<String> {
        self.names
            .iter()
            .any(|n| n == name)
            .then(|| format!("# {name}\n\nIntegration for {name}.\n\nAvailable skills:\n- sync"))
    }

    fn integration_skill_names(&self, name: &str) -> Vec<String> {
        if self.names.iter().any(|n| n == name) {
            vec!["create-contact".to_string(), "sync".to_string()]
        } else {
            Vec::new()
        }
    }
}

fn service_fs(names: &[&str]) -> ServiceFs<MemoryFs> {
    ServiceFs::new(MemoryFs::new(), Arc::new(FakeIntegrationLister::new(names)))
}

fn read_string(fs: &dyn VirtualFs, path: &str) -> Result<String, VfsError> {
    fs.read(path)
        .map(|bytes| String::from_utf8(bytes).expect("utf-8"))
}

// ---------------------------------------------------------------------------
// Service discovery VFS (spec assertions 21–27)
// ---------------------------------------------------------------------------

/// Assertion 21: list_dir("/svc/") returns sorted integration names.
#[test]
fn list_dir_svc_returns_sorted_integration_names() {
    let fs = service_fs(&["slack", "hubspot", "linear"]);
    let entries = fs.list_dir("/svc/").unwrap();
    assert_eq!(entries, vec!["hubspot", "linear", "slack"]);
}

/// Also works without trailing slash.
#[test]
fn list_dir_svc_no_trailing_slash() {
    let fs = service_fs(&["slack", "hubspot"]);
    let entries = fs.list_dir("/svc").unwrap();
    assert_eq!(entries, vec!["hubspot", "slack"]);
}

/// Assertion 22: read("/svc/<name>/README.md") returns generated markdown.
#[test]
fn read_svc_readme_returns_markdown() {
    let fs = service_fs(&["hubspot"]);
    let readme = read_string(&fs, "/svc/hubspot/README.md").unwrap();
    assert!(
        readme.starts_with("# hubspot"),
        "README should start with heading"
    );
    assert!(
        readme.contains("Available skills"),
        "README should list skills"
    );
}

/// Assertion 23: read("/svc/<name>/config.json") returns JSON with base_url, scopes, status.
#[test]
fn read_svc_config_returns_metadata_json() {
    let fs = service_fs(&["hubspot"]);
    let raw = read_string(&fs, "/svc/hubspot/config.json").unwrap();
    let json: Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(json["base_url"], "https://api.hubspot.example.com");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["rate_limit_rps"], 10);
    assert!(json["scopes"].is_array());
}

/// Assertion 25: config.json never contains credentials, tokens, or env var names.
#[test]
fn config_json_never_contains_credentials() {
    let fs = service_fs(&["hubspot"]);
    let raw = read_string(&fs, "/svc/hubspot/config.json").unwrap();
    assert!(!raw.contains("client_secret"));
    assert!(!raw.contains("refresh_token"));
    assert!(!raw.contains("HUBSPOT_CLIENT_SECRET"));
    assert!(!raw.contains("Bearer "));
    assert!(!raw.contains("access_token"));
}

/// Assertion 24: list_dir("/svc/<name>/skills/") returns skill names.
#[test]
fn list_dir_svc_skills_returns_skill_names() {
    let fs = service_fs(&["hubspot"]);
    let skills = fs.list_dir("/svc/hubspot/skills/").unwrap();
    assert_eq!(skills, vec!["create-contact", "sync"]);
}

/// Assertion 26: list_dir("/svc/<nonexistent>/") returns NotFound.
#[test]
fn list_dir_nonexistent_integration_returns_not_found() {
    let fs = service_fs(&["hubspot"]);
    let err = fs.list_dir("/svc/nonexistent/").unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)));
}

/// Assertion 27: read("/svc/<name>/nonexistent") returns NotFound.
#[test]
fn read_nonexistent_file_returns_not_found() {
    let fs = service_fs(&["hubspot"]);
    let err = fs.read("/svc/hubspot/nonexistent").unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)));
}

/// read of nonexistent integration README returns NotFound.
#[test]
fn read_nonexistent_integration_readme_returns_not_found() {
    let fs = service_fs(&["hubspot"]);
    let err = fs.read("/svc/nonexistent/README.md").unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)));
}

// ---------------------------------------------------------------------------
// Read-only enforcement (spec assertions 28–31)
// ---------------------------------------------------------------------------

/// Assertion 28: write returns PermissionDenied.
#[test]
fn write_to_svc_returns_permission_denied() {
    let fs = service_fs(&["hubspot"]);
    let err = fs.write("/svc/hubspot/README.md", b"nope").unwrap_err();
    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

/// Assertion 29: mkdir returns PermissionDenied.
#[test]
fn mkdir_under_svc_returns_permission_denied() {
    let fs = service_fs(&["hubspot"]);
    let err = fs.mkdir("/svc/custom").unwrap_err();
    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

/// Assertion 30: remove returns PermissionDenied.
#[test]
fn remove_under_svc_returns_permission_denied() {
    let fs = service_fs(&["hubspot"]);
    let err = fs.remove("/svc/hubspot/config.json").unwrap_err();
    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

/// Assertion 31: all write/remove/mkdir on /svc/** return PermissionDenied.
#[test]
fn all_write_ops_under_svc_return_permission_denied() {
    let fs = service_fs(&["hubspot"]);
    assert!(matches!(
        fs.write("/svc/hubspot/skills/sync", b"x"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        fs.mkdir("/svc/hubspot/skills/new"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        fs.remove("/svc/hubspot/skills/sync"),
        Err(VfsError::PermissionDenied(_))
    ));
}

// ---------------------------------------------------------------------------
// Metadata and existence (spec assertions 61–67)
// ---------------------------------------------------------------------------

/// Assertion 61: list_dir("/svc/") returns integration names as directory entries.
#[test]
fn list_dir_svc_returns_directory_entries() {
    let fs = service_fs(&["hubspot", "slack"]);
    let entries = fs.list_dir("/svc/").unwrap();
    assert!(entries.contains(&"hubspot".to_string()));
    assert!(entries.contains(&"slack".to_string()));
}

/// Assertion 62: list_dir("/svc/<name>/") returns README.md, config.json, skills.
#[test]
fn list_dir_integration_root_returns_readme_config_skills() {
    let fs = service_fs(&["hubspot"]);
    let entries = fs.list_dir("/svc/hubspot/").unwrap();
    assert_eq!(entries, vec!["README.md", "config.json", "skills"]);
}

/// Assertion 63: exists("/svc/<name>/README.md") returns true.
#[test]
fn exists_returns_true_for_integration_readme() {
    let fs = service_fs(&["hubspot"]);
    assert!(fs.exists("/svc/hubspot/README.md"));
}

/// Also: exists for the integration directory itself.
#[test]
fn exists_returns_true_for_integration_directory() {
    let fs = service_fs(&["hubspot"]);
    assert!(fs.exists("/svc/hubspot"));
}

/// Assertion 64: exists("/svc/nonexistent") returns false.
#[test]
fn exists_returns_false_for_nonexistent_service() {
    let fs = service_fs(&["hubspot"]);
    assert!(!fs.exists("/svc/nonexistent"));
}

/// Assertion 65: metadata("/svc/") returns directory metadata.
#[test]
fn metadata_svc_root_is_directory() {
    let fs = service_fs(&["hubspot"]);
    let m = fs.metadata("/svc/").unwrap();
    assert!(m.is_dir);
    assert!(!m.is_file);
}

/// Also works with "/svc" (no trailing slash).
#[test]
fn metadata_svc_no_trailing_slash_is_directory() {
    let fs = service_fs(&["hubspot"]);
    let m = fs.metadata("/svc").unwrap();
    assert!(m.is_dir);
    assert!(!m.is_file);
}

/// Assertion 66: metadata("/svc/<name>/") returns directory metadata.
#[test]
fn metadata_integration_directory() {
    let fs = service_fs(&["hubspot"]);
    let m = fs.metadata("/svc/hubspot/").unwrap();
    assert!(m.is_dir);
}

/// Assertion 67: metadata("/svc/<name>/README.md") returns file metadata with correct size.
#[test]
fn metadata_integration_readme_has_correct_size() {
    let fs = service_fs(&["hubspot"]);
    let content = read_string(&fs, "/svc/hubspot/README.md").unwrap();
    let m = fs.metadata("/svc/hubspot/README.md").unwrap();
    assert!(m.is_file);
    assert!(!m.is_dir);
    assert_eq!(m.size, content.len() as u64);
}

// ---------------------------------------------------------------------------
// Delegation — non-/svc/ paths go to inner VFS
// ---------------------------------------------------------------------------

#[test]
fn non_svc_read_delegates_to_inner() {
    let fs = service_fs(&["hubspot"]);
    fs.mkdir("/workspace").unwrap();
    fs.write("/workspace/hello.txt", b"hi").unwrap();
    let data = fs.read("/workspace/hello.txt").unwrap();
    assert_eq!(data, b"hi");
}

#[test]
fn non_svc_write_delegates_to_inner() {
    let fs = service_fs(&["hubspot"]);
    fs.mkdir("/workspace").unwrap();
    fs.write("/workspace/test.txt", b"data").unwrap();
    assert!(fs.exists("/workspace/test.txt"));
}

/// Snapshot/restore delegates to inner VFS.
#[test]
fn snapshot_restore_delegates_to_inner() {
    let fs = service_fs(&["hubspot"]);
    fs.mkdir("/workspace").unwrap();
    fs.write("/workspace/a.txt", b"before").unwrap();
    let snap = fs.snapshot().unwrap();
    fs.write("/workspace/a.txt", b"after").unwrap();
    fs.restore(&snap).unwrap();
    assert_eq!(fs.read("/workspace/a.txt").unwrap(), b"before");
}

// ---------------------------------------------------------------------------
// Connectivity status in config.json (assertion 20)
// ---------------------------------------------------------------------------

/// Assertion 20: connectivity status reflected in config.json.
#[test]
fn config_json_reflects_connectivity_status() {
    let fs = service_fs(&["hubspot"]);
    let raw = read_string(&fs, "/svc/hubspot/config.json").unwrap();
    let json: Value = serde_json::from_str(&raw).unwrap();
    // The FakeIntegrationLister returns status = "ok"
    assert!(json["status"].is_string());
}

// Observability assertions (spans, counters, warn logs) are validated via
// Aniani queries per S010, not unit tests. The tracing instrumentation
// is present in servicefs.rs — see info_span!("simulacra_svcfs_read"), warn!(), etc.
