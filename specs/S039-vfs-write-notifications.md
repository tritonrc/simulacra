# S039 — VFS Write Notifications

**Status:** Active
**Crates involved:** `simulacra-vfs`, `simulacra-runtime`, `simulacra-types`, `simulacra-memory`

## Dependencies

- **ARCHITECTURE.md** — VFS layering, capability model
- **S001** — Virtual filesystem (the trait being extended)
- **S011** — Sandbox + VFS Golden Rule
- **S026** — Hook pipeline (this spec adds `Operation::VfsWrite` as a sibling to the existing `ToolCall`/`Llm`/`Spawn`/`HttpRequest` operations)
- **S037** — Memory and semantic retrieval (existing example: `MemoryEvent::{Put, Delete}` broadcast on `/var/memory/**` / `/mnt/**` is the prior-art the general API generalizes)

## Scope

A subscription API on `VirtualFs` so interested consumers — governance hooks, background indexers, observability collectors, future WASM-backed VFS modules (S040) — get notified when paths under a pattern are written or deleted. Today only `MemoryStore` has a write-event channel (via `MemoryEvent`), and only for `/var/memory/**` / `/mnt/**`. This spec generalizes that pattern across the VFS surface.

This spec was originally bundled with WASM-backed VFS nodes; that capability is now **S040**. The two are still natural collaborators (a WASM VFS module is a consumer of write notifications), but each can ship independently.

**In scope:**

1. `VirtualFs::subscribe(prefix: &str) -> VfsWatcher` — a receiver of `VfsEvent::{Written, Removed, Skipped}` events for paths under `prefix`.
2. Default trait impl returning a dead-channel watcher, so existing `VirtualFs` impls compile and don't break consumers that subscribe.
3. `MemoryStoreFs::subscribe` adapts its existing `MemoryEvent` broadcast to the new `VfsEvent` shape. The existing in-process `MemoryEvent` channel remains internal to `simulacra-memory` (still used by `BackgroundEmbedder`); the adapter is one-way.
4. New `NotifyingFsLayer` that wraps any inner `VirtualFs` and emits events on its write paths — provides notifications for layers that don't broadcast internally.
5. `Operation::VfsWrite` added to the S026 hook pipeline so governance can deny or mutate a VFS write before it lands. This covers VFS writes from non-tool callers (journal persistence, session artifacts, runtime bookkeeping) that never traverse `tool_call`.
6. Bounded ring buffer with a `Skipped(n)` sentinel: a slow consumer drops events rather than backpressuring writers.
7. Cross-tenant isolation: a watcher acquired through tenant A's `Arc<dyn VirtualFs>` does not receive events for writes made through tenant B's stack, even when both stacks share underlying storage.

**Out of scope:**

- VFS read notifications — reads don't change state; the governance surface for reads is the existing capability check, not a hook.
- Cross-layer atomicity — a write that spans two VFS layers is not a transaction.
- Durable / cross-process notification fan-out — watchers are per-process; multi-process consumers should run the API server.
- WASM-backed VFS nodes — see S040.
- Identity-preserving rename events. Rename surfaces as `Removed(from) + Written(to)`. A `Renamed { from, to }` variant can be added later if a consumer needs it.

## Context

**Why now.** S037 (memory) shipped a single, narrow write-event channel. Several near-term capabilities need the *generalized* version:

- Background workers that derive from VFS state — S040 WASM modules, future indexers.
- Governance hooks that gate VFS writes from non-tool paths.
- Observability collectors that want a complete write log without source-instrumenting every `VirtualFs` impl.

**Why not extend tool hooks (S026).** S026 hooks intercept tool calls, not VFS operations. A VFS write from a non-tool path (journal persistence, session artifacts) does not reach `tool_call`, but it SHOULD reach a write watcher. Different surface, different invariants.

## Resolved design decisions

The four open questions from the Draft skeleton were resolved at activation (2026-04-25), aligned with S037's existing patterns to minimize surprise:

