# S040 — WASM-Backed VFS Nodes

**Status:** Draft (skeleton — split out from S039 on 2026-04-25)
**Crates involved:** `simulacra-vfs`, `simulacra-wasm`, `simulacra-runtime`, `simulacra-types`

## Dependencies

- **ARCHITECTURE.md** — VFS layering, capability model
- **S001** — Virtual filesystem (the trait being extended)
- **S011** — Sandbox + VFS Golden Rule (snapshot/restore invariant)
- **S025** — WASM tool hosting (wasmtime WASIp2, fuel metering — same runtime infra reused for the VFS surface)
- **S026** — Hook pipeline (capability + governance for WASM calls)
- **S039** — VFS Write Notifications (a WASM VFS module is a natural consumer of writes happening elsewhere in the stack)

## Scope

A subtree of the VFS can be serviced by a WASM module instead of the standard layered stack. `read(/foo/bar.json)` dispatches to a WASM function; `write(/foo/bar.json, bytes)` dispatches to a WASM function. Enables userspace filesystems — virtual databases, remote API proxies, DSL-driven content transformations — without adding a new Rust layer per use case.

This is the "extension story" for VFS: customers ship a WASM module that exposes a custom data source as filesystem paths agents can read and write. The runtime trades a Rust merge for a `simulacra.toml` mount declaration.

**In scope:**

1. `WasmVfsLayer` — `impl VirtualFs` that dispatches `read` / `write` / `list_dir` / `remove` / `stat` to a WASM module via `simulacra-wasm`.
2. Module export contract:
   - `vfs_read(path: string) -> bytes | error`
   - `vfs_write(path: string, data: bytes) -> ok | error`
   - `vfs_list_dir(path: string) -> [entries]`
   - `vfs_remove(path: string) -> ok | error`
   - `vfs_stat(path: string) -> metadata | not_found`
3. Mount point declared at layer construction; paths outside the mount delegate to the inner VFS.
4. Capability gating: `paths_read` / `paths_write` checked at the layer entry, BEFORE the WASM call.
5. Per-call WASM budget — CPU fuel, memory ceiling, wall-clock duration. Reuses S025's metering.
6. Journal records the backing module id + per-call outcome for audit.

**Out of scope:**

- Durable, replicated WASM modules. A WASM VFS module is process-local for now.
- Module hot-reload while agents are mid-call. Requires a snapshot/resume mechanism; defer.
- Cross-layer atomicity. A write that spans `WasmVfsLayer` and an inner layer is not a transaction.
- Discovery / distribution mechanism for WASM VFS modules. Today they're file paths in `simulacra.toml`; a registry is a separate spec.

## Context

**Why now.** S025 (wasm-tools) showed wasmtime + WASIp2 can host extension code with bounded resources. Generalizing the same execution model to a VFS layer is the next move — same fuel/memory/duration story, different export contract. Customers who can already ship a WASM tool can ship a WASM filesystem.

**Why not extend tool hooks (S026).** A WASM tool runs on agent invocation. A WASM VFS layer runs on every read/write to its mount point — including writes from non-tool paths (journal, session artifacts). The execution surfaces are different.

**Why this is split from S039.** Write notifications are an ambient runtime feature; WASM-backed nodes are an extension mechanism. Each has independent open questions (snapshot semantics for remote-API-backed modules vs. backpressure semantics for notifications) and either can ship without the other.

## Design (placeholder — populate before activation)

Rough shape:

- `WasmVfsLayer::mount(prefix: &str, module: WasmModule, budget: WasmBudget) -> WasmVfsLayer`.
- On VFS op: capability check → fuel/memory/duration limits set on the wasmtime store → call the appropriate `vfs_*` export → translate the WASM result into `Result<_, VfsError>`.
- Errors from the module surface as `VfsError::WasmModule { module_id, kind }` with kinds `Trap`, `Timeout`, `OutOfFuel`, `OutOfMemory`, `Returned(structured_error)`.
- `simulacra-runtime` registers a `WasmVfsLayer` at the configured mount point during the existing VFS-stack construction phase.
- Snapshot/restore (S011): `WasmVfsLayer::snapshot()` snapshots the WASM module's linear memory; `restore` re-instantiates from it. Modules backing remote APIs declare themselves non-snapshotable; the runtime fails the snapshot for sandboxes that contain them.

## Behavior (placeholder — populate before activation)

- Read of a path under the mount goes to the module; outside the mount, delegates to the inner VFS.
- Capability denied → no WASM call; VFS error returned synchronously.
- WASM call exceeds budget → call aborted, `VfsError::WasmModule { kind: OutOfFuel | Timeout }` returned.
- Module trap → call aborted, `VfsError::WasmModule { kind: Trap }`. Module instance recreated for next call (no shared corrupted state).
- Module return path includes structured errors (e.g., `not_found`, `permission_denied`) that map to `VfsError` variants.
- Journal entry written for every WASM VFS call (success and failure) with module id, op, path, outcome.

## Assertions (empty — to be filled when this spec is activated)

- [ ] TBD

## Observability (see S010)

- [ ] TBD (spans for WASM VFS calls, histogram for call duration, counter per outcome class)

## Open questions

1. Does the layer expose `snapshot` / `restore` (required by S011)? If yes, what does "snapshot" mean for a module backing a remote API — opaque blob the module produces, or runtime walks linear memory?
2. Do WASM VFS modules need their own capability token, or do they inherit the calling agent's? Inheritance is simpler; a module-specific token would let an operator further restrict what the module sees.
3. Is the module instance per-mount or per-call? Per-mount is fast but accumulates state; per-call is hermetic but pays instantiation cost on every op. Likely per-mount with a forced reset on trap.
4. Is `list_dir` paginated? Mounts backed by remote APIs may have large directories; a single `[entries]` return won't scale.
