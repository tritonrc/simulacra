use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::thread;

use simulacra_memory::HitIdCache;
use simulacra_types::{HitId, MemoryPath, MemoryVersion, TenantId};

fn tenant(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

#[test]
fn mint_returns_unique_ids_for_many_entries() {
    let cache = HitIdCache::new();
    let mut ids = HashSet::new();

    for index in 0..100 {
        let hit_id = cache.mint(
            tenant("tenant-a"),
            memory_path(&format!("/var/memory/self/{index}.md")),
            index,
            MemoryVersion(index as u64 + 1),
        );
        assert!(ids.insert(hit_id));
    }

    assert_eq!(ids.len(), 100);
}

#[test]
fn get_returns_the_cached_entry_for_a_valid_hit_id() {
    let cache = HitIdCache::new();
    let expected_tenant = tenant("tenant-a");
    let expected_path = memory_path("/var/memory/self/note.md");
    let hit_id = cache.mint(
        expected_tenant.clone(),
        expected_path.clone(),
        3,
        MemoryVersion(7),
    );

    let entry = cache.get(&hit_id).unwrap();

    assert_eq!(entry.tenant, expected_tenant);
    assert_eq!(entry.path, expected_path);
    assert_eq!(entry.chunk_index, 3);
    assert_eq!(entry.version, MemoryVersion(7));
}

#[test]
fn get_returns_none_for_an_unknown_hit_id() {
    let cache = HitIdCache::new();

    assert!(cache.get(&HitId("missing-hit".to_string())).is_none());
}

#[test]
#[ignore = "TTL expiry cannot be observed through the current public API without waiting five minutes or mutating private state"]
fn get_returns_none_after_ttl_expiry() {}

#[test]
#[ignore = "Capacity eviction cannot be asserted cleanly through the current public API without lowering the global limit"]
fn oldest_entries_are_evicted_at_capacity() {}

#[test]
fn sweep_expired_is_idempotent_for_the_same_live_state() {
    let cache = HitIdCache::new();
    let first = cache.mint(
        tenant("tenant-a"),
        memory_path("/var/memory/self/first.md"),
        0,
        MemoryVersion(1),
    );
    let second = cache.mint(
        tenant("tenant-a"),
        memory_path("/var/memory/self/second.md"),
        1,
        MemoryVersion(2),
    );

    cache.sweep_expired();
    let len_after_first_sweep = cache.len();
    cache.sweep_expired();

    assert_eq!(cache.len(), len_after_first_sweep);
    assert!(cache.get(&first).is_some());
    assert!(cache.get(&second).is_some());
}

#[test]
fn concurrent_mint_is_thread_safe_and_produces_distinct_ids() {
    let cache = Arc::new(HitIdCache::new());
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = Vec::new();

    for thread_index in 0..10 {
        let cache = cache.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            let mut ids = Vec::new();
            for index in 0..100 {
                ids.push(cache.mint(
                    tenant("tenant-a"),
                    memory_path(&format!("/var/memory/self/{thread_index}-{index}.md")),
                    index,
                    MemoryVersion(index as u64 + 1),
                ));
            }
            ids
        }));
    }

    let mut all_ids = HashSet::new();
    for handle in handles {
        for id in handle.join().unwrap() {
            assert!(all_ids.insert(id));
        }
    }

    assert_eq!(all_ids.len(), 1_000);
}