1. **Delivery semantics: best-effort with bounded ring + `Skipped(n)` sentinel.** Writers never block on slow consumers; slow consumers see `Skipped(n)` and resume. Matches S037's `MemoryEvent` channel.
2. **Hook chain shape: a single global `Operation::VfsWrite` chain.** Same as `ToolCall`/`Llm`/`Spawn`/`HttpRequest`. Hooks that care about a path range filter on path themselves. Per-prefix hook registration was rejected because S026's existing chains are global and the asymmetry would be load-bearing for nothing.
3. **Per-subscriber emission, not per-stack.** Each layer with its own broadcast (`NotifyingFsLayer`, `MemoryStoreFs`) emits on its own channel; a consumer subscribes to ONE layer and receives that layer's events. A stack like `NotifyingFsLayer(MemoryStoreFs(...))` supports two independent broadcasts (one per layer); a subscriber to either receives exactly one event per write through the stack. There is no global "stack-wide" event count — consumers pick their vantage point. Layers do NOT coordinate to suppress each other.
4. **Rename modeling: `Removed { from } + Written { to }`.** Two events, no identity preservation. Simpler API and matches what the current consumer set (memory ingestion, governance, observability) actually needs.
5. **Hook mutation surface (v1): path-only.** `Operation::VfsWrite` hooks may mutate `path` via `Verdict::Continue` with modified context; `tenant` and `bytes_len` are not mutable. Bytes are NOT exposed to or mutable by the hook chain — sidesteps base64-in-JSON and preserves zero-copy write paths. Future spec adds a bytes-mutation surface if a real consumer requires it.
6. **`NotifyingFsLayer` is per-tenant.** Constructor `for_tenant(tenant, inner)` requires the tenant. Events it publishes carry that tenant. Wrapping a multi-tenant inner requires one `NotifyingFsLayer` per tenant — explicit, no implicit tenant discovery.

## Design

- `VfsEvent` enum on `simulacra-vfs`:
  ```rust
  pub enum VfsEvent {
      Written { tenant: TenantId, path: PathBuf, len: u64 },
      Removed { tenant: TenantId, path: PathBuf },
      Skipped { count: u64 },
  }
  ```
- `VfsWatcher` newtype over a `tokio::sync::broadcast::Receiver<VfsEvent>` with a prefix filter applied on `recv`; non-matching events are silently consumed (the prefix filter is a convenience; `Skipped` events surface regardless of prefix).
- `VirtualFs::subscribe(&self, prefix: &str) -> VfsWatcher` — default impl returns a dead-channel watcher (a watcher whose underlying broadcast has zero senders, so `recv` returns `None` on first call). Layers that don't actively support subscription don't break consumers; they cleanly signal "no events from me."
- `MemoryStoreFs::subscribe` adapts `MemoryEvent::Put` → `VfsEvent::Written` and `MemoryEvent::Delete` → `VfsEvent::Removed`, filtering by the layer's bound `TenantId`.
- `NotifyingFsLayer` is constructed via:
  - `for_tenant(tenant: TenantId, inner: Arc<dyn VirtualFs>) -> NotifyingFsLayer` — default broadcast ring size (256).
  - `for_tenant_with_capacity(tenant: TenantId, inner: Arc<dyn VirtualFs>, cap: usize) -> NotifyingFsLayer` — explicit ring size.

  Bound to a single tenant at construction; events the layer publishes carry that tenant. There is no chainable `with_capacity` builder — capacity is set once at construction so subscribers cannot be silently orphaned by a later sender swap. On `write` / `remove` success, publishes a `VfsEvent`. `subscribe(prefix)` returns a watcher with prefix matching.
- Hook integration: a `HookedVfsLayer` wraps any inner `VirtualFs` plus a `Hooks` handle. On `write`/`remove` it runs `Operation::VfsWrite { tenant, path, op, bytes_len }` through the chain and honors `Verdict::{Continue, Deny, Kill}`. Operators install `HookedVfsLayer` at the position in the stack where governance should run. Ordering relative to `NotifyingFsLayer` matters: install `HookedVfsLayer` ABOVE `NotifyingFsLayer` if you want denied writes to suppress events; install it BELOW `NotifyingFsLayer` if you want to observe attempted writes regardless of governance.
- Config surface: `[hooks.vfs_write]` in `simulacra.toml` registers VfsWrite hooks alongside the existing `tool_call` / `llm` / `spawn` / `http_request` chains. The CLI bootstrap loads the section into the global `HookPipeline` exactly the same way as the other operations.

### Hook context schema for `Operation::VfsWrite`

The hook chain uses S026's existing JSON-context plumbing.

