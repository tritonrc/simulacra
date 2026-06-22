//! Concrete `Embedder` implementations.
//!
//! For S037 Phase 2 MVP we ship a deterministic pure-Rust embedder that
//! produces unit-normalized 384-dimensional vectors. It is **not** a real
//! semantic model — it satisfies the trait contract (deterministic, fixed
//! dim, unit-normalized) so the rest of the memory subsystem has something
//! to run against. Real embedding quality comes from a follow-up that
//! plugs in sentence-transformers via ONNX or an Ollama endpoint.

use crate::embedder::{Embedder, EmbedderId};
use crate::error::MemoryError;

/// MVP dimensionality. Matches `all-MiniLM-L6-v2` so a future swap to a
/// real model with the same dim would not require a re-index based on
/// dimension alone (the model id will still differ, which forces a wipe
/// via the embedder-fingerprint check — that's intentional).
const DEFAULT_DIM: usize = 384;
const DEFAULT_MODEL: &str = "simulacra-hash-mvp";
const DEFAULT_VERSION: &str = "1.0";

/// Deterministic pure-Rust embedder for the S037 MVP.
///
/// Produces fixed-dimension unit vectors by hashing whitespace tokens into
/// a sparse signed sketch and L2-normalizing. Stable across process
/// restarts: the same input always produces the same output, and the
/// `EmbedderId` is constant.
///
/// This is NOT a real semantic model. It exists so the storage + index +
/// search pipeline can be exercised end-to-end without requiring an
/// external model download at test time.
pub struct DefaultEmbedder {
    id: EmbedderId,
    dim: usize,
}

impl DefaultEmbedder {
    /// Construct the default local-first embedder. Infallible today; the
    /// `Result` exists so a future ONNX/Ollama backend can fail to load
    /// without a breaking change.
    pub fn load_default() -> Result<Self, MemoryError> {
        Ok(Self {
            id: EmbedderId::new(DEFAULT_MODEL, DEFAULT_VERSION, DEFAULT_DIM),
            dim: DEFAULT_DIM,
        })
    }

    fn embed_one(&self, chunk: &str) -> Vec<f32> {
        let mut raw = vec![0.0f32; self.dim];

        for token in chunk.split_whitespace() {
            let h = fnv1a_64(token.as_bytes());

            // Two independent index/sign pairs per token, derived from the
            // upper and lower halves of the hash. This is a tiny
            // feature-hashing sketch (a la sklearn HashingVectorizer).
            let idx_a = (h as usize) % self.dim;
            let sign_a = if (h >> 63) & 1 == 1 { -1.0 } else { 1.0 };

            let h2 = h.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17);
            let idx_b = (h2 as usize) % self.dim;
            let sign_b = if (h2 >> 63) & 1 == 1 { -1.0 } else { 1.0 };

            raw[idx_a] += sign_a;
            raw[idx_b] += sign_b;
        }

        let norm = raw.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 1e-10 {
            for v in &mut raw {
                *v /= norm;
            }
        } else {
            // Empty (or fully-cancelling) chunk: return a deterministic
            // unit vector so the trait contract still holds. Index 0 = 1
            // is the simplest stable choice.
            raw.iter_mut().for_each(|v| *v = 0.0);
            raw[0] = 1.0;
        }

        // Note: the trait-level `embed` in the Embedder impl below re-checks
        // unit norm in release builds and returns `NotUnitVector` on
        // violation, so we don't need a debug_assert here too.
        raw
    }
}

impl Embedder for DefaultEmbedder {
    fn id(&self) -> &EmbedderId {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let mut out = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let v = self.embed_one(chunk);
            if v.len() != self.dim {
                return Err(MemoryError::VectorDimMismatch {
                    expected: self.dim,
                    got: v.len(),
                });
            }
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if (norm - 1.0).abs() > 1e-5 {
                return Err(MemoryError::NotUnitVector(norm));
            }
            out.push(v);
        }
        Ok(out)
    }
}

/// FNV-1a 64-bit. Stable, dependency-free, fast enough for this purpose.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
