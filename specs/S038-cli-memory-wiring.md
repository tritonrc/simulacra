# S038 — CLI Memory Wiring

**Status:** Active
**Crates:** `simulacra-cli` (primary), `simulacra-config`, `simulacra-memory`, `simulacra-runtime`, `simulacra-tool`, `simulacra-vfs`, `simulacra-types`

## Dependencies

- **S001** — Virtual filesystem
- **S011** — Sandbox composition
- **S012** — Built-in tools
- **S034** — SimulacraEngine (server-side reference for memory wiring)
- **S037** — Memory and semantic retrieval (the subsystem being wired in)

## Scope

The regular `simulacra` CLI (`simulacra-cli`) ignores the memory subsystem entirely.
Even if `simulacra.toml` declares an agent type with `[agent_types.X.capabilities.memory] enabled = true`,
no `MemoryStore` is constructed, no `MemoryStoreFs` is installed in the VFS
stack, no memory tools are registered, and no `BackgroundEmbedder` runs.
Memory writes go to the in-memory `MemoryFs` and disappear when the process
exits.

S038 closes this gap. After this spec lands, a CLI run with `[memory]` in
`simulacra.toml` provides the entry agent with persistent memory backed by
`SqliteMemoryStore` + `SqliteVectorIndex`, gated by capability and drained
cleanly on exit.

S038 is **strictly scoped to the entry agent**. Sub-agents spawned via
`spawn_agent` continue to receive a fresh tool registry with builtins only
and reuse the parent VFS stack as today (`crates/simulacra-runtime/src/spawn_tool.rs`).
Wiring memory into sub-agent construction is a separate, larger spec —
see "Out of scope" and "Known limitations" below.

**In scope:**

1. **`simulacra-memory` API additions** (prerequisite for the rest):
   - `BackgroundEmbedder::shutdown(self) -> Result<(), MemoryError>` — async,
     stops the dispatcher loop, drops per-tenant senders so workers exit
     cleanly, awaits all worker join handles. Replaces today's
     abort-on-Drop behavior for the orderly-shutdown path.
   - `MemoryStore::ensure_tenant(&self, tenant: &TenantId) -> Result<(), MemoryError>` —
     forces a tenant DB open + schema check at bootstrap, so corrupted /
     non-openable tenant DBs are surfaced as a startup failure rather than
     a deferred runtime error.
   - `VectorIndex::ensure_tenant(&self, tenant: &TenantId) -> Result<(), MemoryError>` —
     same, for the vector index.
   - These three additions are explicitly part of S038 because the spec
     would otherwise be unimplementable (the older draft promised a drain
     API that did not exist; reviewer GPT-5.4 caught this).

2. **`simulacra-config`** — new top-level `MemoryConfig` parsed from `[memory]`:
   ```rust
   #[derive(Debug, Clone, Serialize, Deserialize)]
   pub struct MemoryConfig {
       pub dir: PathBuf,
       #[serde(default = "default_tenant")]
       pub tenant: String,
   }
   ```
   No `embedder` field — the older draft included one but the bootstrap
   would have ignored it. Add it back when an embedder selector exists.
   On `SimulacraConfig`: `#[serde(default)] pub memory: Option<MemoryConfig>`.

3. **`simulacra-cli/src/lib.rs::CliBootstrap`** — sync preflight in `bootstrap()`:
   - Parse `MemoryConfig` from the loaded `SimulacraConfig`.
   - If `Some`, validate `TenantId::parse(&memory.tenant)` — fail-fast on error.
   - `std::fs::create_dir_all(&memory.dir)` — fail-fast on IO error.
   - Construct `SqliteMemoryStore::new(&memory.dir)`.
   - Construct `DefaultEmbedder::load_default()`.
   - Construct `SqliteVectorIndex::new(&memory.dir, embedder.id().clone())`.
   - Call `store.ensure_tenant(&tenant)` and `index.ensure_tenant(&tenant)`
     — fail-fast on either. This is the sqlite-validation step the prior
     draft was missing.
   - Construct the `RecentWritesBuffer` and the `markdown` chunker selector.
   - When the **entry agent's** `CapabilityToken.memory.enabled == true`:
     - Wrap the VFS stack with `MemoryStoreFs::new(inner, tenant, store, capability).with_rrwb(rrwb)`.
     - Call `register_memory_tools(&mut registry, MemoryToolHandles { ... })` —
       same `Arc<RRWB>` as the FS layer.
   - Stash `MemoryRuntimeState` (store, index, embedder, chunker selector,
     rrwb, tenant) on `CliBootstrap` so `run_booted` can spawn the
     embedder once the tokio runtime exists.
   - When the entry agent has `memory.enabled == false` AND `[memory]` is
     present, do NOT wrap the VFS with `MemoryStoreFs` and do NOT register
     memory tools. Skip stashing the state — there is nothing to drive the
     embedder, so it should not spawn (see B5 reconciliation, Known
     limitations).