- **Input context** (provided to every hook):
  ```json
  { "tenant": "<TenantId>", "path": "<absolute VFS path>", "op": "write" | "remove", "bytes_len": <u64> }
  ```
  `op` distinguishes a `write` from a `remove`. Without it, governance hooks cannot tell the two apart when `bytes_len == 0` (a zero-byte write looks identical to a remove).
- **`Verdict::Continue` mutation surface (v1):** a hook may return modified context that changes `path`. Only `path` is honored. `tenant` is immutable — cross-tenant rerouting through hooks is rejected as a security pitfall; if a hook returns a context with a different `tenant`, the write fails with `VfsError::HookContractViolation`. `op` is also immutable — hooks cannot upgrade a `write` to a `remove` (or vice versa); attempting to mutate `op` produces `VfsError::HookContractViolation`. Modifications to `bytes_len` are silently ignored (informational, not a control field).
- **Bytes are NOT exposed to or mutable by the hook chain in v1.** Hooks see `bytes_len` and `path`, never the bytes themselves. A future spec can add a bytes-inspection or bytes-mutation surface when a consumer requires it.
- **`Verdict::Deny` payload:** `{ "reason": "<string>" }`. The resulting `VfsError::HookDenied { reason }` carries the string verbatim.
- **`Verdict::Kill`:** propagates as `VfsError::HookKilled { reason }` and terminates the calling agent's run; hooks should reserve `Kill` for catastrophic policy violations.

## Behavior

- `subscribe("/foo")` delivers ordered events for paths under `/foo`. "Under" is **segment-aware**: `/foo` matches itself and any path of the form `/foo/...`, but does NOT match unrelated paths like `/foobar`. Per-path ordering is preserved (writes to `/foo/a` in order A, B, C are observed in order A, B, C).
- Watcher dropped → broadcast sender continues; no leak, no slowdown for other watchers.
- Slow consumer falls behind the bounded ring → next `recv` yields `VfsEvent::Skipped { count }` followed by surviving events.
- Writer never blocks on a slow consumer; bounded broadcast drops oldest events on overflow, accounted for by `Skipped`.
- `Operation::VfsWrite` hook denial → write returns a structured error, inner VFS is NOT called, no `VfsEvent` is emitted.
- `Operation::VfsWrite` hook path-mutation → mutated path lands in inner VFS; emitted `VfsEvent` carries the mutated path. Bytes are immutable through the hook chain in v1.
- Tenant isolation: a watcher acquired through tenant A's `Arc<dyn VirtualFs>` never receives events for writes made through tenant B's `Arc<dyn VirtualFs>`, even if both stacks share the same physical storage.

## Assertions

### `simulacra-vfs` API additions

- [ ] `VfsEvent` enum exists in `simulacra-vfs` with variants `Written { tenant: TenantId, path: PathBuf, len: u64 }`, `Removed { tenant: TenantId, path: PathBuf }`, `Skipped { count: u64 }`.
- [ ] `VfsWatcher` exposes `async fn recv(&mut self) -> Option<VfsEvent>` returning `None` only when the underlying broadcast is permanently closed (zero senders).
- [ ] `VfsWatcher::recv` applies the prefix filter set at `subscribe` time: events whose path does not match the prefix are silently consumed and not surfaced; `Skipped` events surface regardless of prefix.
- [ ] `VirtualFs` trait gains `fn subscribe(&self, prefix: &str) -> VfsWatcher` with a default impl returning a dead-channel watcher.
- [ ] A unit test against `MemoryFs` (which uses the default impl) calls `subscribe("/")` and asserts `recv` resolves to `None` immediately (channel reports closed).

### `MemoryStoreFs` adapter

- [ ] `MemoryStoreFs::subscribe(prefix)` returns a watcher delivering `VfsEvent::Written` for every `MemoryEvent::Put` whose tenant matches the layer's bound tenant and whose path matches the prefix.
- [ ] `MemoryStoreFs::subscribe(prefix)` delivers `VfsEvent::Removed` for every matching `MemoryEvent::Delete`.
- [ ] `MemoryStoreFs::subscribe` filters cross-tenant: a watcher acquired on tenant A's `MemoryStoreFs` does NOT receive events for writes made through tenant B's `MemoryStoreFs`, even when both share the same `Arc<dyn MemoryStore>`.
- [ ] The adapter does not break `BackgroundEmbedder`'s consumption of `MemoryEvent` — both run side by side, both see every write.
- [ ] A unit test writes to two tenants on a shared store, subscribes per-tenant, and asserts no cross-tenant leakage.

