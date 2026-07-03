# simulacra-quickjs

Minimal QuickJS sandbox runtime backed by `VirtualFs`.

## Ownership

- `JsRuntime` owns host configuration, runtime limits, and remote module source caches.
- Each eval creates a fresh `rquickjs::AsyncRuntime` + `AsyncContext`; JS globals, prototypes, and module instances are not shared across eval calls.
- Host functions (`console.log`, `fs.readFileSync`, `fs.writeFileSync`) are re-registered on each eval call so the stdout capture buffer is fresh.

## Key constraints

- **No direct I/O.** All filesystem access goes through the `VirtualFs` trait. The sandbox never touches the real filesystem.
- **Async substrate.** `eval_async` is the primary evaluator. `eval` is a synchronous compatibility wrapper around the async substrate.
- **Error propagation.** JS exceptions are caught via `CatchResultExt::catch` and surfaced as `JsError::Execution` with the exception message.

## Testing

```bash
cargo test -p simulacra-quickjs
cargo clippy -p simulacra-quickjs --all-targets -- -D warnings
```

Uses `simulacra_vfs::MemoryFs` as the VFS backend in all tests.
