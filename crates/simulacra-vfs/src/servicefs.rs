//! Service discovery VFS layer (`/svc/`).
//!
//! [`ServiceFs`] is a [`VirtualFs`] wrapper that intercepts reads to `/svc/**`
//! and returns integration metadata as virtual files. All other paths are
//! delegated to the inner VFS unchanged.
//!
//! All `/svc/**` writes, mkdir, and remove return [`VfsError::PermissionDenied`].

use std::sync::{Arc, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use simulacra_types::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};
use tracing::{debug, info_span, warn};

// ---------------------------------------------------------------------------
// OTel instruments
// ---------------------------------------------------------------------------

fn svcfs_reads() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("simulacra-vfs")
            .u64_counter("simulacra.svcfs.reads")
            .with_description("ServiceFs read operations by integration")
            .build()
    })
}

// ---------------------------------------------------------------------------
// IntegrationLister trait (narrow delegation, avoids depending on simulacra-integration)
// ---------------------------------------------------------------------------

/// Provides integration discovery data for `/svc/` without coupling to
/// `simulacra-integration`.
pub trait IntegrationLister: Send + Sync + 'static {
    /// All integration names available to the current tenant.
    fn integration_names(&self) -> Vec<String>;
    /// JSON metadata for the named integration, or `None` if not found.
    fn integration_metadata(&self, name: &str) -> Option<String>;
    /// Generated README markdown, or `None` if not found.
    fn integration_readme(&self, name: &str) -> Option<String>;
    /// Skill names available for the named integration.
    fn integration_skill_names(&self, name: &str) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// ServiceFs
// ---------------------------------------------------------------------------

/// A read-only VFS layer that serves `/svc/**`.
pub struct ServiceFs<V: VirtualFs> {
    inner: V,
    integrations: Arc<dyn IntegrationLister>,
}