### `NotifyingFsLayer`

- [ ] `NotifyingFsLayer::for_tenant(tenant: TenantId, inner: Arc<dyn VirtualFs>) -> NotifyingFsLayer` constructs a layer bound to the given tenant with its own broadcast sender at the default capacity (256). `NotifyingFsLayer::for_tenant_with_capacity(tenant, inner, cap)` constructs the same with an explicit ring size. There is no chainable `with_capacity` builder — capacity is fixed at construction so subscribers wired between construction and use cannot be silently orphaned. The constructor takes `tenant` as a required parameter; events the layer publishes carry that tenant.
- [ ] On successful `write`, the layer publishes `VfsEvent::Written { tenant, path, len }` where `len` is the bytes written.
- [ ] On successful `remove`, the layer publishes `VfsEvent::Removed { tenant, path }`.
- [ ] On a failing `write` / `remove`, no event is published.
- [ ] `NotifyingFsLayer::subscribe(prefix)` returns a watcher applying the prefix filter on receive.
- [ ] A test that writes to `/foo/bar` and `/baz/qux` with a watcher on `/foo` receives only the `/foo/bar` event.
- [ ] A test that drops a watcher mid-stream and continues writing on the layer succeeds: no panic, no resource leak; the sender stays alive for other subscribers.
- [ ] A test that subscribes a watcher, writes more events than the ring capacity without consuming, then consumes, observes a `VfsEvent::Skipped { count }` followed by the surviving events.
- [ ] Writers do not block when the broadcast ring is full: a writer issuing 1000 sequential writes against a layer with a single consumer that sleeps 100ms between recvs completes in under 1 second of writer wall-clock.

### `Operation::VfsWrite` hook

- [ ] `simulacra-runtime::hooks::Operation` gains a `VfsWrite { tenant, path, op, bytes_len }` variant, alongside the existing `ToolCall`/`Llm`/`Spawn`/`HttpRequest`. `op` distinguishes `write` from `remove`.
- [ ] `HookedVfsLayer::new(tenant: TenantId, inner: Arc<dyn VirtualFs>, hooks: Arc<HookPipeline>) -> Self` constructs a layer bound to the given tenant. The tenant is required at construction (no default); events fired through the chain carry it. The layer runs `Operation::VfsWrite` through the chain on every `write` and `remove` before forwarding.
- [ ] When a hook returns `Verdict::Deny`, the inner VFS is not called, the write returns `VfsError::HookDenied { reason }` carrying the hook's reason verbatim, and no `VfsEvent` is emitted by any layer above (because no write landed). Verified by a test using a `RecordingFs` that asserts the inner FS received zero `write` and zero `remove` calls.
- [ ] When a hook returns `Verdict::Continue` with modified context, only the `path` field is honored as a mutation; the new path is forwarded to the inner VFS and any emitted `VfsEvent` carries the mutated path. Modifications to `bytes_len` are silently ignored. A hook attempting to change `tenant` OR `op` causes the write to fail with `VfsError::HookContractViolation`.
- [ ] When a hook returns `Verdict::Continue` without modifying any field, behavior is identical to no mutation.
- [ ] When all hooks return `Verdict::Continue` (no modifications), behavior is identical to having no hooks present (same write outcome, same `VfsEvent` payload).
- [ ] When a hook returns `Verdict::Kill`, the write fails with `VfsError::HookKilled { reason }` and no `VfsEvent` is emitted.
- [ ] Hook chain is global: every `VfsWrite` passes through every registered hook (matches existing `Operation::ToolCall` semantics).
- [ ] An integration test installs a deny-all `VfsWrite` hook and asserts a write to `/var/memory/foo.md` fails with the hook's reason and no event is observed.
- [ ] An integration test installs a mutate hook that rewrites the path from `/a` to `/b` and asserts the resulting `VfsEvent::Written` carries `path = /b`.

### Behavior invariants

