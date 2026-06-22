# simulacra-vfs

In-memory and overlay virtual filesystem implementations for the Simulacra sandbox.

## What it does

- **`MemoryFs`** — BTreeMap-backed VFS with interior mutability (`RwLock`).
  Write creates parent dirs implicitly (mkdir -p). Paths rooted at `/`;
  `..` that escapes root resolves to root.
- **`OverlayFs`** — Copy-on-write layer: read-only lower + read-write upper.
  Deletes shadow the lower layer via a whiteout set. Snapshot/restore
  only touches the upper layer.

## Dependencies

- `simulacra-types` — `VirtualFs` trait, `VfsError`, `FsMetadata`, `VfsSnapshot`
- `serde`, `serde_json` — snapshot serialisation
- `thiserror` — (transitive via simulacra-types)

## Testing

```bash
cargo test -p simulacra-vfs
cargo clippy -p simulacra-vfs -- -D warnings
cargo fmt -p simulacra-vfs -- --check
```

All tests use `&dyn VirtualFs` to verify behaviour through the trait, not concrete types.