impl<V: VirtualFs> ServiceFs<V> {
    pub fn new(inner: V, integrations: Arc<dyn IntegrationLister>) -> Self {
        Self {
            inner,
            integrations,
        }
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn is_svc(path: &str) -> bool {
    let normalized = path.trim_end_matches('/');
    normalized == "/svc" || normalized.starts_with("/svc/")
}

/// Strip `/svc/` prefix and trailing slashes. Returns `None` for `/svc` itself.
fn svc_tail(path: &str) -> Option<&str> {
    let normalized = path.trim_end_matches('/');
    if normalized == "/svc" {
        None
    } else {
        normalized.strip_prefix("/svc/")
    }
}

// ---------------------------------------------------------------------------
// Read dispatch
// ---------------------------------------------------------------------------

fn svc_read(integrations: &dyn IntegrationLister, path: &str) -> Result<Vec<u8>, VfsError> {
    let tail = svc_tail(path).ok_or_else(|| VfsError::NotAFile(path.to_string()))?;

    // tail is like "hubspot/README.md" or "hubspot/config.json"
    let (name, file) = match tail.split_once('/') {
        Some((n, f)) => (n, f),
        None => return Err(VfsError::NotAFile(path.to_string())),
    };

    match file {
        "README.md" => integrations
            .integration_readme(name)
            .map(|s| s.into_bytes())
            .ok_or_else(|| VfsError::NotFound(path.to_string())),
        "config.json" => integrations
            .integration_metadata(name)
            .map(|s| s.into_bytes())
            .ok_or_else(|| VfsError::NotFound(path.to_string())),
        _ => Err(VfsError::NotFound(path.to_string())),
    }
}

// ---------------------------------------------------------------------------
// list_dir dispatch
// ---------------------------------------------------------------------------

fn svc_list_dir(integrations: &dyn IntegrationLister, path: &str) -> Result<Vec<String>, VfsError> {
    let tail = svc_tail(path);

    match tail {
        None => {
            // "/svc" or "/svc/" — list all integration names, sorted
            let mut names = integrations.integration_names();
            names.sort();
            Ok(names)
        }
        Some(t) => {
            // Split on first /
            let (name, sub) = match t.split_once('/') {
                Some((n, s)) => (n, Some(s)),
                None => (t, None),
            };

            // Check integration exists
            if integrations.integration_metadata(name).is_none() {
                return Err(VfsError::NotFound(path.to_string()));
            }

            match sub {
                None => {
                    // "/svc/<name>/" — list standard entries
                    Ok(vec![
                        "README.md".to_string(),
                        "config.json".to_string(),
                        "skills".to_string(),
                    ])
                }
                Some("skills") | Some("skills/") => {
                    // "/svc/<name>/skills/" — list skill names
                    Ok(integrations.integration_skill_names(name))
                }
                Some(_) => Err(VfsError::NotFound(path.to_string())),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Metadata / exists
// ---------------------------------------------------------------------------

fn svc_metadata(integrations: &dyn IntegrationLister, path: &str) -> Result<FsMetadata, VfsError> {
    let normalized = path.trim_end_matches('/');

    if normalized == "/svc" {
        return Ok(FsMetadata {
            is_file: false,
            is_dir: true,
            size: 0,
        });
    }

    let tail = svc_tail(path).unwrap_or("");

    // Check if it's an integration directory name (e.g., "hubspot")
    if !tail.contains('/') {
        if integrations.integration_metadata(tail).is_some() {
            return Ok(FsMetadata {
                is_file: false,
                is_dir: true,
                size: 0,
            });
        }
        return Err(VfsError::NotFound(path.to_string()));
    }

    // Check if it's a known sub-directory like "hubspot/skills"
    let (name, file) = tail.split_once('/').unwrap();
    if integrations.integration_metadata(name).is_none() {
        return Err(VfsError::NotFound(path.to_string()));
    }

    match file {
        "skills" => Ok(FsMetadata {
            is_file: false,
            is_dir: true,
            size: 0,
        }),
        "README.md" | "config.json" => {
            // Read the content to get the size
            match svc_read(integrations, path) {
                Ok(bytes) => Ok(FsMetadata {
                    is_file: true,
                    is_dir: false,
                    size: bytes.len() as u64,
                }),
                Err(e) => Err(e),
            }
        }
        _ => Err(VfsError::NotFound(path.to_string())),
    }
}

fn svc_exists(integrations: &dyn IntegrationLister, path: &str) -> bool {
    svc_metadata(integrations, path).is_ok()
}

// ---------------------------------------------------------------------------
// VirtualFs impl
// ---------------------------------------------------------------------------

impl<V: VirtualFs> VirtualFs for ServiceFs<V> {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        if is_svc(path) {
            let integration = svc_tail(path)
                .and_then(|t| t.split('/').next())
                .unwrap_or("svc");

            let _span = info_span!(
                "simulacra_svcfs_read",
                "simulacra.svcfs.path" = path,
                "simulacra.svcfs.integration" = integration,
            )
            .entered();

            let result = svc_read(&*self.integrations, path);
            match &result {
                Ok(bytes) => {
                    debug!(
                        simulacra.svcfs.path = path,
                        simulacra.svcfs.value_len = bytes.len(),
                        "svcfs read"
                    );
                    svcfs_reads().add(1, &[KeyValue::new("integration", integration.to_string())]);
                }
                Err(e) => {
                    debug!(simulacra.svcfs.path = path, error = %e, "svcfs read error");
                }
            }
            return result;
        }
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        if is_svc(path) {
            let _ = data;
            warn!(
                simulacra.svcfs.path = path,
                "write attempt to read-only /svc/ path"
            );
            return Err(VfsError::PermissionDenied(format!("{path} is read-only")));
        }
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        if is_svc(path) {
            return svc_exists(&*self.integrations, path);
        }
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        if is_svc(path) {
            let _span =
                info_span!("simulacra_svcfs_list_dir", "simulacra.svcfs.path" = path).entered();
            return svc_list_dir(&*self.integrations, path);
        }
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        if is_svc(path) {
            warn!(
                simulacra.svcfs.path = path,
                "mkdir attempt to read-only /svc/ path"
            );
            return Err(VfsError::PermissionDenied(format!("{path} is read-only")));
        }
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        if is_svc(path) {
            warn!(
                simulacra.svcfs.path = path,
                "remove attempt to read-only /svc/ path"
            );
            return Err(VfsError::PermissionDenied(format!("{path} is read-only")));
        }
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        if is_svc(path) {
            return svc_metadata(&*self.integrations, path);
        }
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}
