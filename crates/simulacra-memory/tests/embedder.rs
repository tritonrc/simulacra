use simulacra_memory::{DefaultEmbedder, Embedder, EmbedderId, MemoryError};

#[derive(Debug, Clone)]
struct HashEmbedder {
    id: EmbedderId,
    dim: usize,
}

impl HashEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            id: EmbedderId::new("hash-test", "1.0", dim),
            dim,
        }
    }
}

impl Embedder for HashEmbedder {
    fn id(&self) -> &EmbedderId {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(chunks
            .iter()
            .map(|chunk| {
                let mut raw = Vec::with_capacity(self.dim);
                for index in 0..self.dim {
                    let mut hash = 0xcbf29ce484222325u64;
                    for byte in chunk.as_bytes() {
                        hash ^= u64::from(*byte);
                        hash = hash.wrapping_mul(0x100000001b3);
                    }
                    hash ^= index as u64;
                    hash = hash.wrapping_mul(0x100000001b3);
                    let centered = ((hash % 2001) as f32) - 1000.0;
                    raw.push(centered);
                }

                let norm = raw.iter().map(|value| value * value).sum::<f32>().sqrt();
                raw.into_iter()
                    .map(|value| value / norm)
                    .collect::<Vec<_>>()
            })
            .collect())
    }
}

fn l2_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}

#[test]
fn hash_embedder_returns_unit_normalized_vectors() {
    let embedder = HashEmbedder::new(8);
    let vectors = embedder.embed(&["alpha", "beta"]).unwrap();

    for vector in vectors {
        let norm = l2_norm(&vector);
        assert!(
            (norm - 1.0).abs() <= 1e-5,
            "expected a unit vector, got norm {norm}"
        );
    }
}

#[test]
fn hash_embedder_preserves_batch_input_order() {
    let embedder = HashEmbedder::new(8);
    let batched = embedder.embed(&["alpha", "beta"]).unwrap();
    let alpha = embedder.embed(&["alpha"]).unwrap().remove(0);
    let beta = embedder.embed(&["beta"]).unwrap().remove(0);

    assert_eq!(batched[0], alpha);
    assert_eq!(batched[1], beta);
}

#[test]
fn hash_embedder_returns_same_number_of_vectors_as_inputs() {
    let embedder = HashEmbedder::new(8);
    let inputs = ["alpha", "beta", "gamma"];
    let vectors = embedder.embed(&inputs).unwrap();

    assert_eq!(vectors.len(), inputs.len());
}

#[test]
fn hash_embedder_outputs_match_the_embedder_dimension() {
    let embedder = HashEmbedder::new(8);
    let vectors = embedder.embed(&["alpha", "beta"]).unwrap();

    assert!(vectors.iter().all(|vector| vector.len() == embedder.dim()));
}

#[test]
fn default_embedder_id_is_stable_across_loads() {
    let first = DefaultEmbedder::load_default().unwrap();
    let second = DefaultEmbedder::load_default().unwrap();

    assert_eq!(first.id().as_str(), second.id().as_str());
    assert!(!first.id().as_str().is_empty());
}

#[test]
fn default_embedder_dim_matches_the_identifier_dimension() {
    let embedder = DefaultEmbedder::load_default().unwrap();

    assert_eq!(embedder.id().dim(), Some(embedder.dim()));
}