4. **`simulacra-cli/src/lib.rs::run_booted`** — async startup + shutdown:
   - After the tokio runtime is created and inside its scope:
     - If `bootstrap.memory_runtime.is_some()`, call
       `BackgroundEmbedder::spawn(...)` and bind the handle to a local
       `embedder_handle: Option<BackgroundEmbedder>` that lives until
       after the agent loop returns.
     - Run the agent loop as today.
     - **Before the runtime is torn down**, call
       `embedder_handle.take().unwrap().shutdown().await`. The shutdown
       MUST be invoked on both the success and the error paths — wrap the
       agent loop call in a `match`/`finally`-style local `async` block
       and shutdown unconditionally. Drop order is not load-bearing; the
       explicit `await` is.
   - This split is mandatory because `BackgroundEmbedder::spawn` requires
     a tokio runtime (`crates/simulacra-memory/src/background.rs:76-83`) but
     the rest of the bootstrap is sync. The prior draft put everything in
     one phase, which was unimplementable.

5. **Telemetry** — exactly one tracing span and one log line:
   - Bootstrap emits a `memory_bootstrap` span as a child of the existing
     `cli_run` span (`crates/simulacra-cli/src/lib.rs:952-960`). Span
     attributes: `simulacra.memory.dir`, `simulacra.memory.tenant`,
     `simulacra.memory.embedder_id`, `simulacra.memory.embedder_dim`,
     `simulacra.memory.entry_agent_enabled` (bool).
   - At the end of bootstrap (success path), one INFO log line with the
     same fields plus `simulacra.memory.outcome = "wired" | "skipped_no_section" | "skipped_disabled_for_entry_agent"`.
   - The earlier draft asked for three sub-spans
     (`memory_store_open`, `vector_index_open`, `bg_embedder_spawn`) AND
     a `memory_bootstrap` span. That was internally inconsistent; we
     reconcile to one span.

**Out of scope (explicitly):**

- **Sub-agent memory wiring.** Sub-agents in `simulacra-runtime/src/spawn_tool.rs`
  reuse the parent's `Arc<dyn VirtualFs>` and build a fresh tool registry
  containing only `register_builtins`. They do not install `MemoryStoreFs`
  and do not see memory tools. Wiring memory into sub-agent construction
  requires invasive changes to the spawn factory and is a separate spec.
  S038 documents this as a known limitation; the entry-only path is
  enough to unlock the two-Simulacra demo and any single-agent CLI workflow.
- Wiping or rebuilding the index on embedder dimension changes.
- Retention reaper (S037 follow-up).
- Hot-swapping the embedder mid-run.
- Multi-tenant CLI runs (the CLI is single-tenant; one resolved
  `TenantId` for the lifetime of the process).
- Multi-process coordination beyond what SQLite WAL already provides
  (see "Multi-process semantics" below).
- Per-agent-type memory directory overrides.

## Context

`crates/simulacra-server/src/engine.rs:780-950` does the full memory wiring
sequence: VFS layering with `MemoryStoreFs`, conditional
`register_memory_tools`, RRWB threading. The CLI does none of it.

The asymmetry is the bug. After S038, memory works the same way regardless
of whether you launch via the CLI or via the API server, **for the entry
agent**. Sub-agent parity is deferred.

### Why the spec owns API additions to `simulacra-memory`

