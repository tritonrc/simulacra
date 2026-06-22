//! [`HookedVfsLayer`] — a [`VirtualFs`] decorator that runs every successful
//! `write` and `remove` through the global S026 [`HookPipeline`] under the
//! `Operation::VfsWrite` chain (S039).
//!
//! The layer:
//!   1. Builds a JSON context `{ tenant, path, op, bytes_len }` per the v1
//!      schema. `op` is `"write"` or `"remove"` so a governance hook can
//!      distinguish a zero-byte write from a remove.
//!   2. Runs the `Before` chain.
//!   3. Honors the verdict:
//!      - `Continue` → forward to inner (with optional path mutation).
//!      - `Deny`     → return `VfsError::HookDenied { reason }` without
//!        touching inner.
//!      - `Kill`     → return `VfsError::HookKilled { reason }` without
//!        touching inner.
//!   4. Emits a `vfs_write_hook` span as a child of the calling span with
//!      attributes `simulacra.vfs.{tenant,path,bytes_len,hook_outcome}` for o11y
//!      validation per S010 / R010.
//!
//! Bytes are NOT exposed to or mutable by the hook chain in v1; only `path`
//! mutations on `Continue` are honored. Modifying `tenant` or `op` on
//! `Continue` is a contract violation.

use std::sync::Arc;

use simulacra_hooks::{HookError, HookPipeline, Operation, Verdict};
use simulacra_types::{FsMetadata, TenantId, VfsError, VfsSnapshot, VfsWatcher, VirtualFs};

/// A `VirtualFs` decorator that runs every `write` and `remove` through the
/// global `Operation::VfsWrite` hook chain before forwarding to the inner FS.
pub struct HookedVfsLayer {
    inner: Arc<dyn VirtualFs>,
    hooks: Arc<HookPipeline>,
    tenant: TenantId,
}

impl HookedVfsLayer {
    /// Construct a layer bound to `tenant` that wraps `inner` and runs `hooks`
    /// on every `write` / `remove`. The tenant is required at construction
    /// (no default); events fired through the chain carry it, and the
    /// `simulacra.vfs.tenant` span attribute reflects it.
    pub fn new(tenant: TenantId, inner: Arc<dyn VirtualFs>, hooks: Arc<HookPipeline>) -> Self {
        Self {
            inner,
            hooks,
            tenant,
        }
    }

    /// Run the `Operation::VfsWrite` hook chain for a write/remove and apply
    /// the verdict. Returns the (possibly mutated) path the caller should pass
    /// to the inner FS. On `Deny` / `Kill` returns the appropriate `VfsError`.
    ///
    /// `op_kind` is `"write"` or `"remove"` and is exposed verbatim in the
    /// hook context as the `op` field. Hooks may not mutate it.
    fn run_vfs_write_chain(
        &self,
        path: &str,
        op_kind: &'static str,
        bytes_len: u64,
    ) -> Result<String, VfsError> {
        // Build v1 context: {tenant, path, op, bytes_len}. `bytes_len` is u64
        // but JSON numbers are doubles — `bytes_len` values that fit in
        // f64::MAX_SAFE_INTEGER (2^53) round-trip exactly, which covers any
        // realistic byte count. `op` distinguishes write from remove so a
        // governance hook can tell the two apart even when `bytes_len == 0`.
        let context = serde_json::json!({
            "tenant": self.tenant.as_str(),
            "path": path,
            "op": op_kind,
            "bytes_len": bytes_len,
        })
        .to_string();

        // Emit the `vfs_write_hook` span as a child of the calling span.
        // Attributes are filled in once we know the outcome.
        let span = tracing::info_span!(
            "vfs_write_hook",
            "simulacra.vfs.tenant" = %self.tenant,
            "simulacra.vfs.path" = path,
            "simulacra.vfs.bytes_len" = bytes_len,
            "simulacra.vfs.hook_outcome" = tracing::field::Empty,
        );
        let _entered = span.enter();

        let result = self.hooks.run_before(Operation::VfsWrite, &context);

        match result {
            Ok((Verdict::Continue(_), final_ctx)) => {
                // The pipeline threads each hook's `Continue(Some(modified))`
                // payload into `final_ctx`. Compare the returned context to
                // the original we built to detect path mutations and
                // contract violations on `tenant` / `op`.
                match modified_ctx_path_if_changed(
                    &context,
                    Some(final_ctx.as_str()),
                    &self.tenant,
                    op_kind,
                ) {
                    Ok(Some(new_path)) => {
                        // `Verdict::Continue` with a mutated `path`.
                        span.record("simulacra.vfs.hook_outcome", "mutate");
                        Ok(new_path)
                    }
                    Ok(None) => {
                        // `Verdict::Continue` with no path change.
                        span.record("simulacra.vfs.hook_outcome", "allow");
                        Ok(path.to_string())
                    }
                    Err(err) => {
                        // Cross-tenant or op mutation rejected as
                        // `VfsError::HookContractViolation`.
                        span.record("simulacra.vfs.hook_outcome", "violation");
                        Err(err)
                    }
                }
            }
            Ok((Verdict::Deny(reason), _)) => {
                span.record("simulacra.vfs.hook_outcome", "deny");
                Err(VfsError::HookDenied { reason })
            }
            Ok((Verdict::Kill(reason), _)) => {
                span.record("simulacra.vfs.hook_outcome", "kill");
                Err(VfsError::HookKilled { reason })
            }
            Err(HookError::Killed { reason, .. }) => {
                span.record("simulacra.vfs.hook_outcome", "kill");
                Err(VfsError::HookKilled { reason })
            }
            Err(other) => {
                // Other hook chain errors (e.g., serde failure, hook execution
                // error) fail closed as a deny so writes never bypass
                // governance. Tag as "error" — `violation` is reserved for the
                // specific case where a hook returns `Continue` with a context
                // that mutates an immutable field (`tenant` or `op`).
                span.record("simulacra.vfs.hook_outcome", "error");
                Err(VfsError::HookDenied {
                    reason: format!("hook error: {other}"),
                })
            }
        }
    }
}

