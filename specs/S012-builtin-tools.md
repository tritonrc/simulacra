# S012 — Built-in Tools

**Status:** Active
**Crate:** `simulacra-tool`
**Priority:** Phase 1 required

## Context

The agent loop dispatches LLM tool calls to a `ToolRegistry`. The LLM sees tool definitions (name, description, JSON Schema for input) and emits `ToolCall` messages. The registry looks up the tool by name and invokes it. Built-in tools are the bridge between what the LLM asks for and what the sandbox can do.

Each built-in tool delegates to `AgentCell`, which enforces the Golden Rule. The tool itself does NOT check capabilities, budgets, or write journal entries — that is `AgentCell`'s job (see S011). The tool's job is: parse input, call `AgentCell`, format output.

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
       │  format result as JSON
       ▼
  ToolResult { content: "...", is_error: false }
```

## Tool Definitions

### `file_read`

**Description:** Read the contents of a file at the given path.

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Absolute path to the file to read" }
  },
  "required": ["path"]
}
```

**Output:** File contents as a string. If the file does not exist, returns an error result with a descriptive message.

**Delegates to:** `AgentCell::read_file(path)`

### `file_write`

**Description:** Write content to a file, creating parent directories as needed.

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Absolute path to the file to write" },
    "content": { "type": "string", "description": "Content to write to the file" }
  },
  "required": ["path", "content"]
}
```

**Output:** Confirmation message with the path and bytes written. On failure, error result.

**Delegates to:** `AgentCell::write_file(path, content.as_bytes())`

### `file_edit`

**Description:** Apply a search-and-replace edit to an existing file.

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Absolute path to the file to edit" },
    "old_string": { "type": "string", "description": "Exact text to find in the file" },
    "new_string": { "type": "string", "description": "Text to replace old_string with" }
  },
  "required": ["path", "old_string", "new_string"]
}
```

**Behavior:** Reads the file, verifies `old_string` appears exactly once, replaces it with `new_string`, writes the file back. If `old_string` is not found or appears more than once, returns an error result (not a SandboxError — the tool handles this as a user-facing error).

**Delegates to:** `AgentCell::read_file(path)` then `AgentCell::write_file(path, modified_content)`

### `shell_exec`

**Description:** Execute a shell command and return stdout, stderr, and exit code.

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "command": { "type": "string", "description": "Shell command to execute" }
  },
  "required": ["command"]
}
```

**Output:** JSON object with `stdout`, `stderr`, and `exit_code` fields. Non-zero exit code is NOT treated as a tool error — it is a normal result the LLM can interpret.

**Delegates to:** `AgentCell::execute_shell(command)`

### `js_exec`

**Description:** Execute JavaScript code in the QuickJS runtime and return the result.

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "code": { "type": "string", "description": "JavaScript code to execute (ESM)" }
  },
  "required": ["code"]
}
```

**Output:** The string result of the JS execution (stdout capture + return value). On JS exception, returns an error result with the exception message and stack trace.

**Delegates to:** `AgentCell::execute_js(code)`

### `list_dir`

**Description:** List the contents of a directory.

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Absolute path to the directory to list" }
  },
  "required": ["path"]
}
```

**Output:** Newline-separated list of entries, with directory entries suffixed by `/`. Similar to `ls -1` output. On failure (path not found, not a directory), returns an error result.

**Delegates to:** `AgentCell::list_dir(path)`

## Behavior

1. Each built-in tool implements the `Tool` trait from `simulacra-types`.
2. Tool input is validated against the schema before delegation. Missing required fields return `ToolError::InvalidArguments`.
3. `SandboxError::CapabilityDenied` from `AgentCell` is converted to `ToolError::CapabilityDenied`.
4. `SandboxError::BudgetExhausted` from `AgentCell` is converted to `ToolError::ExecutionFailed` with a descriptive message including the exhausted resource.
5. Other `SandboxError` variants are converted to `ToolError::ExecutionFailed`.
6. Non-zero shell exit codes are returned as successful tool results (not errors). The LLM interprets the output.
7. `file_edit` with ambiguous match (old_string appears 0 or >1 times) returns a `ToolResult` with `is_error: true` and a message explaining the ambiguity. This is NOT a `ToolError` — it is a normal tool response the LLM can act on.
8. All built-in tools are registered in `ToolRegistry` by a `register_builtins(cell: Arc<AgentCell>)` function.
9. Tool definitions use `schemars::JsonSchema` derive on input structs to generate the `input_schema` field.

## Assertions

### Tool registration

- [x] `register_builtins` registers exactly 6 tools: `file_read`, `file_write`, `file_edit`, `shell_exec`, `js_exec`, `list_dir`.
- [x] `ToolRegistry::definitions()` after `register_builtins` returns 6 definitions with correct names and descriptions.
- [x] Each tool definition has a valid JSON Schema as `input_schema`.

### file_read

- [x] `file_read` with a path that exists returns the file content.
- [x] `file_read` with a path that does not exist returns `ToolResult { is_error: true, .. }` with "not found" in the message.
- [x] `file_read` without `path` argument returns `ToolError::InvalidArguments`.

### file_write

- [x] `file_write` writes content and returns confirmation with byte count.
- [x] `file_write` to a nested path creates parent directories.
- [x] `file_write` without `content` argument returns `ToolError::InvalidArguments`.

### file_edit

- [x] `file_edit` replaces `old_string` with `new_string` in the file.
- [x] `file_edit` where `old_string` is not found returns `ToolResult { is_error: true }` with "not found" message.
- [x] `file_edit` where `old_string` appears more than once returns `ToolResult { is_error: true }` with "ambiguous" message.
- [x] `file_edit` on a non-existent file returns `ToolResult { is_error: true }`.

### shell_exec

- [x] `shell_exec("echo hello")` returns `{ stdout: "hello\n", stderr: "", exit_code: 0 }`.
- [x] `shell_exec("nonexistent_command")` returns a result with non-zero exit code (NOT a ToolError).
- [x] `shell_exec` without `command` argument returns `ToolError::InvalidArguments`.

### js_exec

- [x] `js_exec("1 + 1")` returns the string result.
- [x] `js_exec` with a syntax error returns `ToolResult { is_error: true }` with the error message.
- [x] `js_exec` without `code` argument returns `ToolError::InvalidArguments`.

### list_dir

- [x] `list_dir("/")` returns entries in the root directory.
- [x] `list_dir` on a non-existent path returns `ToolResult { is_error: true }`.
- [x] Directory entries are suffixed with `/` in the output.

### Error mapping

- [x] `AgentCell` capability denial surfaces as `ToolError::CapabilityDenied` through the tool.
- [x] `AgentCell` budget exhaustion surfaces as `ToolError::ExecutionFailed` with resource details.
- [x] VFS errors from `AgentCell` surface as `ToolError::ExecutionFailed` with path and error message.

## Observability (see S010 for conventions)

- [x] Each tool invocation produces a span with `gen_ai.tool.name` = the tool name (per OTel GenAI conventions).
- [x] Tool invocation spans are children of the agent turn span.
- [x] Tool errors (ToolError, not is_error results) are logged at `ERROR` level with the tool name and error message.
- [x] Tool results are captured as events on the tool span (per `gen_ai.tool.message` convention).
