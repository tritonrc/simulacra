//! S037 §13: startup policy dispatch for embedder mismatch.
//!
//! Called by the CLI bootstrap after reading `MemoryConfig.on_model_change`.
//! Inspects the stored embedder fingerprint against the configured one and
//! applies the policy:
//!
//! - `Refuse`: surface `EmbedderMismatch` / `EmbedderDimensionMismatch`.
//! - `ReindexBackground` (same-dim only): clear `memory_vectors` via
//!   `mark_tenant_stale`, populate `memory_embed_backlog` with existing
//!   `(path, version)` rows from `memory_chunks`, update
//!   `memory_schema_meta` to the new embedder, append an audit row to
//!   `memory_embedder_log`. Different-dim under this policy errors with
//!   `EmbedderDimensionMismatch`.
//! - `WipeAndRebuild` (any dim): drop+recreate `memory_vectors` +
//!   `memory_chunks`, seed the backlog from `memory_content`, update
//!   `memory_schema_meta` to the new embedder, append an audit row —
//!   all atomic via [`SqliteVectorIndex::wipe_and_reopen`].
//!
//! After a successful dispatch, the caller constructs a reconciled
//! [`SqliteVectorIndex`] with the new embedder — the fingerprint check
//! now passes — and the normal bootstrap continues. The background
//! embedder will drain the staged backlog asynchronously.

use std::path::Path;

use simulacra_types::TenantId;

use crate::embedder::EmbedderId;
use crate::error::MemoryError;
use crate::index::VectorIndex;
use crate::sqlite_index::SqliteVectorIndex;

/// Policy variant matching `simulacra_config::OnModelChange`. Redeclared
/// here so `simulacra-memory` does not depend on `simulacra-config`; the CLI
/// maps between them at the top of the bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnModelChangePolicy {
    Refuse,
    ReindexBackground,
    WipeAndRebuild,
}

/// S037 §13 startup dispatch. Call BEFORE constructing the reconciled
/// `SqliteVectorIndex` for the new embedder.
pub fn apply_policy(
    root: &Path,
    tenant: &TenantId,
    configured_id: &EmbedderId,
    policy: OnModelChangePolicy,
) -> Result<(), MemoryError> {
    let configured_dim = configured_id.dim().ok_or_else(|| {
        MemoryError::Internal(format!(
            "configured embedder has no parseable dim: {}",
            configured_id.as_str()
        ))
    })?;

    let stored = SqliteVectorIndex::read_fingerprint_at(root, tenant)?;
    let Some((stored_id, stored_dim)) = stored else {
        // Fresh tenant — nothing to reconcile. Constructor will seed.
        return Ok(());
    };

    let same_dim = stored_dim == configured_dim;
    let same_id = stored_id.as_str() == configured_id.as_str();
    if same_id && same_dim {
        return Ok(());
    }

    match policy {
        OnModelChangePolicy::Refuse => {
            if !same_dim {
                Err(MemoryError::EmbedderDimensionMismatch {
                    stored: stored_dim,
                    configured: configured_dim,
                })
            } else {
                Err(MemoryError::EmbedderMismatch {
                    stored: stored_id.as_str().to_string(),
                    configured: configured_id.as_str().to_string(),
                    requires_wipe: false,
                })
            }
        }
        OnModelChangePolicy::ReindexBackground => {
            if !same_dim {
                return Err(MemoryError::EmbedderDimensionMismatch {
                    stored: stored_dim,
                    configured: configured_dim,
                });
            }
            // Open with the OLD embedder, stage re-embed work, drop
            // the handle, then flip the meta. Cross-operation
            // atomicity is achieved by: same-dim lock is trivial
            // (only one process per tenant by design), and the backlog
            // plus meta updates are each individually atomic. A crash
            // between mark_tenant_stale and set_embedder_id_at leaves
            // the tenant with vectors cleared and old fingerprint —
            // the next startup with the same configured embedder sees
            // the same mismatch and re-runs the policy, converging.
            let old_index = SqliteVectorIndex::new(root, stored_id.clone())?;
            let cleared = old_index.mark_tenant_stale(tenant)?;
            old_index.enqueue_backlog_from_chunks(tenant)?;
            drop(old_index);
            SqliteVectorIndex::set_embedder_id_at(root, tenant, configured_id)?;
            SqliteVectorIndex::append_embedder_log_at(
                root,
                tenant,
                configured_id,
                cleared,
                "reindex",
            )?;
            Ok(())
        }
        OnModelChangePolicy::WipeAndRebuild => {
            // wipe_and_reopen drops + recreates tables at the new dim,
            // seeds the backlog from memory_content, and writes the
            // audit log row atomically.
            let _cleared = SqliteVectorIndex::wipe_and_reopen(root, tenant, configured_id.clone())?;
            Ok(())
        }
    }
}