Reviewer GPT-5.4 correctly observed that the previous draft promised
"drain via the embedder's existing drain/shutdown API" but no such API
existed. Rather than spawn a separate spec for two trivial method
additions, S038 owns them because they have no consumer outside the
work being done here. The simulacra-server engine path benefits too: today
its embedder is also abort-on-Drop, which is fine for an always-running
server but wrong for any orderly shutdown.

### Multi-process semantics

`SqliteMemoryStore` and `SqliteVectorIndex` use SQLite WAL with a busy
timeout (`crates/simulacra-memory/src/sqlite_store.rs:112-115` and
`sqlite_index.rs:138-142`), so two CLI processes against the same
`memory.dir` and `tenant` will *eventually* converge through the
on-disk DB and index. They will **NOT** see each other's
`RecentWritesBuffer` (per-process) and the in-process broadcast event
stream is per-process only (`SqliteMemoryStore` doc comment lines
12-18). S038 documents this and does not attempt to fix it. Operators
who want multi-writer semantics should run the API server.

### Why no sqlite "partial-init cleanup" path

Reviewer GPT-5.4 also caught that the prior "half-initialized SQLite
state" framing was wrong. `SqliteMemoryStore::new()` only does
`create_dir_all(root/memory)`. Tenant DBs are not opened until first
use. After the new `ensure_tenant()` is called eagerly, partial init
means *at most* an empty `memory/` directory left under `dir`. We do
not delete it on failure — explicit `rm -rf` by the operator is
acceptable and avoids the "did the cleanup just delete my real data"
class of bug.

## Design

### TOML schema

```toml
[memory]
dir = "./.simulacra-memory"
tenant = "cli"           # optional, default "cli"
```

When `[memory]` is **absent**, the CLI behaves identically to today
(see "Acceptance criteria → Memory absent" for the concrete observable
form of "identical").

### `MemoryRuntimeState` (local to `simulacra-cli`)

```rust
// crates/simulacra-cli/src/lib.rs (private to the bootstrap module)
pub(crate) struct MemoryRuntimeState {
    pub tenant: TenantId,
    pub store: Arc<dyn MemoryStore>,
    pub index: Arc<dyn VectorIndex>,
    pub embedder: Arc<dyn Embedder>,
    pub chunker_selector: ChunkerSelector,
    pub rrwb: Arc<Mutex<RecentWritesBuffer>>,
}
```

Named **`MemoryRuntimeState`** to avoid confusion with the existing
`simulacra_tool::MemoryToolHandles` (NIT N1 from review).

### Bootstrap sequence — sync phase (in `bootstrap()`)

```text
load simulacra.toml -> SimulacraConfig
  if config.memory.is_some():
    1. tenant = TenantId::parse(memory.tenant)?  // fail-fast
    2. fs::create_dir_all(&memory.dir)?           // fail-fast
    3. embedder = DefaultEmbedder::load_default()?
    4. store = SqliteMemoryStore::new(&memory.dir)?
    5. index = SqliteVectorIndex::new(&memory.dir, embedder.id().clone())?
    6. store.ensure_tenant(&tenant)?              // sqlite open + schema check
    7. index.ensure_tenant(&tenant)?              // sqlite open + schema check
    8. rrwb = Arc::new(Mutex::new(RecentWritesBuffer::new()))
    9. chunker_selector = build_md_chunker_selector()
    10. resolve entry_agent capability_token (existing path)
    11. if capability_token.memory.enabled:
          vfs = MemoryStoreFs::new(inner_vfs, tenant.clone(), store.clone(), cap.memory.clone())
                  .with_rrwb(Arc::clone(&rrwb))
          register_memory_tools(&mut tool_registry, MemoryToolHandles { ... rrwb: Some(rrwb.clone()) })
          memory_runtime = Some(MemoryRuntimeState { ... })
        else:
          memory_runtime = None
          warn!("memory configured but entry agent has no memory capability — embedder will not start")
    12. emit `memory_bootstrap` span + INFO log line
```

### Bootstrap sequence — async phase (in `run_booted()`)

