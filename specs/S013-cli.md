# S013 — CLI (simulacra-cli)

**Status:** Active
**Crate:** `simulacra-cli`
**Priority:** Phase 1 required

## Context

`simulacra-cli` is the single binary entry point. For Phase 1, it supports headless mode only: parse config, construct the runtime, run a single agent to completion, print the result. Interactive (TUI) mode is Phase 2.

The CLI is a thin orchestration layer. It does not contain business logic. It wires: config parsing (simulacra-config) → provider creation (simulacra-provider) → sandbox construction (simulacra-sandbox) → tool registration (simulacra-tool) → agent loop execution (simulacra-runtime) → output formatting.

## Behavior

### Argument Parsing

1. `simulacra --config <path>` — path to `simulacra.toml`. Default: `simulacra.toml` in the current directory.
2. `simulacra --task <string>` — task description for headless mode. Overrides `[task].task` from config.
3. `simulacra --mode headless` — run in headless mode (Phase 1 only). Default if `--task` is provided.
4. `simulacra --mode interactive` — reserved for Phase 2. Returns an error with "interactive mode not yet implemented" in Phase 1.
5. `simulacra --verbose` — enable DEBUG-level tracing output.
6. `simulacra --otlp-endpoint <url>` — OTLP exporter endpoint. Overrides any config-level setting. If not set and no config, OTLP export is disabled.

### Startup Sequence

7. Parse CLI arguments (clap).
8. Initialize tracing subscriber. If `--otlp-endpoint` is set, configure `tracing-opentelemetry` + `opentelemetry-otlp`. Otherwise, use `tracing_subscriber::fmt` for stderr logging.
9. Load `SimulacraConfig` from the config file path. If the file does not exist and `--task` is provided, use a default config (single agent, model from `SIMULACRA_MODEL` env var or `"claude-sonnet-4-20250514"`).
10. Resolve the task string: `--task` flag takes precedence over `[task].task` in config. If neither is set, exit with error: "no task specified. Use --task or set [task].task in config."
11. Resolve the entry agent type: `[task].entry_agent` from config, or `"default"` if using the implicit default config.
12. Build `CapabilityToken` from the agent type's `[capabilities]` section.
13. Build `ResourceBudget` from the agent type's `max_turns`, `max_tokens`, etc.
14. Construct `VirtualFs`: detect project root, process automatic mounts and configured `[[vfs.mounts]]` (see S020), then pre-seed `/workspace/task.md` with the task string. All mounts copy into a plain `MemoryFs`.
15. Construct `AgentCell` with VFS, capability token, resource budget, and journal storage (in-memory for Phase 1).
16. Register built-in tools with `ToolRegistry` (see S012).
17. Construct the LLM provider from config (`model`, provider inferred from model name or explicit config).
18. Construct and run the agent loop. The agent loop calls the provider, dispatches tool calls to the registry, and loops until the LLM returns a final response (no tool calls) or budget is exhausted.
19. Print the final agent response to stdout.
20. Exit with code 0 on success, 1 on error.

### Default Config (No simulacra.toml)

21. When no config file exists but `--task` is provided, the CLI creates an implicit configuration:
    - Project name: `"simulacra-adhoc"`
    - Single agent type `"default"` with model from `SIMULACRA_MODEL` env var (or `"claude-sonnet-4-20250514"`)
    - Capabilities: `shell: true`, `javascript: true`, `paths_read: ["/**"]`, `paths_write: ["/**"]`
    - Budget: `max_turns: 50`, `max_tokens: 200_000`
    - No MCP servers

### Environment Variables

22. `SIMULACRA_MODEL` — default model when not specified in config.
23. `ANTHROPIC_API_KEY` — API key for Anthropic provider.
24. `OPENAI_API_KEY` — API key for OpenAI provider.
25. Provider is inferred from model name prefix: `claude-*` → Anthropic, `gpt-*` / `o1-*` / `o3-*` → OpenAI, `ollama:*` → Ollama.

### Output

26. In headless mode, the final agent response text is printed to stdout. No decoration, no framing — just the response. This makes `simulacra` composable in pipelines.
27. Tracing/log output goes to stderr (not stdout).
28. If the agent run fails (provider error, budget exhaustion, etc.), a human-readable error message is printed to stderr, and the process exits with code 1.

### Async Runtime

29. The CLI initializes a tokio multi-thread runtime. The agent loop, provider calls, and tool dispatches all run within this runtime.

## Assertions

### Argument parsing

- [x] `simulacra --task "hello"` parses successfully with task = "hello" and mode = headless.
- [x] `simulacra --config custom.toml --task "x"` uses the custom config path.
- [x] `simulacra --mode interactive` returns an error in Phase 1 with "not yet implemented".
- [x] `simulacra` with no `--task` and no `[task].task` in config exits with error.

### Config loading

- [x] Valid `simulacra.toml` is parsed and the entry agent type is resolved.
- [x] Missing config file with `--task` provided uses default config with `SIMULACRA_MODEL` or fallback model.
- [x] Invalid TOML in config file exits with a parse error message.
- [x] `CapabilityToken` is built from `[agent_types.<name>.capabilities]` section.
- [x] `ResourceBudget` is built from `max_turns`, `max_tokens` in agent type config.

### Startup sequence

- [x] VFS is created and pre-seeded with `/workspace/task.md` containing the task text.
- [x] VFS host mounts from `[[vfs.mounts]]` config are processed before agent loop starts (see S020).
- [x] All mounts copy into a plain MemoryFs (no OverlayFs for mounts in this phase).
- [x] Project root is detected from `--config` path and recorded in the CLI root span.
- [x] Built-in tools (6 tools from S012) are registered in the ToolRegistry.
- [x] Provider is constructed from config model string.
- [x] Provider selection: `claude-*` → Anthropic, `gpt-*` → OpenAI, `ollama:*` → Ollama.

### Output

- [x] Headless mode prints the final response to stdout.
- [x] Log output goes to stderr, not stdout.
- [x] Successful run exits with code 0.
- [x] Failed run exits with code 1 and prints error to stderr.

### Tracing initialization

- [x] `--otlp-endpoint` configures OTLP exporter (spans are exported to the endpoint).
- [x] Without `--otlp-endpoint`, tracing goes to stderr via `fmt` subscriber.
- [x] `--verbose` enables DEBUG-level output.

## Observability (see S010 for conventions)

- [x] CLI startup produces a root span with `simulacra.operation.name` = `cli_run`, `simulacra.task` (first 100 chars of task), and `simulacra.config.path`.
- [x] The agent loop span is a child of the CLI root span.
- [x] CLI shutdown flushes the OTLP exporter before exiting (no lost spans).
