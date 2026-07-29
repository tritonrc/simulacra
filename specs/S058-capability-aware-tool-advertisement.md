# S058 — Capability-Aware Tool Advertisement

**Status:** Active

## Problem

Capability enforcement and tool advertisement are decided in different places, and only
one of them consults the `CapabilityToken`.

- **Enforcement** happens at execution time, inside the cell/proxy layer (the Golden Rule
  chain): `AgentCell::execute_shell_with_workdir` → `check_and_journal_capability(||
  self.capability.check_shell(), …)` (`simulacra-sandbox/src/shell.rs:45-51`); file I/O
  goes through `check_path_read`/`check_path_write`
  (`simulacra-types/src/capability.rs:265,293`); JS through `check_javascript` (`:309`);
  Python through `check_python` (`simulacra-python/src/tool.rs:48`). This layer is sound
  and this spec does not touch it.
- **Advertisement** — the tool list the model actually sees — is assembled by
  `ToolRegistry::definitions()` (`simulacra-tool/src/registry.rs:150-156`) with **no
  reference to any token**. `register_builtins`
  (`simulacra-tool/src/sandbox_tools.rs:423-459`) registers all six builtins
  (`file_read`, `file_write`, `apply_patch`, `shell_exec`, `js_exec`, `list_dir`)
  unconditionally, and the individual tool structs are private
  (`sandbox_tools.rs:95,146,193,295,351`) — `register_builtins` is the only public
  entry point, all-or-nothing.

Consequence: a host embedding an agent with a restricted token (e.g. `shell: false`,
`javascript: false`, empty `paths_write` — `CapabilityToken` is `#[derive(Default)]` so
this is the default posture, `capability.rs:15-24`) cannot advertise a truthful tool list.
The model sees `shell_exec` and `file_write` that are *guaranteed* to return
`CapabilityDenied`. That wastes turns, produces confusing transcripts, and teaches the
model wrong things about its own affordances. Concrete downstream case: the DevForge
control plane wants to give its teammate agent a small tool surface and today must avoid
`register_builtins` entirely because it cannot take `file_read`/`list_dir` without also
advertising a dead `shell_exec`.

## Rejected design — static `Tool::requires()` + registration-time demotion

The first proposal was: a `CapabilityRequirement` enum, a defaulted
`Tool::requires(&self) -> &[CapabilityRequirement] { &[] }`, and
`try_register_with_exposure` demoting unsatisfiable tools to `ToolExposure::Hidden`.
Adversarial review (Codex, 2026-07-28) killed it, and the objections were verified:

1. **Requirements are argument-dependent, so a static declaration is unsound.**
   `apply_patch` needs read capability on *update* operations but not on pure adds;
   network/MCP authorization depends on which host/tool the *call* names
   (`check_network(url)`, `check_mcp_tool(name)`); per-path write grants cannot be
   decided at registration because the path arrives in the arguments. Any static
   `requires()` is either too coarse (lies in both directions) or degenerates to
   "some grant of this kind exists".
2. **A defaulted `&[]` makes it opt-in annotation, not an invariant.** Any
   capability-sensitive tool that forgets to override — `py_exec`
   (`simulacra-python/src/tool.rs:27`) on day one — silently stays advertised. The
   feature would *read* like a guarantee ("advertised ⇒ executable") while actually
   being a lint.
3. **Demotion mutates permanent registry state from one token's perspective**, but the
   registry is not intrinsically single-token: `call()` takes the token *per call*
   (`registry.rs:193-208`). Demotion also strands `Deferred` tools —
   `search_deferred()` filters on `exposure == Deferred` (`registry.rs:166-185`), so a
   demoted deferred tool becomes permanently undiscoverable — and the duplicate-name
   check (`registry.rs:125-135`) blocks re-registering it under a broader token later.
4. **Storing a token in `ToolRegistry` at construction** breaks or contorts real call
   sites that build registries before/without a token: `simulacra-cli/src/lib.rs:958`,
   `simulacra-server/src/engine.rs:1561`,
   `simulacra-runtime/src/spawn_tool/child_environment.rs:96`.

## Proposed design — a derived view, plus granular registration

Two small, independent pieces. Neither changes enforcement; execution-time capability
checks remain the sole security boundary. This is honesty-of-advertisement, UX only.

### A. `definitions_for(&CapabilityToken)` — computed, never stored