```text
build tokio runtime
  rt.block_on(async {
    if let Some(state) = bootstrap.memory_runtime.take() {
      embedder_handle = Some(BackgroundEmbedder::spawn(
        state.store.clone(),
        state.index.clone(),
        state.embedder.clone(),
        state.chunker_selector,
        BackgroundEmbedderConfig::default(),
      )?);
    }
    let agent_result = run_agent_loop(...).await;     // existing agent loop
    if let Some(handle) = embedder_handle.take() {
      handle.shutdown().await?;                       // unconditional drain
    }
    agent_result
  })
```

The shutdown call is on **both** the success and error path. We
implement this with an explicit `match` rather than relying on Rust drop
order, because:
1. Drop order is the reverse of declaration order, which is fragile to
   future refactoring.
2. `shutdown` is async and can return errors that should be reported.
3. The runtime must still be alive when shutdown is awaited; relying on
   `Drop` would race against runtime teardown.

### `BackgroundEmbedder::shutdown` design

```rust
pub async fn shutdown(self) -> Result<(), MemoryError>
```

Implementation outline:
1. Replace the current dispatcher with a select over
   `(subscription.recv(), shutdown_rx)`. On `shutdown_rx` fire, break
   out of the loop.
2. Track per-tenant worker `JoinHandle`s in the dispatcher's
   `tenant_senders` map (becomes `HashMap<TenantId, (Sender, JoinHandle)>`).
3. On shutdown signal, drop all senders, then `await` all worker
   handles with a bounded total timeout (default 30s) so a runaway
   worker cannot hang the CLI forever.
4. Finally `await` the dispatcher handle.
5. Return `Ok(())` if all workers exited cleanly; `MemoryError::Shutdown`
   variants for `Timeout`, `WorkerPanic { tenant }`.

This is a real change to `BackgroundEmbedder`'s internals. It is
load-bearing for S038 and tested in the `simulacra-memory` crate independently.

### `MemoryStore::ensure_tenant` and `VectorIndex::ensure_tenant`

Trait method on both `MemoryStore` and `VectorIndex`:

```rust
fn ensure_tenant(&self, tenant: &TenantId) -> Result<(), MemoryError>;
```

For the SQLite implementations: open the per-tenant connection (the
existing private `open_conn`), run the schema migration if not present,
close. For any in-memory fake: store the tenant in a known set.

### Failure mode table

| Condition | Behavior |
|---|---|
| `[memory]` absent | CLI runs as today; no memory wiring; no embedder. |
| `[memory]` present, `dir` not creatable (permission denied, ENOSPC at create time) | `CliError::Memory(...)` with `"cannot create memory dir: ..."`; non-zero CLI exit. |
| `[memory]` present, `tenant` fails `TenantId::parse` | `CliError::Memory("invalid tenant id: ...")`; non-zero CLI exit. |
| `[memory]` present, `SqliteMemoryStore::new` fails | `CliError::Memory("memory store open failed: ...")`; non-zero CLI exit. |
| `[memory]` present, `SqliteVectorIndex::new` fails | `CliError::Memory("vector index open failed: ...")`; non-zero CLI exit. |
| `[memory]` present, `ensure_tenant` fails (corrupted SQLite, wrong schema, locked DB beyond busy timeout) | `CliError::Memory("ensure_tenant failed: ...")`; non-zero CLI exit. **Without `ensure_tenant` this would be a deferred runtime error — that's the whole point of the new method.** |
| `[memory]` present, entry agent has `memory.enabled = false` | OK; one WARN log line; no embedder spawned. |
| `[memory]` absent, entry agent has `memory.enabled = true` | OK; one WARN log line containing `"memory enabled in agent type"` and `"no [memory] section"`; agent has no memory tools. |
| `BackgroundEmbedder::spawn` fails inside `run_booted` | Surfaced as `CliError`; agent loop is NOT entered. |
| `BackgroundEmbedder::shutdown` exceeds 30s drain timeout | WARN log; `CliError::Memory("embedder shutdown timeout")` returned in addition to (not replacing) the agent loop result. Agent result wins on conflict; embedder timeout is reported separately. |
| Worker panic during shutdown drain | WARN log per tenant; `CliError::Memory("embedder worker panic in tenant ...")` returned alongside the agent loop result. |
| Disk full mid-run during a memory write | Surfaces through the existing memory tool error path; not S038's responsibility, but the failure mode table acknowledges it exists. |
| SQLite WAL contention with another process holding the DB longer than busy timeout | Surfaces at `ensure_tenant` (startup) or as a tool error mid-run. Same as above — not S038's job to retry indefinitely. |
| Two CLI processes against the same `dir` + `tenant` | Allowed but not coordinated. They share the on-disk store/index via SQLite WAL. They do NOT share `RecentWritesBuffer` or the in-process event stream. Documented; not enforced. |

