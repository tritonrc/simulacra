# S055 — CLI Headless JSONL Output Stream

**Status:** Active
**Crates involved:** `simulacra-cli`, `simulacra-types`, `simulacra-runtime`

## Dependencies

- **ARCHITECTURE.md** — stdout reserved for output, stderr for tracing/logs; single-binary composability
- **S013** — CLI argument parsing, headless bootstrap, output contract (S055 extends headless output)
- **S019** — Activity event protocol (`ActivityEvent` enum, `ActivitySink` trait)
- **S050** — Provider streaming → `ActivityEvent::Token` / `Think*` deltas

## Runtime dependency

S055 requires the multi-turn `AgentLoop::run` path to emit
`ActivityEvent::TurnComplete` so JSONL consumers see a per-turn boundary
before the terminal `result` line. The runtime previously passed
`emit_turn_complete = false` from the headless `run` loop (only
`run_single_turn`, used by interactive mode, emitted it). S055 flips that to
`true`. This is harmless for text headless (events go to `NoopActivitySink`)
and correct for JSONL headless.

## Context

S013 defines headless mode as a single-shot run that prints only the final
assistant message to stdout. The runtime already emits a rich
`ActivityEvent` stream (tokens, thinking, tool calls/results, child-agent
activity, workflow progress, `TurnComplete`) via the `ActivitySink` trait
(S019). Headless mode wires this sink to `NoopActivitySink` and discards it.

S055 adds a **headless output format** that surfaces the activity stream to
stdout as newline-delimited JSON (JSONL), so another program — an outer agent,
a pipeline, a script — can consume simulacra's execution in real time without
parsing terminal rendering. This makes `simulacra` usable as a sub-tool: an
orchestrator passes a `--task`, reads structured events line-by-line, and
reacts to tool calls, streaming tokens, and child-agent work as they happen.

The text output format (current behavior) is unchanged and remains the default.

## Scope

**In scope:**

- New `--output-format <text|jsonl>` CLI flag (default `text`), applicable in
  headless mode.
- JSONL line schema: an **envelope** wrapping either an `ActivityEvent` or a
  terminal `result` record.
- Streaming semantics: one JSON object per line on stdout, flushed after each
  line; tracing/log output stays on stderr.
- Terminal `result` line carrying success/failure, the final assistant message,
  turn/token counts, and the process exit code.
- Exit codes: 0 on success, 1 on agent-run failure (unchanged from text mode).

**Out of scope (v1):**

- Bidirectional approval / stdin responses. Headless mode never attaches a
  HITL runtime, so approval-gated tools are not supported in *any* headless
  mode (text or jsonl). Approval-gated runs require interactive mode (S015) or
  the API server (S031/S051). See "Approval / input" below.
- Any change to the `ActivityEvent` enum. The envelope layer keeps the
  activity protocol stable across CLI and server consumers.
- Changes to text-mode output.

## Design

```
  Orchestrator (outer agent / script)
       │  invokes:  simulacra --task "..." --output-format jsonl
       ▼
  ┌──────────────────────────────────────────────────────────┐
  │  simulacra-cli (headless, output-format = jsonl)         │
  │                                                          │
  │   bootstrap  →  AgentLoop                                │
  │                   │                                      │
  │                   │  ActivitySink = ChannelActivitySink  │
  │                   │  (instead of NoopActivitySink)       │
  │                   ▼                                      │
  │   drain mpsc receiver, for each ActivityEvent:           │
  │       write  { "kind":"activity", "event": <event> }     │
  │       flush stdout                                       │
  │                                                          │
  │   on run completion:                                     │
  │       write  { "kind":"result", ok, final_message, ... } │
  │       flush stdout                                       │
  └──────────────────────────────────────────────────────────┘
       │  stdout: JSONL (one envelope object per line)
       │  stderr: tracing/logs (same as text mode)
       ▼
  Orchestrator parses lines incrementally
```

## Behavior

### Argument parsing

- [ ] `--output-format` accepts `text` and `jsonl`. Unknown values exit with a
  clap parse error.
- [ ] Default is `text` when `--output-format` is omitted.
- [ ] `--output-format jsonl` is accepted in headless mode and routes output
  through the JSONL streamer.
- [ ] In interactive mode, `--output-format` is ignored (terminal rendering
  always wins); a `--mode interactive --output-format jsonl` invocation does
  not error but produces the normal interactive terminal output. This is
  documented, not silent: the JSONL flag is headless-only.

### Headless JSONL streaming

- [ ] When `--output-format jsonl` and mode is headless, the CLI constructs the
  `AgentLoop` with a `ChannelActivitySink` (an `mpsc::unbounded_channel` whose
  receiver is drained by the streamer) instead of `NoopActivitySink`.
- [ ] The streamer runs concurrently with `agent_loop.run(&task)`: it reads
  `ActivityEvent` values from the receiver and writes one envelope line per
  event to stdout.
- [ ] Each event is emitted in the order produced by the agent loop. No
  reordering, batching, or coalescing.
- [ ] Every line is a single compact JSON object terminated by `\n`, with no
  embedded newlines.
- [ ] Every line is flushed to stdout immediately after it is written
  (`stdout.flush()` per line) so a streaming consumer can read incrementally.
