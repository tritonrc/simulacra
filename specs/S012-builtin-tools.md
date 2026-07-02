# S012 — Built-in Tools

**Status:** Active
**Crate:** `simulacra-tool`
**Priority:** Phase 1 required

## Context

The agent loop dispatches LLM tool calls to a `ToolRegistry`. The LLM sees
directly exposed tool definitions (name, description, JSON Schema for input) and
emits `ToolCall` messages. The registry looks up the tool by name and invokes
it. Built-in tools are the bridge between what the LLM asks for and what the
sandbox can do.

Each built-in tool delegates side effects to `AgentCell`, which enforces the
Golden Rule. The tool itself does not bypass capabilities, budgets, journaling,
or VFS mediation. The tool's job is: parse input, call `AgentCell`, and format a
typed tool output.

## Design

```
  LLM response: tool_use("file_read", {"path": "/workspace/foo.rs"})
       │
       ▼
  ToolRegistry::call("file_read", args, capability)
       │
       ▼
  FileReadTool::call(args, capability)
       │  parse args → extract path
       │  delegate to AgentCell::read_file(path)
       │  format result as ToolOutput
       ▼
  ToolOutput { content: "...", is_error: false, ... }
```

## Tool Output Contract

Tools return a typed output value with:

- `content`: model-visible string content.
- `is_error`: authoritative tool-result error flag.
- `log_preview`: bounded telemetry/log preview string.
- `structured`: optional structured JSON payload for internal consumers.
- `hook_input`: optional stable pre-hook payload.
- `hook_output`: optional stable post-hook payload.

Provider wire output remains compatible with the current conversation model:
the agent loop still emits `Message { role: Tool, content, tool_call_id }`.
The loop uses `ToolOutput.is_error` as authoritative. It must not infer tool
errors merely because a structured payload contains an `"error"` field.

## Tool Registration Metadata

Each registered tool has an exposure:

- `Direct`: visible in `ToolRegistry::definitions()` and callable.
- `Hidden`: callable by dispatch but omitted from model-visible definitions.
- `Deferred`: omitted from initial definitions, callable by dispatch, and
  discoverable through registry search.

The registry rejects duplicate names deterministically at registration time.
Tool metadata also carries optional `output_schema`, `supports_parallel_tool_calls`,
and `waits_for_runtime_cancellation`. The conservative defaults are no output
schema, serial execution, and no runtime-cancellation wait.

## Schema Helpers

`simulacra-tool` provides typed JSON Schema helper builders for common
object/string/number/boolean schemas. Tool implementations can use these
helpers while provider adapters continue to receive plain JSON Schema values.

## Built-in Tools

### `file_read`

**Description:** Read the contents of a file at the given path.

**Input schema:** object with required string `path`.

**Output:** File contents as a string. If the file does not exist, returns
`ToolOutput { is_error: true, .. }` with a descriptive message.

**Delegates to:** `AgentCell::read_file(path)`

### `file_write`

**Description:** Write content to a file, creating parent directories as needed.

**Input schema:** object with required string `path` and required string
`content`.

**Output:** Confirmation message with the path and bytes written. On failure,
returns `ToolError` for delegated sandbox failures.

**Delegates to:** `AgentCell::write_file(path, content.as_bytes())`

### `apply_patch`

**Description:** Apply a Simulacra-style patch to the VFS.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "patch": { "type": "string", "description": "Patch text using the Simulacra patch grammar" }
  },
  "required": ["patch"]
}
```

**Grammar:**

```text
*** Begin Patch
*** Add File: <path>
+<line>
*** Delete File: <path>
*** Update File: <path>
*** Move to: <path>
@@
 <context line>