1. New defaulted method on `Tool` (`simulacra-types/src/tool.rs`, alongside the existing
   defaulted extension methods at `:204-235`):

   ```rust
   /// Whether this tool should be advertised to a model operating under
   /// `capability`. Advertisement only — execution-time checks still enforce.
   /// Default `true`: tools with no static capability signal stay visible.
   fn advertised_to(&self, _capability: &CapabilityToken) -> bool { true }
   ```

2. New registry views (`simulacra-tool/src/registry.rs`), leaving `definitions()` and
   `search_deferred()` untouched:

   ```rust
   pub fn definitions_for(&self, capability: &CapabilityToken) -> Vec<ToolDefinition>;
   pub fn search_deferred_for(&self, query: &str, capability: &CapabilityToken) -> Vec<ToolDefinition>;
   ```

   Same filters as their existing counterparts, plus `advertised_to(capability)`. No
   registry state is read or written beyond the immutable tool list — same token in,
   same view out; a different token gets a different view from the same registry.

3. Builtin overrides — only the *statically decidable, coarse* signal:
   - `shell_exec` → `capability.check_shell().is_ok()`
   - `js_exec` → `capability.check_javascript().is_ok()`
   - `py_exec` → `capability.check_python().is_ok()`
   - `file_write`, `apply_patch` → token grants **some** generic write pattern, or
     memory is enabled with **some** write scope
   - `file_read`, `list_dir` → token grants **some** generic read pattern, or memory
     is enabled with **some** search scope

   Memory scopes count because `check_path_read`/`check_path_write` route memory paths
   exclusively through `CapabilityToken.memory`, not the generic path vectors.
   Documented limitation: "some grant exists" ≠ "the path this call will name is
   allowed". The view does not match or otherwise interpret a future argument path.
   That gap is enforcement's job, per-call, as today.

4. Hosts opt in by calling `definitions_for` where they hold the agent's token (server
   engine, CLI, child-environment spawn). Hosts without a meaningful token keep calling
   `definitions()` — zero behavior change.

### B. Granular builtin registration

Export the pieces `register_builtins` composes, so hosts can take a subset:

```rust
pub fn register_file_tools(registry: &mut ToolRegistry, cell: Arc<AgentCell>) -> Result<(), ToolError>;  // file_read, file_write, apply_patch, list_dir
pub fn register_exec_tools(registry: &mut ToolRegistry, cell: Arc<AgentCell>) -> Result<(), ToolError>;  // shell_exec, js_exec
```

`register_builtins` composes the same registrations while preserving its legacy
`definitions()` order: `file_read`, `file_write`, `apply_patch`, `shell_exec`, `js_exec`,
`list_dir`. This fixes the all-or-nothing problem even for hosts that never adopt
`definitions_for`.

## Acceptance

- [x] A token with `shell: false` ⇒ `definitions_for` omits `shell_exec`; `definitions()`
      still includes it; `call("shell_exec", …)` still returns `CapabilityDenied`.
- [x] A token with empty `paths_write` but non-empty `paths_read` ⇒ `file_read`/`list_dir`
      advertised, `file_write`/`apply_patch` not.
- [x] A token with empty generic path vectors but enabled memory-only read or write scopes
      advertises the corresponding read or write file tools. Argument-specific path
      matching remains an execution-time concern.
- [x] Two different tokens against the **same** registry get different
      `definitions_for` views; registry state (exposure, duplicate-check) is unaffected.
- [x] A `Deferred` tool not advertised to a token is absent from `search_deferred_for`
      for that token but still present in plain `search_deferred`.
- [x] A tool with the defaulted `advertised_to` appears in every `definitions_for` view
      (backward compatibility pin).
- [x] `register_file_tools` alone advertises no `shell_exec`/`js_exec`; `register_builtins`
      still registers all six in its legacy `definitions()` order.
- [x] Doc comment on `advertised_to` + `definitions_for` states explicitly that this is
      advertisement, not enforcement, and that the Golden Rule chain is unchanged.

## Out of scope

- Any change to execution-time capability checking.
- Argument-dependent advertisement (per-path, per-host, per-MCP-tool) — undecidable at
  list-assembly time by construction.
- Auto-adopting `definitions_for` in every host in this change; hosts migrate as they
  acquire a token at list-assembly time.
- DevForge `share_file` (control-plane S071) does **not** depend on this — it registers
  per-surface tools itself. This spec is an independent runtime improvement.
