# S001 — Virtual Filesystem

**Status:** Active
**Crate:** `simulacra-vfs`

## Related Specs

- **S020** — VFS Host Mounts (how host filesystem paths are copied into the VFS before agent execution)

## Behavior

1. All paths are rooted at `/`. There is no escape from the virtual root.
2. Path traversal (`..`) that would escape `/` must resolve to `/`. E.g. `/../../../etc/passwd` → `/etc/passwd` (inside VFS, not real FS).
3. `write(path, data)` creates parent directories implicitly (mkdir -p semantics).
4. `list_dir(path)` returns entries sorted by name (BTreeMap backing guarantees this).
5. `snapshot()` returns a serializable clone of all VFS state. Must be cheap — it's called at checkpoint boundaries.
6. `restore(snapshot)` replaces the entire VFS state with the snapshot. After restore, the VFS is byte-for-byte identical to when the snapshot was taken.
7. `OverlayFs` combines a read-only lower layer with a read-write upper layer. Reads check upper first, then lower. Writes always go to upper. Deletes in upper shadow lower entries.

## Assertions (must be tested)

- [x] Write then read roundtrip returns identical bytes.
- [x] `../` at root resolves to root.
- [x] `snapshot()` then `restore()` is a no-op (state unchanged).
- [x] OverlayFs: write to upper does not mutate lower.
- [x] OverlayFs: read falls through to lower when upper has no entry.
- [x] OverlayFs: delete in upper shadows lower entry (read returns not-found).
- [x] `list_dir` on non-existent path returns error, not empty list.
- [x] `metadata()` returns correct size after write.
- [x] `remove()` on non-existent path returns error. **Note: tested implicitly via overlay tests but no dedicated MemoryFs test.**
- [x] `write()` to deeply nested path creates intermediate directories (mkdir -p). **Note: test exists (`write_creates_parent_directories_implicitly`) but not listed as a spec assertion — adding for completeness.**
- [x] OverlayFs `snapshot()` then `restore()` preserves whiteout state. **Tested in `overlay_snapshot_then_restore_preserves_whiteout_state`.**
- [x] VFS operations are thread-safe (concurrent reads and writes do not corrupt state). **Tested in `concurrent_reads_and_writes_do_not_corrupt_state`.**

## Observability (see S010 for conventions)

- [x] `write()` produces a span with `simulacra.operation.name` = `vfs_write` and `simulacra.vfs.path`.
- [x] `read()` produces a span with `simulacra.operation.name` = `vfs_read` and `simulacra.vfs.path`.
- [x] `snapshot()` and `restore()` produce spans with `simulacra.operation.name` = `vfs_snapshot` / `vfs_restore`.