### Backward-compat / workspace churn

Adding `SimulacraConfig.memory: Option<MemoryConfig>` is parse-compatible
(`#[serde(default)]`) but is **not free** at the type level. Several
files in the workspace construct `SimulacraConfig { ... }` literals that
will stop compiling until the new field is added:

```bash
$ rg -l 'SimulacraConfig\s*\{' crates
crates/simulacra-cli/src/lib.rs
crates/simulacra-cli/tests/cli_bootstrap.rs
crates/simulacra-config/src/lib.rs
crates/simulacra-server/examples/s037_coworkers_demo.rs
crates/simulacra-server/examples/s037_memory_demo.rs
crates/simulacra-server/examples/s037_product_team_demo.rs   # already deleted
crates/simulacra-server/examples/toy_saas.rs
crates/simulacra-server/examples/serve.rs
... (≈17 files total)
```

Implementation must update each constructor with `memory: None` (or the
new field, if appropriate). Acceptance criteria below include a final
`cargo build --workspace` gate to catch them all.

## Acceptance Criteria

### `simulacra-memory` API additions

- [x] `MemoryStore` trait gains `fn ensure_tenant(&self, tenant: &TenantId) -> Result<(), MemoryError>`.
- [x] `VectorIndex` trait gains `fn ensure_tenant(&self, tenant: &TenantId) -> Result<(), MemoryError>`.
- [x] `SqliteMemoryStore::ensure_tenant` opens the tenant DB, runs migrations, closes. A subsequent call is idempotent.
- [x] `SqliteVectorIndex::ensure_tenant` opens the tenant DB, runs migrations, closes. Idempotent.
- [x] A unit test in `simulacra-memory` writes a corrupt sqlite file at the expected tenant path and asserts `ensure_tenant` returns an error containing `"corrupt"` or `"malformed"`.
- [x] `BackgroundEmbedder::shutdown(self) -> Result<(), MemoryError>` exists, is async, takes ownership of `self`.
- [x] `BackgroundEmbedder` internally tracks per-tenant worker `JoinHandle`s.
- [x] `BackgroundEmbedder::shutdown` stops the dispatcher loop, drops senders, awaits all workers within a 30s default timeout, returns `Ok(())` on clean exit.
- [x] `BackgroundEmbedder::shutdown` returns `MemoryError::ShutdownTimeout` if any worker exceeds the drain timeout.
- [x] `BackgroundEmbedder::shutdown` returns `MemoryError::WorkerPanic { tenant }` if a worker panicked, while still attempting to shut down the rest.
- [x] A unit test wires a fake `Embedder` whose `embed` blocks on a controllable barrier and asserts `shutdown` waits for in-flight work to complete before returning.
- [x] A unit test asserts that calling `shutdown` on an idle embedder (no events ever delivered) returns within 1 second.

### Config parsing

- [x] `simulacra-config::MemoryConfig { dir, tenant }` exists, derives `Serialize/Deserialize`.
- [x] `MemoryConfig.tenant` defaults to `"cli"` when absent.
- [x] `SimulacraConfig` gains `#[serde(default)] pub memory: Option<MemoryConfig>`.
- [x] Parsing a `simulacra.toml` with no `[memory]` yields `SimulacraConfig.memory == None`.
- [x] Parsing a `simulacra.toml` with `[memory] dir = "./.x"` yields `Some(MemoryConfig { dir: "./.x", tenant: "cli" })`.
- [x] All existing `SimulacraConfig { ... }` literals in the workspace are updated and `cargo build --workspace` succeeds.

### Bootstrap behavior — memory absent (concrete observables)