-<removed line>
+<added line>
*** End Patch
```

`apply_patch` supports add, delete, update, and move. Moves require read
capability for the source path and write capability for both source and
destination paths, because the source bytes become available at the destination.
It verifies update hunks against current VFS content before writing and fails
cleanly on malformed patches, stale hunks, missing files, existing add targets,
denied paths, and budget failures. A failing patch must not partially apply
later file changes.
Rollback is defined over final VFS state for paths whose contents still match
this batch's expected effects; observer-visible transactional VFS events require
a VFS transaction primitive and are out of scope for S012.
All reads, writes, deletes, and moves go through `AgentCell`.

**Delegates to:** `AgentCell::read_file`, `AgentCell::write_file`, and
AgentCell-mediated delete/move operations.

### `shell_exec`

**Description:** Execute a shell command in the sandbox shell and return
structured output.

**Input schema:** object with required string `command` and optional `workdir`,
`yield_time_ms`, and `max_output_tokens`.

`workdir` sets the command's working directory for this call without creating a
persistent shell session. `yield_time_ms` is accepted for future ergonomics but
does not create streaming or background sessions. `max_output_tokens` bounds the
returned stdout/stderr content using centralized output truncation.

**Output:** Structured JSON with `stdout`, `stderr`, `exit_code`, `truncated`,
`stdout_original_len`, `stderr_original_len`, `stdout_truncated_len`, and
`stderr_truncated_len`. Non-zero exit codes are successful tool outputs with
`is_error: false`. Persistent-session parameters and stdin continuation are not
supported until Simulacra has persistent shell sessions.

**Delegates to:** `AgentCell::execute_shell(command)` using the sandbox shell.

### `js_exec`

**Description:** Execute JavaScript code in the QuickJS runtime and return the
result.

**Input schema:** object with required string `code`.

**Output:** The string result of JS execution (stdout capture + return value).
On JS exception, returns `ToolOutput { is_error: true, .. }` with the exception
message and stack trace.

**Delegates to:** `AgentCell::execute_js(code)`

### `list_dir`

**Description:** List the contents of a directory.

**Input schema:** object with required string `path`.

**Output:** Newline-separated list of entries, with directory entries suffixed
by `/`. On failure (path not found, not a directory), returns
`ToolOutput { is_error: true, .. }`.

**Delegates to:** `AgentCell::list_dir(path)` and mediated metadata reads for
directory suffixes.

## Behavior

1. Each built-in tool implements the `Tool` trait from `simulacra-types`.
2. Tool input is validated before delegation. Missing required fields return
   `ToolError::InvalidArguments`.
3. `SandboxError::CapabilityDenied` from `AgentCell` is converted to
   `ToolError::CapabilityDenied`.
4. `SandboxError::BudgetExhausted` from `AgentCell` is converted to
   `ToolError::ExecutionFailed` with a descriptive message including the
   exhausted resource.
5. Other `SandboxError` variants are converted to `ToolError::ExecutionFailed`.
6. Non-zero shell exit codes are returned as successful tool outputs, not
   `ToolError` and not `ToolOutput.is_error`.
7. `file_edit` is removed. Calling `file_edit` returns unknown-tool behavior.
8. All direct built-in tools are registered by
   `register_builtins(registry: &mut ToolRegistry, cell: Arc<AgentCell>)
   -> Result<(), ToolError>`.
9. Hook payloads are stable per tool. Generic registry hooks use those payloads,
   and tools that own their hook lifecycle must not double-fire generic hooks.
10. Output truncation and telemetry preview are centralized and shared across
    registry events and structured tool outputs.

## Assertions

### Tool output and runtime handling

- [x] Built-in tools return typed `ToolOutput` values with model-visible
  content, explicit `is_error`, log preview, and optional structured JSON.
- [x] The agent loop journals and surfaces `ToolOutput.is_error` as
  authoritative.
- [x] A structured payload containing an `"error"` field with `is_error: false`
  is not surfaced as a tool error.
- [x] `is_error: true` from built-in tools, skill tools, and memory tools is
  journaled and surfaced as an error.

### Tool registration

- [x] `register_builtins` registers exactly 6 direct tools: `file_read`,
  `file_write`, `apply_patch`, `shell_exec`, `js_exec`, `list_dir`.
- [x] `ToolRegistry::definitions()` after `register_builtins` returns 6 direct
  definitions with correct names and descriptions.
- [x] `file_edit` is not registered, and calling `file_edit` returns unknown-tool
  behavior.
- [x] Duplicate tool registration fails deterministically.
- [x] Hidden tools are callable but omitted from model-visible definitions.
- [x] Deferred tools are omitted from initial definitions and discoverable
  through registry search.
- [x] Each tool definition has a valid JSON Schema as `input_schema`.
- [x] Optional output schema metadata can be stored internally without requiring
  provider adapters to send it.
- [x] Per-tool `supports_parallel_tool_calls` and
  `waits_for_runtime_cancellation` metadata defaults to conservative false
  values and can be overridden.

### file_read

- [x] `file_read` with a path that exists returns the file content.
- [x] `file_read` with a path that does not exist returns
  `ToolOutput { is_error: true, .. }` with "not found" in the message.
- [x] `file_read` without `path` argument returns
  `ToolError::InvalidArguments`.

### file_write

- [x] `file_write` writes content and returns confirmation with byte count.
- [x] `file_write` to a nested path creates parent directories.
- [x] `file_write` without `content` argument returns
  `ToolError::InvalidArguments`.

### apply_patch

- [x] `apply_patch` adds a new file.
- [x] `apply_patch` updates an existing file when context matches.
- [x] `apply_patch` deletes an existing file.
- [x] `apply_patch` moves a file when read capability allows the source and
  write capability allows both paths.
- [x] `apply_patch` rejects malformed patches with
  `ToolOutput { is_error: true, .. }`.
- [x] `apply_patch` rejects stale or mismatched hunks without changing the file.
- [x] `apply_patch` rejects denied paths through `ToolError::CapabilityDenied`.
- [x] `apply_patch` rejects budget failures through `ToolError::ExecutionFailed`.
- [x] `apply_patch` leaves no partial final-state changes from its own batch
  when any file operation in a patch fails.

### shell_exec

- [x] `shell_exec("echo hello")` returns structured stdout, stderr, and
  exit code.
- [x] `shell_exec("nonexistent_command")` returns a result with non-zero exit
  code, not a `ToolError`.
- [x] `shell_exec` without `command` argument returns
  `ToolError::InvalidArguments`.
- [x] `shell_exec` honors optional `workdir` for relative paths in the command.
- [x] `shell_exec` returns output truncation metadata when `max_output_tokens`
  bounds stdout or stderr.
- [x] `shell_exec` accepts `yield_time_ms` without creating a persistent session.
- [x] Unsupported persistent-session or stdin-continuation arguments return
  `ToolError::InvalidArguments`.

### js_exec

- [x] `js_exec("1 + 1")` returns the string result.
- [x] `js_exec` with a syntax error returns
  `ToolOutput { is_error: true }` with the error message.
- [x] `js_exec` without `code` argument returns `ToolError::InvalidArguments`.

### list_dir

- [x] `list_dir("/")` returns entries in the root directory.
- [x] `list_dir` on a non-existent path returns `ToolOutput { is_error: true }`.
- [x] Directory entries are suffixed with `/` in the output.

### Hook payloads and truncation

- [x] Hook input rewrite uses stable per-tool payloads and does not double-fire
  for tools owning hooks.
- [x] Tool result telemetry uses centralized preview truncation and preserves
  original length metadata.

### Error mapping

- [x] `AgentCell` capability denial surfaces as `ToolError::CapabilityDenied`
  through the tool.
- [x] `AgentCell` budget exhaustion surfaces as `ToolError::ExecutionFailed`
  with resource details.
- [x] VFS errors from `AgentCell` surface as `ToolError::ExecutionFailed` with
  path and error message.

## Observability (see S010 for conventions)

- [x] Each tool invocation produces a span with `gen_ai.tool.name` = the tool
  name (per OTel GenAI conventions).
- [x] Tool invocation spans are children of the agent turn span.
- [x] Tool errors (`ToolError`, not `ToolOutput.is_error` results) are logged at
  `ERROR` level with the tool name and error message.
- [x] Tool results are captured as events on the tool span (per
  `gen_ai.tool.message` convention).
