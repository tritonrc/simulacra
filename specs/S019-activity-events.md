# S019 — Activity Events

**Status:** Active
**Crates involved:** `simulacra-runtime`, `simulacra-cli`, `simulacra-types`

## Dependencies

- **S007** — Provider (streaming token delivery, extended thinking)
- **S009** — Supervisor (child agent lifecycle)
- **S012** — Built-in tools (tool registry, tool execution)
- **S015** — Interactive mode (REPL rendering, status line)
- **S018** — Interactive sub-agents (spawn_agent tool, synchronous delegation)
- **S050** — Agent streaming runtime (provider stream events to activity events)

## Context

Today, when an agent executes — whether it is the parent running tools or a child agent spawned via `spawn_agent` — the user sees nothing until the operation completes. The LLM streams tokens, but tool calls and child agent work happen in a black box. Claude Code solved this with **collapsible activity blocks**: a header line showing the operation, a scrolling tail of recent output lines, a rolling count of hidden lines, and a summary line on completion with stats and an expand affordance.

This spec defines an **activity event** protocol that makes agent work observable in real time. It applies uniformly to tool execution, sub-agent delegation, and model thinking. The same event stream powers the interactive CLI rendering and will later power the server-side SSE protocol. The spec defines event shapes, producer/consumer contracts, and the rendering model.

## Design

```text
AgentLoop (parent or child)
   |
   |  emits ActivityEvent values via an ActivitySink
   |
   +--> Provider response
   |      ThinkStart                     → sink.emit(ThinkStart)
   |      ThinkDelta { text }            → sink.emit(ThinkDelta)  (streaming)
   |      ThinkEnd { duration, tokens }  → sink.emit(ThinkEnd)
   |      Token("Analyzing...")          → sink.emit(Token)
   |
   +--> Tool call
   |      ToolApprovalRequired { ... }   → sink.emit(ToolApprovalRequired)
   |      ToolStart { name, args }       → sink.emit(ToolStart)
   |      ToolOutput { line }            → sink.emit(ToolOutput)  (streaming)
   |      ToolFinish { name, stats }     → sink.emit(ToolFinish)
   |
   +--> Human input
   |      InputRequired { prompt }       → sink.emit(InputRequired)
   |
   +--> Sub-agent (child AgentLoop)
          ChildSpawned { id, type, task } → sink.emit(ChildSpawned)
          ChildActivity { id, event }     → sink.emit(ChildActivity)  (recursive)
          ChildFinished { id, stats }     → sink.emit(ChildFinished)

InteractiveSession / Server handler
   |
   |  consumes ActivityEvent from a broadcast channel
   |
   +--> renders collapsible activity blocks (CLI)
   +--> serializes to SSE frames (server, future spec)
```

The key insight: a child agent's `ActivitySink` forwards events to the parent's sink, wrapped in `ChildActivity`. This gives the consumer a nested event tree rooted at the parent, with full visibility into every layer of delegation.

## Rendering Model

The rendering model is inspired by Claude Code's activity display. Three states:

### In-progress (spinner active)

```
● Bash(cargo test -p simulacra-runtime --test s018...)
└    Compiling simulacra-runtime v0.1.0 (...)
         Finished `test` profile in 1.15s
         Running tests/s009_supervisor_red.rs (target/...)
  ... +73 lines
└  (timeout 5m)
```

- **Header line**: spinner indicator + operation name + argument summary (truncated)
- **Tail window**: last N lines of output (default: 3), scrolling as new lines arrive
- **Hidden count**: `... +{count} lines` when total output exceeds the tail window
- **Metadata line**: timeout, budget, or other operation-specific metadata

### Completed (success)

```
● Explore(Research streaming architecture)
└  Done (21 tool uses · 83.5k tokens · 46s)
```

- **Header line**: success indicator (filled dot) + operation name + argument summary
- **Summary line**: operation stats — varies by operation type:
  - **Tool**: exit code · duration
  - **Agent**: tool uses · token count · duration
  - **Thinking**: duration · token count · think time

### Thinking (model reasoning)

```
+ Tinkering... (2m 20s · ↓ 3.2k tokens · thought for 8s)
```

- **Single line**: thinking indicator (+) + thinking label + stats
- **Stats**: elapsed time · tokens received so far · thinking duration
- **Updates in place**: the line is rewritten as stats change
- When thinking finishes, the line remains as a static summary

