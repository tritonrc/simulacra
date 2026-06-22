//! `Embedder` trait — produces unit-normalized vectors for chunks.
//!
//! See S037 §3 (cosine score contract, unit vector invariant) and §13
//! (model identity).

use crate::error::MemoryError;

/// Stable identifier for an embedder model + version + dimension.
///
/// Format: `"{model_name}@{model_version}:{dim}"`
/// Example: `"all-MiniLM-L6-v2@1.0:384"`.
///
/// Two embedders that produce the same `EmbedderId` are interchangeable for
/// search purposes; two with different ids are not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmbedderId(pub String);

impl EmbedderId {
    pub fn new(model_name: &str, model_version: &str, dim: usize) -> Self {
        Self(format!("{model_name}@{model_version}:{dim}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse the dim out of the identifier. Returns `None` if malformed.
    pub fn dim(&self) -> Option<usize> {
        self.0.rsplit_once(':')?.1.parse().ok()
    }
}

impl std::fmt::Display for EmbedderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Produces vector embeddings for text chunks.
///
/// **Unit-vector invariant:** all implementations MUST return unit-normalized
/// vectors (L2 norm = 1 ± 1e-5). Providers that produce unnormalized vectors
/// normalize before returning. This is the only way `cosine_score` is
/// comparable across embedders.
///
/// See S037 §3.
pub trait Embedder: Send + Sync + 'static {
    /// Stable identifier for this embedder. Survives process restart.
    fn id(&self) -> &EmbedderId;

    /// Vector dimensionality. Must equal `id().dim().unwrap()`.
    fn dim(&self) -> usize;

    /// Embed a batch of chunks. The returned vectors:
    /// - MUST have length `chunks.len()` (preserve input order)
    /// - MUST each have length `self.dim()`
    /// - MUST be unit-normalized (L2 norm = 1 ± 1e-5)
    fn embed(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError>;
}