- [ ] After the agent run completes (success or failure), the receiver is
  drained of any remaining events before the terminal `result` line is written,
  so no activity line is dropped.

### Envelope line schema

- [ ] An **activity** line has the shape:
  ```json
  {"kind":"activity","event":{<ActivityEvent>}}
  ```
  where `<ActivityEvent>` is the existing `#[serde(tag="type")]` serialization
  defined in `simulacra-types::activity`. The `event` field preserves all
  variant fields unchanged.

- [ ] A **result** line has the shape (success):
  ```json
  {"kind":"result","ok":true,"final_message":"...","turns":3,"tokens":4120,"exit_code":0}
  ```
  - `ok` is `true`.
  - `final_message` is the last assistant message text (same string text mode
    prints to stdout), or `null` if the conversation has no assistant message.
  - `turns` is the number of turns the agent loop executed.
  - `tokens` is the total token usage metered for the run.
  - `exit_code` is `0`.

- [ ] A **result** line has the shape (failure):
  ```json
  {"kind":"result","ok":false,"final_message":null,"error":"budget exhausted after 50 turns","turns":50,"tokens":200000,"exit_code":1}
  ```
  - `ok` is `false`.
  - `final_message` is `null` (a failed run has no committed final answer).
  - `error` is the human-readable error string (the same `Display` text text
    mode prints to stderr).
  - `turns` and `tokens` reflect consumption up to the point of failure when
    available; if unavailable, `0`.
  - `exit_code` is `1`.

- [ ] The `result` line is always the **last** line on stdout. After it, no
  further stdout output is produced.

### Stdout / stderr separation

- [ ] In JSONL mode, **stdout contains only JSONL envelope lines**. No tracing,
  no log lines, no banners, no debug prints.
- [ ] Tracing output (from `--verbose` / `--otlp-endpoint`) goes to stderr,
  unchanged from text mode.
- [ ] A consumer can feed stdout line-by-line to a JSON parser and never see a
  non-JSON line (other than, on a bootstrap failure before JSONL output begins,
  the existing text error on stderr).

### Exit codes

- [ ] A successful JSONL run exits with code `0` and the `result` line has
  `exit_code: 0`.
- [ ] A failed agent run (provider error, budget exhaustion, etc.) emits the
  `result` line with `ok: false` and exits with code `1`.
- [ ] Bootstrap failures (config parse error, missing API key, etc.) that occur
  before any JSONL output is produced behave exactly as text mode: the error is
  printed to stderr and the process exits `1` with **no** stdout output. (The
  JSONL streamer only starts after bootstrap + provider build succeed.)

### Streaming tokens and tools

- [ ] Provider text deltas are emitted as `activity` lines wrapping
  `ActivityEvent::Token` in provider order, one per delta (per S050).
- [ ] Extended thinking is emitted as `ThinkStart` / `ThinkDelta` / `ThinkEnd`
  activity lines (per S050).
- [ ] Tool execution emits `ToolStart`, zero or more `ToolOutput`, then
  `ToolFinish` activity lines, in order.
- [ ] Child-agent work is emitted as `ChildSpawned`, nested `ChildActivity`
  (recursively serialized), and `ChildFinished` activity lines.
- [ ] Workflow runs emit their `Workflow*` activity lines unchanged.

### Turn boundary

- [ ] The runtime's `ActivityEvent::TurnComplete` is emitted as a normal
  activity line. It is **not** the terminator; the `result` line is the
  terminator. A consumer must not treat `TurnComplete` as end-of-stream.

### Approval / input

- [ ] Headless JSONL mode never attaches a HITL runtime, mirroring text
  headless mode. Tools auto-run with no approval gate.
- [ ] If a governance hook (S026) or future feature causes the runtime to emit
  `ActivityEvent::ToolApprovalRequired` or `ActivityEvent::InputRequired`, the
  event is serialized as a normal activity line, but no responder exists and
  the run will block. This is a **documented limitation** of headless mode
  (shared with text mode), not a JSONL-specific behavior. The spec notes that
  approval-gated runs require interactive mode or the API server.

### Composability

- [ ] `simulacra --task "x" --output-format jsonl | head -n 1` produces exactly
  one valid JSON envelope object on the first line of stdout.
- [ ] `cat out.jsonl | jq 'select(.kind=="activity") | .event.type'` succeeds
  for every line of a JSONL run's stdout (every line is valid JSON).
- [ ] Piping JSONL stdout to a consumer that closes its stdin early (e.g.
  `head`) does not crash the producer ungracefully: the agent loop and streamer
  shut down on the next write/flush error without panicking.

## Observability (see S010 for conventions)

- [ ] No new span or metric attributes are required for S055. The CLI root span
  (`simulacra.operation.name = cli_run`) records the output format as a span
  attribute `simulacra.cli.output_format` (`text` | `jsonl`) so operators can
  distinguish JSONL runs in traces.

## Non-goals

- S055 does not define a request/response protocol. Input is still a single
  `--task` string. Multi-turn, approval, and steering over JSONL are deferred
  to the API server (S031) or a future spec.
- S055 does not change the activity event set. New event types belong in S019.
