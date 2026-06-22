# R012 — Embedder thread isolation (simulacra-memory)

## Rule

`Embedder::embed` implementations MUST tolerate being invoked from a detached
`std::thread::spawn` worker that is not owned by the tokio runtime they were
created against. In particular:

1. **Embedders must not assume re-entrance is exclusive.** A wedged embed
   call on a leaked thread may still be running while a subsequent embed
   call starts on the same `Arc<dyn Embedder>`. Any internal mutable
   state must be `Sync` and safe against concurrent calls.

2. **Embedders must not panic on a dropped result channel.** When the
   `BackgroundEmbedder` shutdown path hits the drain timeout, the detached
   thread running `embed` is left alive — its `oneshot` reply channel is
   dropped on the tokio side, so a late `Send::send` errors silently.
   Embedders that rely on side-effecting the reply must handle the error.

3. **Embedders must not call back into the owning `BackgroundEmbedder`**
   (there's no such handle in scope, but the invariant is worth naming).

## Why

`BackgroundEmbedder` dispatches each `handle_put` call to a detached
`std::thread::spawn(...)` named `simulacra-memory-embed`, not to
`tokio::task::spawn_blocking`. This choice is load-bearing for the
`S038` shutdown semantics and was validated against every shutdown
test in `crates/simulacra-memory/tests/s038_ensure_tenant_and_shutdown.rs`.

**Why not `spawn_blocking`.** `tokio::task::spawn_blocking` hands work to
the runtime's blocking pool. The `S038` shutdown tests include a
`ShutdownTimeout` path that plants an embedder which blocks on a
`tokio::sync::Notify` and is never released. A wedged `spawn_blocking`
task holds a `BlockingPool::Sender` clone; `Drop for BlockingPool` waits
indefinitely for every blocking task to exit. A wedged embedder would
therefore hang `Runtime::drop` and, transitively, the whole
`#[tokio::test]` function — the timeout test would never return and CI
would stall for hours.

**Why `std::thread::spawn`.** A plain OS thread is outside tokio's
bookkeeping. A wedged embedder leaks exactly one thread until process
exit; tokio can tear down its runtime cleanly, `shutdown(self)` can
return `MemoryError::ShutdownTimeout`, and the rest of the process
proceeds normally. This was verified: before this change the shutdown
timeout test hung >13 minutes; after the change it returns in ≤31s.

The tradeoff is that on the shutdown timeout path we deliberately leak
one OS thread per wedged embed call. For the CLI's single-process-per-run
model this is tolerable: the process exits shortly after `shutdown` and
the OS reaps the thread. For long-running servers, the invariant is that
`Embedder::embed` must either (a) complete in bounded time, or (b)
respond to cancellation via the embedder's own internal mechanism. A
future `Embedder::embed_async(&mut cancel: impl CancelToken)` API would
let us drop the detached-thread workaround entirely; until then, this
rule documents the invariant so future implementers don't regress to
`spawn_blocking` without understanding the consequences.

## Enforcement

- `crates/simulacra-memory/src/background.rs` `handle_put` is the only call
  site. Any new path that runs `Embedder::embed` from a tokio task is a
  regression.
- The `ScriptedEmbedder` test fake in
  `crates/simulacra-memory/tests/s038_ensure_tenant_and_shutdown.rs` uses
  `tokio::task::block_in_place` inside the `std::thread` body, which
  requires the thread to be entered into the owning runtime's handle —
  this works because the detached thread clones a `Handle` before
  blocking.
- Code review: any PR that touches `BackgroundEmbedder` worker dispatch
  must be reviewed against this rule.

## Related

- `specs/S037-memory-and-semantic-retrieval.md` §8 — background embedder
  queue semantics.
- `specs/S038-cli-memory-wiring.md` §`BackgroundEmbedder::shutdown design`
  — shutdown contract.
- `crates/simulacra-memory/src/background.rs` — the detached-thread
  implementation + inline comment at `handle_put`.