- [x] CLI run with no `[memory]`: no `BackgroundEmbedder` is spawned (a fake embedder injected for testing records zero `spawn` calls).
- [x] CLI run with no `[memory]`: the resolved `tool_definitions` list contains neither `semantic_search` nor `memory_read_chunk`.
- [x] CLI run with no `[memory]`: writing to `/var/memory/foo.md` from the agent fails with a VFS `NotFound` (because `MemoryStoreFs` is not in the stack).
- [x] CLI run with no `[memory]`: no `memory_bootstrap` tracing span is emitted.
- [x] CLI run with no `[memory]` AND entry agent has `memory.enabled = true`: one WARN log line containing `"memory enabled in agent type"` and `"no [memory] section"`.

### Bootstrap behavior — memory present, entry agent enabled

- [x] CLI run with `[memory]` and entry agent `memory.enabled = true` creates the `dir` if absent.
- [x] CLI run wraps the VFS stack with `MemoryStoreFs` keyed by the configured `TenantId`.
- [x] CLI run registers `semantic_search` and `memory_read_chunk` in `tool_definitions`.
- [x] CLI run spawns `BackgroundEmbedder` exactly once after the tokio runtime is created.
- [x] CLI run's `MemoryStoreFs` and `MemoryToolHandles` share the **same** `Arc<Mutex<RecentWritesBuffer>>` instance (verified by pointer equality in a test).
- [x] An agent that writes `/var/memory/self/note.md` in CLI run #1 can `semantic_search("note about ...")` and find it in CLI run #2 against the same `dir` and `tenant`. (Driven by an end-to-end integration test using two `bootstrap()` + `run_booted()` calls in the same test process; reuses the existing `OnceLock`-based tracing guards from `crates/simulacra-cli/tests/cli_bootstrap.rs:30-33,129-139`.)
- [x] The persistence test from the previous bullet **fails** when `BackgroundEmbedder::shutdown` is replaced with an immediate return (no drain). This proves drain is load-bearing rather than incidental.
  - To make this test deterministic and not flaky on scheduler timing, the test injects a fake embedder that blocks `embed` until the test signals release. The shutdown path must wait for that release.

### Bootstrap behavior — memory present, entry agent disabled

- [x] CLI run with `[memory]` and entry agent `memory.enabled = false`: VFS stack is NOT wrapped with `MemoryStoreFs`.
- [x] CLI run with `[memory]` and entry agent `memory.enabled = false`: `tool_definitions` contains neither memory tool.
- [x] CLI run with `[memory]` and entry agent `memory.enabled = false`: `BackgroundEmbedder` is **not** spawned (reverses the prior draft).
- [x] CLI run with `[memory]` and entry agent `memory.enabled = false`: one WARN log line stating that memory is configured but the entry agent does not use it.

### Capability invariants (regression coverage)

- [x] Entry agent with `paths_write = ["/**"]` AND `memory.enabled = false` and `[memory]` configured cannot write `/var/memory/foo.md` from the CLI bootstrap. (Verified at the integration level — the unit-level invariant is already covered by `simulacra-types::capability` tests.)
- [x] Entry agent with `paths_write = ["/workspace/**"]`, `memory.enabled = true`, `write_scopes = ["/var/memory/self"]`, `[memory]` configured can write `/var/memory/self/foo.md`.

### Lifecycle

- [x] `BackgroundEmbedder::shutdown` is called on the **success** path when the agent loop exits normally.
- [x] `BackgroundEmbedder::shutdown` is called on the **error** path when the agent loop returns an error.
- [x] If `BackgroundEmbedder::shutdown` itself errors, the error is reported alongside (not in place of) the agent loop result.
- [x] No background tokio task survives `run_booted` returning.

### Telemetry

- [x] When memory is wired (entry agent enabled), the bootstrap emits one `memory_bootstrap` span as a child of `cli_run` with attributes `simulacra.memory.dir`, `simulacra.memory.tenant`, `simulacra.memory.embedder_id`, `simulacra.memory.embedder_dim`, `simulacra.memory.entry_agent_enabled = true`, and an outcome attribute.
- [x] When `[memory]` is configured but the entry agent is disabled, the same span is emitted with `entry_agent_enabled = false` and `outcome = "skipped_disabled_for_entry_agent"`.
- [x] When `[memory]` is absent, no `memory_bootstrap` span is emitted (the absence is itself observable).

