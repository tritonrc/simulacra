# simulacra-quickjs

Minimal QuickJS sandbox runtime backed by `VirtualFs`.

## Ownership

- `JsRuntime` owns a `rquickjs::Runtime` + `Context` and an `Arc<dyn VirtualFs>`.
- Host functions (`console.log`, `fs.readFileSync`, `fs.writeFileSync`) are re-registered on each `eval` call so the stdout capture buffer is fresh.

## Key constraints

- **No direct I/O.** All filesystem access goes through the `VirtualFs` trait. The sandbox never touches the real filesystem.
- **Sync only.** `eval` is synchronous. The QuickJS runtime is not shared across threads.
- **Error propagation.** JS exceptions are caught via `CatchResultExt::catch` and surfaced as `JsError::Execution` with the exception message.

## Testing

```bash
cargo test -p simulacra-quickjs
cargo clippy -p simulacra-quickjs --all-targets -- -D warnings
```

Uses `simulacra_vfs::MemoryFs` as the VFS backend in all tests.
