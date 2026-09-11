# S062 — Opaque `placement_target` on `spawn_agent`

**Status:** Active — implemented
**Crates involved:** `simulacra-runtime`, `simulacra-types`

## Dependencies

- **ARCHITECTURE.md** — the ACP child runtime boundary: where an ACP child
  executes is the embedding's decision, opaque to Simulacra
- **S018** — `SubAgentSpawned` is journaled before the child runs
- **S056** — `AcpChildRequest` is the whole of what an injected
  `AcpChildRuntime` learns about a spawn
- **S060** — the placement/instructions/task spawn contract this spec extends
- **S061** — the last extension of the accepted top-level key set; this spec
  makes the same shape of change

## Why this spec exists

A placement selects a runtime and a capability profile. It does not say
*where within that profile* a child should run: which workspace, which host,
which sandbox. An embedding that holds several workspaces for one
conversation needs to send a given ACP child to one of them, and it needs to
say so **at spawn time** — the runtime may call `start_child` before the
spawn acknowledgement returns, so anything recorded against the child id
after the ack can arrive too late.

Today the embedding has no way to say it. `spawn_agent` rejects unknown
top-level keys and `SpawnArguments` is `deny_unknown_fields`, so a wrapper
that adds its own argument cannot forward it, and `AcpChildRequest` carries
nothing the embedding did not already know from the placement.

This spec adds one opaque string that travels from the tool arguments to the
`AcpChildRequest` unchanged. Simulacra does not interpret it, validate its
content, or act on it. It is the placement's own job — *where* a child runs
— and it does not touch the S060 refusal of per-spawn skills, which is about
*how* a child works.

## Model-facing contract

`spawn_agent` gains one optional argument:

- `placement_target` — `string`. "An embedding-defined refinement of the
  placement: which workspace, host, or sandbox the child runs in. Opaque to
  the runtime and passed to the placement's child runtime unchanged. Omit it
  unless the embedding's guidance names a value."

The `required` array stays `["placement", "task", "budget"]`. Unknown keys
remain rejected, so `placement_target` joins the accepted top-level key set.

## Flow

The value is preserved byte-for-byte at every hop. Absent is `None`; present
is `Some(value)` with no trimming, no emptiness check, and no normalisation —
the embedding minted it and the embedding reads it.

1. **Arguments.** `SpawnArguments` gains `placement_target: Option<String>`
   (`#[serde(default)]`); `deny_unknown_fields` stays. Shape validation
   rejects a non-string value as `InvalidArguments` naming
   `placement_target`, before any supervisor request is submitted.
2. **Spawn config.** `SpawnConfig` gains `placement_target: Option<String>`,
   set from the parsed arguments.
3. **Journal.** `JournalEntryKind::SubAgentSpawned` gains
   `placement_target: Option<String>`, written beside `placement`. The key
   is present in the encoding only when the value is: entries journaled
   before this spec decode with `None`, and entries journaled without a
   target keep their pre-S062 key set.
4. **ACP request.** `AcpChildRequest` gains `placement_target: Option<String>`
   with the same encoding rule, copied from the spawn config by the task
   factory. The injected `AcpChildRuntime` is the only consumer.

A native placement carries the value through steps 1–3 like any other
argument and then has nothing to hand it to; the native factory ignores it.
Whether a native spawn *should* carry one is the embedding's rule, enforced
in the embedding — Simulacra does not reject it.

## Compatibility

`JOURNAL_SCHEMA_VERSION` stays at 3. `JournalEntryKind` is
`deny_unknown_fields`, so a reader that predates this spec rejects a
`SubAgentSpawned` entry that *carries* a target; entries without one are
byte-identical to before. That is the trade-off S060 made when it added
`instructions` to the same variant, and it is accepted here for the same
reason: a version bump would make `journal_sqlite` refuse every existing
journal, which is strictly worse than an older reader refusing the one entry
kind only a newer embedding can write.

## Non-goals

- No validation of the value beyond its JSON type. No reserved values, no
  length bound, no character set.
- No resolution: Simulacra never looks the value up or checks that it names
  anything.
- Not exposed to spawn hooks. `SpawnBeforeContext` is unchanged; hooks can
  neither observe nor modify it.
- Not surfaced on `SpawnAck`, `ChildMetadata`, `ChildStatus`, the roster, or
  any child-control tool result. The embedding already knows what it sent.
- No change to `RestartStrategy`, budgets, capabilities, or skills.

## Assertions

### Tool definition and validation

- [x] The `spawn_agent` schema contains an optional `placement_target` string
      property; `required` remains exactly `["placement", "task", "budget"]`.
- [x] A spawn call with `placement_target: "ws-7f3a"` passes shape validation
      and the `SpawnConfig` submitted to the supervisor carries
      `placement_target == Some("ws-7f3a")`.
- [x] A spawn call without `placement_target` submits a `SpawnConfig` with
      `placement_target == None`.
- [x] A non-string `placement_target` (for example `7`) fails as
      `InvalidArguments` naming `placement_target`, and no supervisor request
      is submitted.
- [x] The value is preserved byte-for-byte: `"  ws-7f3a  "` reaches the
      `SpawnConfig` with its surrounding whitespace intact.
- [x] An empty string is a value, not an absence: `""` reaches the
      `SpawnConfig` as `Some("")`.

### Runtime forwarding

- [x] An ACP spawn whose `SpawnConfig` carries `placement_target ==
      Some("ws-7f3a")` hands the injected `AcpChildRuntime` an
      `AcpChildRequest` with `placement_target == Some("ws-7f3a")`.
- [x] An ACP spawn whose `SpawnConfig` carries `None` hands the runtime a
      request with `placement_target == None`.
- [x] `AcpChildRequest` round-trips through serde with
      `placement_target == Some(..)` intact.

### Journal

- [x] `SubAgentSpawned` journaled for a spawn with `placement_target` carries
      the value.
- [x] `SubAgentSpawned` journaled for a spawn without `placement_target`
      carries `None`.
- [x] A `SubAgentSpawned` entry serialized without the `placement_target`
      field deserializes with `placement_target == None`.
- [x] `SubAgentSpawned` round-trips through serde with
      `placement_target == Some(..)` intact.