- [ ] Per-path ordering: writes to the same path in order A, B, C are observed by a watcher in order A, B, C (modulo `Skipped`).
- [ ] Per-subscriber emission: a write through a `NotifyingFsLayer(MemoryStoreFs(...))` stack produces one event for a subscriber to the `NotifyingFsLayer` AND one event for a subscriber to the `MemoryStoreFs` — they are independent broadcasts. A subscriber to either layer sees exactly one event per write through its own layer; layers do NOT coordinate to suppress each other.
- [ ] Cross-tenant isolation across a stack containing both `MemoryStoreFs` and `NotifyingFsLayer`: tenant A's watcher receives only tenant A's events.
- [ ] Hook denial does NOT publish a notification.
- [ ] Hook mutation publishes a notification reflecting the mutated landed state.
- [ ] A watcher created on a `VirtualFs` whose only reference is then dropped sees its `recv` return `None` (channel closed) rather than hanging.

### Negative coverage

- [ ] Subscribing to `MemoryFs` (the default-impl case) yields a watcher whose `recv` returns `None` (dead channel), not an error.
- [ ] A subscriber whose prefix matches no writes returns no events; the broadcast remains alive for other subscribers.
- [ ] Subscribing with an empty prefix (`""`) is equivalent to `"/"` — receives all events.

## Observability (see S010)

- [ ] On every `VfsEvent` published (across all layer impls), the counter `simulacra.vfs.events` is incremented with attributes `kind ∈ {written, removed}` and `layer ∈ {memory_store_fs, notifying, ...}`. `VfsEvent::Skipped` is a watcher-side synthetic signal (consumer-observed, derived from broadcast `Lagged`) and is not counted as a published event.
- [ ] On every `Operation::VfsWrite` hook chain invocation, a span `vfs_write_hook` is emitted as a child of the calling span with attributes `simulacra.vfs.tenant`, `simulacra.vfs.path` (the original requested path), `simulacra.vfs.bytes_len`, `simulacra.vfs.hook_outcome ∈ {allow, mutate, deny, kill, violation, error}`. Semantics: `allow` = `Verdict::Continue` with no path change; `mutate` = `Continue` with a mutated `path`; `deny` = `Verdict::Deny`; `kill` = `Verdict::Kill`; `violation` = `Continue` returned with a different `tenant` or `op` (rejected as `VfsError::HookContractViolation`); `error` = any other hook-chain failure (e.g., serde failure, hook execution error propagated as `Err`).
- [ ] An integration test runs a small program that issues VFS writes through a `HookedVfsLayer` + `NotifyingFsLayer` stack and verifies the counter and span are visible via local Obsidian PromQL/TraceQL queries (per `rules/R010-observability-validation.md`).

## Test Strategy

Per `rules/R004-test-against-fakes.md`:

- **`simulacra-vfs` unit tests** use real `MemoryFs` for default-impl coverage and a dedicated `NotifyingFsLayer` over `MemoryFs` for the broadcast/watcher behavior.
- **`MemoryStoreFs` adapter tests** use real `SqliteMemoryStore` against `tempfile::tempdir()` since the adapter sits on top of the existing memory infrastructure.
- **Slow-consumer / Skipped tests** drive the broadcast ring deterministically by sizing the capacity small (e.g., 4) and writing more than capacity without consuming.
- **Hook tests** install a recording fake `Hooks` impl that captures the operation, returns a configured `Decision`, and lets the test assert order and outcome.

## Known Limitations

1. **Per-process only.** Watchers live in the same process as the writer. Multi-process consumers (e.g., a sidecar collector) need to run the API server.
2. **Bounded best-effort.** `Skipped(n)` is the safety valve; consumers that need lossless delivery must drain faster than writers produce.
3. **No identity-preserving renames.** A `mv /a /b` surfaces as `Removed(/a) + Written(/b)`. Consumers building inverted indexes or lineage graphs need to reconstruct identity themselves; we'll add `Renamed` if/when a consumer requires it.
4. **Paths are emitted as supplied.** `VfsEvent::{Written, Removed}` carry the path string the caller passed to `write` / `remove`. The notifying / hook layers do NOT canonicalize. Callers requesting non-canonical paths (e.g., `/foo/../bar`) get those raw bytes in the event payload and the hook context. Path canonicalization is the caller's responsibility (or the responsibility of an inner VFS layer that performs it before this layer runs).

## Open questions

(All four open questions from the original Draft were resolved at activation — see "Resolved design decisions" above. Add new ones here as they arise during implementation.)
