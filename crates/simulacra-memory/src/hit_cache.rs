//! `HitIdCache` — per-process cache of `HitId → (tenant, path, chunk_index,
//! version, expires_at)` used by `semantic_search` to mint tokens and by
//! `memory_read_chunk` to resolve them.
//!
//! Per S037 §9:
//! - 24-byte CSPRNG tokens, base32-encoded (192 bits of entropy)
//! - 5-minute TTL
//! - Process-wide bounded size ([`HIT_ID_CACHE_MAX`](simulacra_types::HIT_ID_CACHE_MAX))
//! - Oldest-first eviction on capacity pressure
//! - Periodic sweeper task removes expired entries

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use simulacra_types::{
    HIT_ID_CACHE_MAX, HIT_ID_TTL_SECONDS, HitId, MemoryPath, MemoryVersion, TenantId,
};

/// What gets stored behind a `HitId`. The `memory_read_chunk` tool uses this
/// to resolve the hit to a concrete `(tenant, path, chunk_index, version)`
/// and then runs the TOCTOU check against the current store version.
#[derive(Debug, Clone)]
pub struct HitCacheEntry {
    pub tenant: TenantId,
    pub path: MemoryPath,
    pub chunk_index: usize,
    pub version: MemoryVersion,
    pub expires_at: Instant,
}

/// Process-wide hit id cache. Bounded by [`HIT_ID_CACHE_MAX`].
pub struct HitIdCache {
    inner: Mutex<HitIdCacheInner>,
}

struct HitIdCacheInner {
    // HitId → entry
    entries: HashMap<HitId, HitCacheEntry>,
    // Insertion order for LRU-ish eviction. `Instant` tie-breaks across equal insertions.
    insertion_order: BTreeMap<(Instant, u64), HitId>,
    // Monotonic counter for insertion ordering (handles same-Instant collisions).
    seq: u64,
}

impl Default for HitIdCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HitIdCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HitIdCacheInner {
                entries: HashMap::new(),
                insertion_order: BTreeMap::new(),
                seq: 0,
            }),
        }
    }

    /// Insert a new cache entry and return the minted `HitId`. The token is
    /// a 24-byte CSPRNG value base32-encoded (192 bits of entropy).
    pub fn mint(
        &self,
        tenant: TenantId,
        path: MemoryPath,
        chunk_index: usize,
        version: MemoryVersion,
    ) -> HitId {
        let now = Instant::now();
        let expires_at = now + Duration::from_secs(HIT_ID_TTL_SECONDS);
        let token = generate_token();
        let hit_id = HitId(token);

        let entry = HitCacheEntry {
            tenant,
            path,
            chunk_index,
            version,
            expires_at,
        };

        let mut inner = self.inner.lock().expect("hit id cache poisoned");
        inner.seq += 1;
        let seq = inner.seq;
        // Evict expired entries opportunistically.
        evict_expired(&mut inner, now);
        // Evict oldest if at capacity.
        while inner.entries.len() >= HIT_ID_CACHE_MAX
            && let Some((&key, victim)) = inner.insertion_order.iter().next()
        {
            let victim = victim.clone();
            inner.insertion_order.remove(&key);
            inner.entries.remove(&victim);
        }
        inner.insertion_order.insert((now, seq), hit_id.clone());
        inner.entries.insert(hit_id.clone(), entry);
        hit_id
    }

    /// Look up a hit id. Returns `None` if missing OR expired (the caller
    /// treats both as 404 per §9).
    pub fn get(&self, hit_id: &HitId) -> Option<HitCacheEntry> {
        let mut inner = self.inner.lock().expect("hit id cache poisoned");
        let now = Instant::now();
        evict_expired(&mut inner, now);
        inner.entries.get(hit_id).cloned()
    }

    /// Current entry count (including not-yet-swept-expired).
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("hit id cache poisoned")
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Explicitly sweep expired entries. Called periodically by the
    /// caller's sweeper task (every 60s per §9). Tests can call this
    /// directly to exercise the eviction path.
    pub fn sweep_expired(&self) {
        let mut inner = self.inner.lock().expect("hit id cache poisoned");
        evict_expired(&mut inner, Instant::now());
    }
}

fn evict_expired(inner: &mut HitIdCacheInner, now: Instant) {
    let expired: Vec<HitId> = inner
        .entries
        .iter()
        .filter(|(_, entry)| entry.expires_at <= now)
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        inner.entries.remove(&id);
    }
    inner
        .insertion_order
        .retain(|_, hit_id| inner.entries.contains_key(hit_id));
}

/// Generate a 24-byte CSPRNG token, base32-encoded.
///
/// Per S037 §9: "24 bytes of CSPRNG-generated randomness, base32-encoded
/// (192 bits of entropy). Unguessable." We use `rand::rngs::OsRng` which
/// draws from the operating system's CSPRNG on every call, and
/// `data_encoding::BASE32_NOPAD` for the encoding. The resulting string
/// is 39 characters (24 bytes × 8 bits / 5 bits per base32 char ≈ 38.4,
/// padded up by the encoding).
fn generate_token() -> String {
    use rand::TryRngCore;
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS CSPRNG failed");
    let encoded = data_encoding::BASE32_NOPAD.encode(&bytes);
    format!("hit_{encoded}")
}