## Behavior

### ActivityEvent enum

1. The `ActivityEvent` enum MUST define the following variants:

```rust
pub enum ActivityEvent {
    /// LLM token arrived (streaming response text).
    Token { text: String },

    /// Model has started an extended thinking block.
    ThinkStart,

    /// A chunk of thinking text arrived (streaming).
    ThinkDelta { text: String },

    /// Model thinking block has ended.
    ThinkEnd {
        /// Thinking duration in milliseconds.
        think_duration_ms: u64,
        /// Approximate token count of thinking content.
        think_tokens: u64,
    },

    /// A tool call has started.
    ToolStart {
        tool_call_id: String,
        name: String,
        /// Full arguments. Display layer truncates for rendering.
        arguments: serde_json::Value,
    },

    /// A tool call is waiting for human approval before execution starts.
    ToolApprovalRequired {
        tool_call_id: String,
        name: String,
        arguments: serde_json::Value,
        reason: Option<String>,
    },

    /// A provider streamed part of a tool-call argument payload.
    ///
    /// This is observational only. It does not mean the tool has started.
    ToolCallDelta {
        index: u64,
        tool_call_id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },

    /// A line of output from a running tool (e.g. shell stdout/stderr).
    ToolOutput {
        tool_call_id: String,
        line: String,
    },

    /// The agent is waiting for user-provided input.
    InputRequired {
        prompt: String,
        schema: Option<serde_json::Value>,
    },

    /// A tool call has finished.
    ToolFinish {
        tool_call_id: String,
        name: String,
        is_error: bool,
        duration_ms: u64,
        /// Optional exit code (for shell tools).
        exit_code: Option<i32>,
    },

    /// A child agent has been spawned.
    ChildSpawned {
        child_id: String,
        agent_type: String,
        task: String,
    },

    /// A forwarded event from a running child agent.
    ChildActivity {
        child_id: String,
        agent_type: String,
        event: Box<ActivityEvent>,
    },

    /// A child agent has finished.
    ChildFinished {
        child_id: String,
        agent_type: String,
        exit_reason: String,
        duration_ms: u64,
        tool_uses: u32,
        token_count: u64,
    },

    /// The agent turn has completed.
    TurnComplete,
}
```

2. `ActivityEvent` MUST implement `Clone`, `Send`, `Serialize`, `Deserialize`, and be `'static`.
3. All string fields MUST be owned (`String`, not `&str`) for channel safety.
4. `ChildActivity` nests recursively: a grandchild's events arrive as `ChildActivity { event: Box(ChildActivity { event: Box(ToolStart { ... }) }) }`.

### ActivitySink trait

5. An `ActivitySink` trait MUST be defined:

```rust
pub trait ActivitySink: Send + Sync + 'static {
    fn emit(&self, event: ActivityEvent);
}
```

6. `ActivitySink::emit` MUST be non-blocking. Implementations buffer or drop events rather than blocking the agent loop.
7. A `NoopActivitySink` MUST exist for headless mode and tests where no consumer is listening.
8. A `ChannelActivitySink` MUST exist that sends events through a `tokio::sync::mpsc::UnboundedSender<ActivityEvent>`.
9. `emit()` uses `UnboundedSender::send()` which never drops events and never blocks. The unbounded channel is safe because the consumer (CLI renderer) processes events faster than they are produced, and the event stream is bounded by LLM response speed. Future multi-consumer scenarios (SSE server) will add their own subscription mechanism.

### AgentLoop integration