/// Inspect a hook's `Continue(Some(modified_ctx))` payload against the
/// original context. Enforces the v1 mutation surface:
///   - `tenant` is immutable. Returns `Err(HookContractViolation)` if it
///     differs from the original tenant.
///   - `op` is immutable. Returns `Err(HookContractViolation)` if a hook
///     swaps a `write` for a `remove` (or vice versa).
///   - `path` may be mutated. Returns `Ok(Some(new_path))` when the path
///     differs from the original; otherwise `Ok(None)`.
///   - `bytes_len` modifications are silently ignored.
///
/// When `modified_ctx` is `None` (the hook did not mutate the context),
/// returns `Ok(None)`.
fn modified_ctx_path_if_changed(
    original_ctx: &str,
    modified_ctx: Option<&str>,
    expected_tenant: &TenantId,
    expected_op: &str,
) -> Result<Option<String>, VfsError> {
    let Some(modified) = modified_ctx else {
        return Ok(None);
    };
    let original_value: serde_json::Value =
        serde_json::from_str(original_ctx).map_err(|_| VfsError::HookContractViolation)?;
    let modified_value: serde_json::Value =
        serde_json::from_str(modified).map_err(|_| VfsError::HookContractViolation)?;

    let new_tenant = modified_value
        .get("tenant")
        .and_then(serde_json::Value::as_str);
    if let Some(t) = new_tenant
        && t != expected_tenant.as_str()
    {
        return Err(VfsError::HookContractViolation);
    }

    // `op` is immutable. If the hook drops the field entirely, accept that
    // (the original `op` we built is still authoritative for the layer's
    // routing). If the hook returns a different `op`, reject.
    let new_op = modified_value.get("op").and_then(serde_json::Value::as_str);
    if let Some(op) = new_op
        && op != expected_op
    {
        return Err(VfsError::HookContractViolation);
    }

    let original_path = original_value
        .get("path")
        .and_then(serde_json::Value::as_str);
    let new_path = modified_value
        .get("path")
        .and_then(serde_json::Value::as_str);

    match (original_path, new_path) {
        (Some(orig), Some(new)) if orig != new => Ok(Some(new.to_string())),
        _ => Ok(None),
    }
}

impl VirtualFs for HookedVfsLayer {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let effective_path = self.run_vfs_write_chain(path, "write", data.len() as u64)?;
        self.inner.write(&effective_path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        let effective_path = self.run_vfs_write_chain(path, "remove", 0)?;
        self.inner.remove(&effective_path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }

    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        self.inner.subscribe(prefix)
    }
}