### Negative tests

- [x] `simulacra.toml` with `[memory] dir = "/dev/null/cannot-create"` produces a non-zero exit and a stderr line containing `"cannot create memory dir"`.
- [x] `simulacra.toml` with `[memory] tenant = "INVALID UPPER"` produces a non-zero exit and a stderr line containing `"invalid tenant id"`.
- [x] `simulacra.toml` with `[memory] dir = "<tempdir with corrupt sqlite>"` produces a non-zero exit and a stderr line containing `"ensure_tenant failed"`.
- [x] CLI exit code on memory startup failure is the existing `EXIT_USAGE` (or whatever the existing convention is for config-fatal errors — verified against the existing tests).

## Test Strategy

Per `rules/R004-test-against-fakes.md`:

- **`simulacra-memory` unit tests** use real `SqliteMemoryStore` / `SqliteVectorIndex` against `tempfile::tempdir()`. The corrupt-sqlite test writes garbage bytes to the expected tenant DB path before calling `ensure_tenant`.
- **`BackgroundEmbedder::shutdown` tests** use a `FakeEmbedder` that blocks on a `tokio::sync::Notify` so the test can deterministically control "in flight work exists / does not exist".
- **`simulacra-config` tests** round-trip TOML strings through `SimulacraConfig::from_toml`.
- **`simulacra-cli` integration tests** add three new tests in `crates/simulacra-cli/tests/cli_bootstrap.rs`:
  1. End-to-end persistence across two `bootstrap`+`run_booted` calls in the same process.
  2. The same test with a fake embedder whose `shutdown` is replaced by `async {}` — must fail to find the prior write, proving drain is load-bearing.
  3. Negative test matrix for the failure modes table (parameterized).

The persistence test reuses the existing `OnceLock` tracing guards in
`cli_bootstrap.rs:30-33,129-139` (the same harness that survives multiple
`run_booted` calls in the existing CLI test suite). The spec does NOT
require launching a child process — in-process two-run is sufficient and
faster.

## Known Limitations

1. **Sub-agent memory wiring is deferred.** Sub-agents spawned by
   `spawn_agent` reuse the parent's VFS and only get builtin tools. If
   the entry agent has memory and spawns a sub-agent, the sub-agent
   inherits the parent's *VFS stack* (so writes to `/var/memory/**`
   still go through `MemoryStoreFs` and are gated by capability) but
   does NOT see `semantic_search` or `memory_read_chunk` tools. This
   asymmetry is documented; resolution is a separate spec.
2. **No multi-process write coordination.** Two CLI processes against
   the same `dir` + `tenant` share the on-disk store via SQLite WAL but
   not the in-process RRWB or event stream. Acceptable for single-user
   CLI workflows; users who need concurrent writers should run the API
   server.
3. **Drain timeout is fixed at 30s.** Configurable later if needed.
4. **`embedder` field is absent from `MemoryConfig`.** Today's bootstrap
   uses `DefaultEmbedder::load_default()` unconditionally. Add the
   field when an embedder selector exists.

## Implementation Plan (informative, not normative)

The TDD ordering implied by the acceptance criteria:

1. **Phase 1a — `simulacra-memory` API additions.** Tests for `ensure_tenant`
   on store and index. Tests for `BackgroundEmbedder::shutdown`. Implement.
2. **Phase 1b — `simulacra-config` parsing.** Tests for `MemoryConfig`. Implement.
3. **Phase 1c — Workspace constructor churn.** Update every
   `SimulacraConfig { ... }` literal. Pure mechanical.
4. **Phase 2 — `simulacra-cli` bootstrap.** Tests for absent / present /
   disabled / failure paths. Implement in `bootstrap()` and `run_booted()`.
5. **Phase 3 — End-to-end persistence test.** Two-run scenario. Drain
   regression test.
6. **Phase 4 — Mechanical gates + final reviews.**

Each phase can be implemented by an independent sub-agent because the
test files compile-fail until the phase ships, so cross-phase
contamination is structurally prevented.