10. `AgentLoop::new()` MUST accept an optional `Arc<dyn ActivitySink>`. If not provided, a `NoopActivitySink` is used.
11. When the provider streams tokens, each token chunk MUST be emitted as `ActivityEvent::Token { text }`.
12. When the provider's streaming response includes an extended thinking block (as defined by the provider's content block types), the agent loop MUST emit `ThinkStart` at the start of the thinking content block.
13. Thinking content chunks from the provider stream MUST be emitted as `ThinkDelta { text }` as they arrive.
14. When the thinking content block ends in the provider stream, the agent loop MUST emit `ThinkEnd`. The `think_duration_ms` is measured by the agent loop from `ThinkStart` to `ThinkEnd`. The `think_tokens` is the approximate character-to-token count of accumulated `ThinkDelta` text (divide character count by 4).
15. When the provider streams tool-call argument chunks, each chunk MUST be emitted as `ToolCallDelta { index, tool_call_id, name, arguments_delta }`. The event is display-only and MUST NOT be journaled.
16. When tool approval is enabled, the agent loop MUST emit `ToolApprovalRequired` before `ToolStart`.
17. Before executing a tool call, the agent loop MUST emit `ToolStart` with the tool's name, call ID, and full parsed arguments.
18. `request_input` MUST emit `InputRequired` before waiting for `input.response`.
19. After a tool call completes, the agent loop MUST emit `ToolFinish` with the tool name, call ID, error status, duration, and optional exit code.
20. The agent loop MUST emit `ToolOutput` for each line of streaming output from a tool (e.g. shell stdout/stderr). The agent loop — not the tool implementation — owns the sink. Tools return output through existing channels (e.g. streaming stdout); the agent loop captures it and emits `ToolOutput` events.
21. The agent loop MUST emit `TurnComplete` when `run_single_turn()` returns.

### Boundary with S015 (approval, retry, cancellation)

21a. Activity events model server-side HITL wait points introduced by S051:
`ToolApprovalRequired` is emitted before a gated tool starts, and
`InputRequired` is emitted before `request_input` waits. CLI-specific approval
rendering remains owned by S015. Retry indicators (S015 item 40) are rendered
by the interactive layer based on provider errors, not activity events.

### Sub-agent activity forwarding

19. When `CliTaskFactory` creates a child `AgentLoop`, it MUST provide a `ForwardingActivitySink` that wraps each child event in `ChildActivity` and forwards it to the parent's sink.
20. Before spawning a child, `SpawnAgentTool` MUST emit `ChildSpawned` via the parent's sink.
21. When a child finishes, the supervisor MUST emit `ChildFinished` via the parent's sink, with aggregated stats (tool uses, token count, duration).
22. The forwarding sink MUST NOT buffer — child events are forwarded immediately for real-time rendering.

### Interactive CLI rendering — activity blocks

23. Each `ToolStart`, `ChildSpawned`, or `ThinkStart` event opens an **activity block** in the terminal.
24. An activity block has three rendering phases: **in-progress**, **completed**, and **collapsed**.

**In-progress phase:**

25. The header line MUST show: a spinner indicator, the operation name, and a truncated argument/task summary.
26. The **tail window** MUST show the most recent N output lines (default: 3). As new `ToolOutput` or `ChildActivity` lines arrive, older lines scroll out of the tail window.
27. When total output exceeds the tail window size, a `... +{hidden_count} lines` indicator MUST appear above the tail window. The count updates as new lines arrive.
28. While an activity block is in-progress, the spinner indicator MUST animate to signal active work.

**Completed phase:**

29. When a `ToolFinish` or `ChildFinished` event arrives, the activity block transitions to completed.
30. The header indicator MUST change from a spinner to a static indicator: filled dot for success, X for error.
31. A summary line MUST appear beneath the header showing operation-specific stats:
    - **Tool**: `Done · {duration}` or `Error (exit code {N}) · {duration}`
    - **Agent**: `Done ({tool_uses} tool uses · {token_count} tokens · {duration})`
    - **Thinking**: `{duration} · ↓ {token_count} tokens · thought for {think_duration}`
32. The tail window and hidden count are replaced by the summary line.

**Thinking blocks:**

33. `ThinkStart` opens a single-line activity block with a thinking indicator (+) and a label (e.g. "Thinking...").
34. While thinking is in progress, the line MUST update in-place showing: elapsed time, tokens received so far, and think duration.
35. `ThinkEnd` finalizes the line to a static summary.
36. Thinking content (`ThinkDelta`) is NOT rendered — it is available in the event stream for server consumers and logging, but the CLI does not display thinking text.

### Server-side event serialization (future)

37. `ActivityEvent` MUST be serializable to JSON via `serde::Serialize`. The JSON shape is the wire format for SSE streaming to API consumers.
38. The server handler (future spec) subscribes to the same broadcast channel as the CLI and serializes each event as an SSE `data:` frame.

### Observability

39. `ToolStart` events MUST be correlated with the `tool_invoke` span from S012 via `tool_call_id`.
40. `ChildSpawned` events MUST be correlated with the `create_agent` span from S009 via `child_id`.
41. Tool duration in `ToolFinish` MUST match the `tool_invoke` span duration within reasonable precision (< 50ms drift).

## Assertions

### ActivityEvent type

- [x] `ActivityEvent` defines all variants: `Token`, `ThinkStart`, `ThinkDelta`, `ThinkEnd`, `ToolStart`, `ToolApprovalRequired`, `ToolCallDelta`, `ToolOutput`, `InputRequired`, `ToolFinish`, `ChildSpawned`, `ChildActivity`, `ChildFinished`, `TurnComplete`. **All variants defined in `simulacra-types/src/activity.rs`.**
- [x] `ActivityEvent` implements `Clone + Send + 'static`. **`#[derive(Debug, Clone, Serialize, Deserialize)]` on the enum; all fields are owned types (`String`, `Box`, `serde_json::Value`).**
- [x] `ActivityEvent` implements `Serialize + Deserialize`. **`#[derive(Serialize, Deserialize)]` with `#[serde(tag = "type")]` for tagged JSON.**
- [x] All string fields are owned `String`, not borrowed. **Every string field in every variant is `String`, not `&str`.**
- [x] `ChildActivity.event` is `Box<ActivityEvent>` for recursive nesting. **`event: Box<ActivityEvent>` in the `ChildActivity` variant.**
- [x] JSON round-trip preserves all fields including nested `ChildActivity`. **`json_round_trip_nested_child_activity` test in `activity.rs` serializes doubly-nested `ChildActivity` and asserts `json == json2`.**

### ActivitySink trait

- [x] `ActivitySink` trait is object-safe with a single `emit(&self, ActivityEvent)` method. **Trait defined in `simulacra-runtime/src/activity_sink.rs` with `fn emit(&self, event: ActivityEvent)`; used as `Arc<dyn ActivitySink>`.**
- [x] `NoopActivitySink` exists and discards all events. **`NoopActivitySink` struct with empty `emit()` body.**
- [x] `ChannelActivitySink` sends events through a `tokio::sync::mpsc::UnboundedSender`. **`ChannelActivitySink` wraps `UnboundedSender<ActivityEvent>` and calls `self.sender.send(event)`.**
- [x] `emit()` never blocks the caller and never drops events. **Uses `UnboundedSender::send()` which is non-blocking; failure (receiver dropped) is silently ignored with `let _ =`.**

### AgentLoop integration

- [x] `AgentLoop::new()` accepts an optional `Arc<dyn ActivitySink>`. **`activity_sink: Option<Arc<dyn ActivitySink>>` parameter; defaults to `NoopActivitySink` when `None`.**
- [x] Provider streaming tokens emit `Token` events via the sink. **S050 test `streaming_provider_tokens_emit_in_order_and_final_response_is_journaled_once` verifies provider-delta emission.**
- [x] Extended thinking emits `ThinkStart`, `ThinkDelta` (streaming), and `ThinkEnd` with duration and token count.
- [x] Tool call start emits `ToolStart` with name, call ID, and arguments. **`self.sink.emit(ActivityEvent::ToolStart { tool_call_id, name, arguments })` before tool execution in `run_single_turn()`.**
- [x] Tool approval waits emit `ToolApprovalRequired` before `ToolStart`. **S051 test `tool_approval_required_emits_before_tool_start_and_approval_executes` verifies approval event ordering.**
- [x] Tool-call input streaming emits `ToolCallDelta` before `ToolStart`. **S050 test `provider_tool_call_deltas_map_to_activity_events_without_partial_journal_entries` verifies provider delta emission before any tool execution event.**
- [x] Human input waits emit `InputRequired`. **S051 test `request_input_tool_waits_for_input_response_and_journals_tool_result` verifies request_input emits and waits.**
- [x] Tool call completion emits `ToolFinish` with name, call ID, error status, duration, and optional exit code. **`self.sink.emit(ActivityEvent::ToolFinish { tool_call_id, name, is_error, duration_ms, exit_code: None })` after tool execution.**
- [x] `run_single_turn()` emits `TurnComplete` on return. **`self.sink.emit(ActivityEvent::TurnComplete)` on both the Complete and ToolCallsProcessed return paths.**

### Sub-agent forwarding

- [x] Child `AgentLoop` receives a `ForwardingActivitySink` that wraps events in `ChildActivity`. **`AgentTaskFactory::create_task()` creates `ForwardingActivitySink::new(child_id, agent_type, parent_sink)` and passes it to the child `AgentLoop`.**
- [x] `SpawnAgentTool` emits `ChildSpawned` before the child starts. **`self.activity_sink.emit(ActivityEvent::ChildSpawned { child_id, agent_type, task })` in `SpawnAgentTool::call()` before sending the spawn message.**
- [x] Supervisor emits `ChildFinished` with aggregated stats after child completion/failure. **`self.activity_sink.emit(ActivityEvent::ChildFinished { child_id, agent_type, exit_reason, duration_ms, tool_uses, token_count })` after `result_rx.await` in `SpawnAgentTool::call()`.**
- [x] Nested children produce doubly-wrapped `ChildActivity` events. **`ForwardingActivitySink::emit()` wraps in `ChildActivity { event: Box::new(event) }`; a grandchild's ForwardingActivitySink wraps again, producing double nesting.**

### Interactive CLI rendering — activity blocks

- [x] `ToolStart` opens an activity block with spinner + tool name + argument summary. **`ActivityBlockRenderer::process_event()` creates an `ActivityBlock` with kind "tool" and renders header via `render_block_header()` with spinner char.**
- [x] `ToolOutput` lines appear in the tail window (last 3 lines visible). **`block.push_output()` adds to `tail_lines` with `TAIL_WINDOW_SIZE = 3`; excess lines are removed from front.**
- [x] When output exceeds tail window, `... +N lines` indicator appears and updates. **`render_block_body()` outputs `"  ... +{hidden} lines"` when `block.hidden_count() > 0`.**
- [x] `ToolFinish` transitions block to completed: static indicator + summary line. **Sets `block.completed = true` and `completion_summary`; `render_block_completed()` shows `"●"` or `"✗"` + summary.**
- [x] `ChildSpawned` opens an activity block with spinner + agent type + task summary. **Creates `ActivityBlock` with kind "agent", name = `agent_type`, summary = truncated task.**
- [x] `ChildActivity` events render within the child's activity block using the same rules recursively. **`process_event()` recurses on the inner event and adds indented output to the child block's tail.**
- [x] `ChildFinished` transitions block to completed with stats: tool uses, token count, duration. **Summary: `"Done ({tool_uses} tool uses · {token_count} tokens · {duration_ms}ms)"`.**
- [x] `ThinkStart` opens a single-line block with thinking indicator and label. **Creates "thinking" block and returns `vec!["+ Thinking..."]`.**
- [x] Thinking line updates in-place with elapsed time, token count, and think duration. **`render_thinking_progress()` method exists showing elapsed, tokens, and think duration.**
- [x] `ThinkEnd` finalizes thinking line to static summary. **Returns `"+ {elapsed}s · ↓ {think_tokens} tokens · thought for {think_duration_ms}ms"`.**
- [x] Thinking content (`ThinkDelta`) is not rendered in the CLI. **`ThinkDelta` match arm returns `vec![]` with comment "Not rendered in CLI".**
- [x] Spinner animates while activity block is in-progress. **`next_spinner()` cycles through `SPINNER_FRAMES` braille characters on each header render.**
- [x] Error tool results show error indicator (X) instead of success indicator. **`render_block_completed()` uses `"✗"` when `block.is_error`, `"●"` otherwise.**

### Observability

- [x] `ToolStart.tool_call_id` matches the `tool_invoke` span's tool call ID from S012. **Both use `tc.id` — `ToolStart` emitted with `tc.id.clone()` and `ToolRegistry::call()` span uses the same tool call.**
- [x] `ChildSpawned.child_id` matches the `create_agent` span's agent ID from S009. **`SpawnAgentTool::call()` generates `child_id` and uses it for both `ChildSpawned` event and `SpawnConfig.agent_id`.**
- [x] `ToolFinish.duration_ms` is within 50ms of the `tool_invoke` span duration. **`tool_start.elapsed().as_millis()` measured immediately around the same `execute_tool_live` call that the span wraps.**
