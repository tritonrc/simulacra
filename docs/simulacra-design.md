# Simulacra: AI Agent Framework — Architecture Design Document

**Status:** Design Phase
**Date:** March 8, 2026
**Stack:** Rust (single binary), QuickJS, WASM (future)

---

## 1. Vision

A **single Rust binary** that provides a generic, configurable AI agent framework. Users specify a system prompt, skills, tools/MCP, model, and sub-agents — and Simulacra creates a sandboxed, isolated agent environment that looks and feels like a full development environment (filesystem, shell, code execution) while remaining lightweight enough to run dozens of agents concurrently without containers or VMs.

### Success Criteria

- **Single binary.** `cargo install simulacra` and you're running.
- **Headed or headless.** Chat mode (TUI) for interactive use, task mode for autonomous completion.
- **User-configurable.** Model, system prompt, skills (JS/Python), MCP servers, tools, sub-agents — all via TOML config.
- **Sandboxed and isolated.** Each agent gets its own virtual environment. No containers. No VMs.
- **Fault-tolerant.** Agents crash, get budget-limited, or time out without taking down the system.
- **Natural to the LLM.** The agent environment mirrors what models were trained on — `ls`, `cat`, `grep`, `node`, `python` all work as expected.

---

## 2. Core Insight

LLMs are trained on millions of examples of humans using Unix environments. Claude Code's effectiveness comes not from the model but from giving the agent real `bash`, real `fs`, real `grep`. The agent rides the training distribution.

**Our bet:** you can provide 90% of that behavioral fidelity without a real OS by faking the filesystem, shell, and execution runtime — and the LLM can't tell the difference, because `ls` returns a file listing, `cat` returns contents, and `grep` filters lines regardless of whether the kernel is real.

**What we gain:** instant startup, zero infrastructure, per-agent isolation, full observability, capability-based security, snapshot/replay, and the ability to run 50+ agents in a single process.

**What we lose:** can't `pip install pandas` (C extensions), can't compile and run arbitrary binaries, can't run Docker inside the sandbox. For the ~20% of tasks that need this, we offer an escape hatch to real containers using the same config and agent code (swap VFS backend).

---

## 3. Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│                        User Config (TOML)                     │
│   model, system_prompt, skills, mcp, tools, sub_agents        │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│                     Orchestrator (Rust)                        │
│                                                               │
│  ┌────────────┐  ┌────────────┐  ┌─────────────────────────┐ │
│  │ Model      │  │ MCP        │  │ Session / State          │ │
│  │ Client     │  │ Manager    │  │ Manager                  │ │
│  │ (Provider  │  │ (HTTP/SSE, │  │ (journal, snapshot,      │ │
│  │  trait)    │  │  WASM)     │  │  durable context)        │ │
│  │            │  │            │  │                          │ │
│  └────────────┘  └────────────┘  └─────────────────────────┘ │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │                  Agent Supervisor                         │ │
│  │   (actor-style: spawn, cancel, meter, snapshot, log)      │ │
│  │   (policy-per-agent-type, restart strategies)             │ │
│  │   (Erlang-inspired, built on raw tokio, not ractor)       │ │
│  └──────────────────────────────────────────────────────────┘ │
└──────────────────────────────┬───────────────────────────────┘
                               │ spawns N
┌──────────────────────────────▼───────────────────────────────┐
│                    Agent Cell (per-agent)                      │
│                                                               │
│  ┌────────────┐  ┌────────────┐  ┌─────────────────────────┐ │
│  │ VFS        │  │ QuickJS    │  │ Shell Emulator           │ │
│  │ (MemoryFS  │  │ Runtime    │  │ (builtins + pipes +      │ │
│  │  + Overlay) │  │ (rquickjs) │  │  redirects)              │ │
│  └────────────┘  └────────────┘  └─────────────────────────┘ │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Capability Token  +  Resource Budget                     │ │
│  │  (which tools, which URLs, which paths, how much $)       │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Proxy Layer                                              │ │
│  │  fetch() → host HTTP client    exec() → shell emulator   │ │
│  │  tool()  → MCP manager         spawn() → supervisor      │ │
│  │  ALL side effects mediated by host                        │ │
│  └──────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

### The Golden Rule

**Everything the agent does that has side effects goes through the host.** File writes go through the VFS (host-controlled). Network calls go through the proxy (host-controlled). Tool calls go through the MCP client (host-controlled). Sub-agent spawning goes through the supervisor (host-controlled). The agent's code runs in QuickJS, which is a pure computation sandbox — it *cannot* do anything the host doesn't explicitly provide.

---

## 4. Agent Cell: The Sandboxed Environment

Each agent session gets an isolated "cell" comprising three subsystems:

### 4.1 Virtual Filesystem (VFS)

**Implementation:** Rust-side trait backed by in-memory storage. Uses an OverlayFS pattern:
- **Lower layer (read-only):** Skills, system context, reference files
- **Upper layer (read-write):** Agent's working directory, outputs, scratch space

```rust
trait VirtualFs: Send + Sync {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError>;
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError>;
    fn exists(&self, path: &str) -> bool;
    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError>;
    fn mkdir(&mut self, path: &str) -> Result<(), FsError>;
    fn remove(&mut self, path: &str) -> Result<(), FsError>;
    fn metadata(&self, path: &str) -> Result<Metadata, FsError>;
    fn snapshot(&self) -> VfsSnapshot;
    fn restore(&mut self, snapshot: &VfsSnapshot);
}
```

**Pre-seeding:** Before the agent runs, the host populates:
- `/workspace/task.md` — the task description
- `/workspace/context/` — any injected context files
- `/skills/<name>/` — skill code and prompts
- `/output/` — where the agent writes deliverables

**Snapshotting:** Before each agent "turn," `snapshot()` clones the VFS state. If the turn fails, `restore()` rolls back. Since it's in-memory (`HashMap<PathBuf, Vec<u8>>`), this is microsecond-cheap.

**Path semantics:** Chroot-style — `/` is the root of the VFS. No `/etc/passwd`, no escape. The agent sees a clean Unix-like tree.

### 4.2 Shell Emulator

A Rust module (~2000 lines) that parses command lines and dispatches to builtins operating against the VFS:

**Builtins (Phase 1 — ~15-20 commands):**
`cat`, `ls`, `echo`, `mkdir`, `cp`, `mv`, `rm`, `head`, `tail`, `grep`, `sed`, `wc`, `find`, `sort`, `uniq`, `cut`, `tr`, `tee`

**Operators:**
Pipes (`|`), redirects (`>`, `>>`), command substitution (`$(...)`), environment variables (`$VAR`), `&&` and `||` chaining.

**Escape hatches:**
- `node -e "..."` → dispatches to QuickJS runtime
- `python -c "..."` → dispatches to Pyodide (phase 3)
- Anything else → "command not found" with a helpful message

**Key insight:** The LLM doesn't need real bash. It needs behavioral fidelity — `ls` returns a file listing, `cat` returns contents, `grep` filters lines. The outputs match what the model was trained on.

### 4.3 QuickJS Runtime

Embedded via `rquickjs` crate. Exposes a Node-like API surface, following the pattern established by **AWS LLRT** — Amazon's production Rust+QuickJS runtime that implements Node-compatible APIs as Rust host functions rather than JS polyfills.

**Modules injected into the runtime:**
- `fs` — `readFileSync`, `writeFileSync`, `existsSync`, `readdirSync`, `mkdirSync` → all dispatch to Rust VFS
- `process` — `env`, `cwd()`, `exit()` → managed by host
- `fetch` — global `fetch()` → dispatches to host HTTP client through capability proxy
- `console` — `log`, `error`, `warn` → captured as agent output

**Sync by default:** QuickJS is single-threaded. Sync APIs (`readFileSync`) are the natural fit — no promises needed for FS operations. `fetch()` is the one async API, handled via QuickJS promise loop.

---

## 5. Actor-Based Supervision (Erlang-Inspired, Raw Tokio)

We use the **actor model as a mental framework** without adopting a framework dependency. Built on raw `tokio::spawn` + `mpsc` channels + a custom supervisor.

### 5.1 Actor Tree

```
                    SystemSupervisor
                    ┌── restart policy: one_for_one
                    ├── global resource limits
                    └── top-level error handling
                          │
            ┌─────────────┼──────────────┐
            │             │              │
       MCPManager    ModelPool     AgentSupervisor
       (manages MCP   (semaphore    (per-task policy,
        connections,   on concurrent  spawns agent
        registry of    API calls,     trees)
        available      rate limiting)
        servers)                        │
                          ┌─────────────┼──────────────┐
                          │             │              │
                     AgentCell      AgentCell       AgentCell
                     "planner"      "coder"         "reviewer"
                     (can spawn     (leaf agent)    (leaf agent)
                      children)
                          │
                    ┌─────┼─────┐
                    │           │
               AgentCell    AgentCell
               "sub-task-1" "sub-task-2"
```

### 5.2 Message Types (Priority Order)

```rust
// 1. Signals (highest priority) — immediate action
enum Signal {
    Kill,           // unconditional termination
    Timeout,        // budget/time exceeded
}

// 2. Supervision events — lifecycle notifications
enum SupervisionEvent {
    ChildStarted { id: AgentId },
    ChildCompleted { id: AgentId, result: AgentResult },
    ChildFailed { id: AgentId, error: String, journal: AgentJournal },
    ChildPanicked { id: AgentId, panic_info: String },
}

// 3. Commands — parent → child work assignments
enum AgentCommand {
    RunTask {
        task: String,
        context_files: Vec<(PathBuf, Vec<u8>)>,  // pre-seed VFS
        capabilities: CapabilityToken,
        budget: ResourceBudget,
    },
    Cancel { reason: String },
}

// 4. Results — child → parent responses
enum AgentResult {
    Completed {
        output_files: Vec<(PathBuf, Vec<u8>)>,    // extracted from VFS
        token_usage: TokenUsage,
        log: Vec<AgentEvent>,
    },
    Failed {
        reason: String,
        partial_output: Option<Vec<(PathBuf, Vec<u8>)>>,
        snapshot: VfsSnapshot,
    },
    NeedsInput {
        question: String,
        context: serde_json::Value,
    },
}
```

### 5.3 Restart Strategies

Configured per agent type:

| Strategy | Behavior |
|---|---|
| `retry_n_times(n)` | Respawn with last VFS snapshot, up to N times |
| `snapshot_and_fail` | Save full journal + VFS snapshot, report failure to parent |
| `retry_with_context` | Respawn with failure reason injected into new task prompt |
| `escalate` | Immediately report to parent for re-planning |

### 5.4 Resource Metering

Each agent cell has a `ResourceBudget` checked **at the host boundary** before every mediated operation:

```rust
struct ResourceBudget {
    max_llm_tokens: u64,          // total input+output tokens
    max_llm_calls: u32,           // number of model invocations
    max_tool_calls: u32,          // number of MCP/tool invocations
    max_wall_clock: Duration,     // total elapsed time
    max_vfs_bytes: u64,           // total storage consumed
    max_sub_agents: u32,          // how many children can be spawned
    tokens_used: u64,             // running counter
    // ... other running counters
}
```

When a limit is hit, the agent receives a structured error: `"Budget exhausted: token limit reached (98,234 / 100,000). Wrap up your work and report results."` This gives the agent a chance to gracefully summarize rather than being hard-killed.

---

## 6. Durable Context + Journaling

Inspired by **Restate's durable execution engine** (journal-based replay at 94K+ actions/sec) and **LangGraph's checkpoint model** (time-travel debugging, fork-from-any-point), every agent cell maintains an append-only journal with periodic full checkpoints.

### Journal Architecture (Restate-inspired)

The journal uses **conditional append** — not full-state snapshots at every step. This keeps durable steps cheap (a log append, not a VFS clone). Full VFS snapshots are taken as periodic *checkpoints*, not at every turn.

```rust
struct AgentJournal {
    agent_id: AgentId,
    agent_type: String,
    schema_version: u32,            // versioned from day one (LangGraph lesson)
    entries: Vec<JournalEntry>,
    checkpoints: Vec<Checkpoint>,   // periodic full-state snapshots
}

struct Checkpoint {
    after_entry: usize,             // journal index this checkpoint follows
    vfs_snapshot: VfsSnapshot,      // full VFS state at this point
    message_history: Vec<Message>,  // full conversation at this point
    resource_usage: ResourceBudget, // budget consumed so far
    timestamp: Instant,
}

enum JournalEntry {
    TurnStart { turn: u32 },
    LlmRequest { messages: Vec<Message>, tools: Vec<ToolDef> },
    LlmResponse { response: LlmResponse, tokens: TokenUsage },
    ToolCall { name: String, args: Value, result: Value },
    ShellCommand { command: String, stdout: String, stderr: String, exit_code: i32 },
    CodeExecution { language: String, code: String, output: String },
    SubAgentSpawned { child_id: AgentId, config: AgentTypeConfig },
    SubAgentCompleted { child_id: AgentId, result: AgentResult },
    FileWrite { path: PathBuf, size: u64 },
    HttpRequest { url: String, method: String, status: u16 },
}
```

### Replay (Restate pattern)

When replaying, the runtime walks the journal. For each entry, it checks: "has this step been executed previously?" If yes, it substitutes the recorded result and skips re-execution. If no (we've reached the frontier), it executes live. This is Restate's core model — deterministic replay of completed steps, live execution from the frontier forward.

### Time-Travel + Forking (LangGraph pattern)

Beyond linear replay, we support **forking** from any checkpoint:

```
Original execution: [step 1] → [step 2] → [step 3] → [FAILURE at step 4]

Fork from checkpoint after step 2:
  → [step 2 checkpoint restored]
  → [step 3'] (different tool choice)
  → [step 4'] (succeeds)
```

This enables: debugging ("what went wrong?"), retry-with-context ("try step 3 differently"), and exploration ("what if the agent had chosen the other tool?").

### Storage (pluggable, following LangGraph's CheckpointSaver pattern)

```rust
#[async_trait]
trait JournalStorage: Send + Sync {
    // Write path
    async fn append(&self, agent_id: &AgentId, entry: &JournalEntry) -> Result<()>;
    async fn save_checkpoint(&self, agent_id: &AgentId, cp: &Checkpoint) -> Result<()>;

    // Read path — full journal
    async fn load(&self, agent_id: &AgentId) -> Result<AgentJournal>;
    async fn fork_from(&self, agent_id: &AgentId, checkpoint_idx: usize) -> Result<AgentJournal>;
    async fn list_checkpoints(&self, agent_id: &AgentId) -> Result<Vec<CheckpointMeta>>;

    // Supervisor queries — lightweight internal inspection
    // (external observability goes through OTLP, not journal queries)
    async fn query_token_usage(&self, agent_id: &AgentId) -> Result<TokenUsage>;
    async fn query_errors(&self, agent_id: &AgentId) -> Result<Vec<&JournalEntry>>;
    async fn query_child_status(&self, agent_id: &AgentId) -> Result<Vec<(AgentId, AgentStatus)>>;
}

// Implementations (mirroring LangGraph's InMemorySaver / SqliteSaver / PostgresSaver):
// InMemoryJournalStorage — for development/testing
// SqliteJournalStorage — file-backed production single-node (default in CLI)
```

The supervisor uses journal queries internally for restart decisions and budget enforcement. External monitoring and agent introspection goes through OTLP (see Section 10).

### What journaling enables:

1. **Deterministic replay** (Restate pattern). All external inputs are recorded. Replay substitutes recorded values.
2. **Crash recovery.** Process dies at step 47? Restart, replay to 46, resume live from 47.
3. **Time-travel debugging** (LangGraph pattern). Inspect any checkpoint, fork and explore alternatives.
4. **Cost attribution.** Exact token counts per agent, per turn, per sub-agent tree.
5. **Audit trail.** Every external effect, in order, with timestamps.
6. **Schema evolution.** Journal entries are versioned so older journals work with newer runtimes.

---

## 7. Capability-Gated Tool Calls

All side effects are mediated. The agent's code never directly touches the network, filesystem (real), or external services.

### How it works:

```
Agent JS code                    QuickJS boundary              Rust host
─────────────                    ────────────────              ─────────
fetch("https://api.stripe.com")  → intercept fetch()          → check capability token
                                                               → "net:api.stripe.com" allowed?
                                                               → yes: make HTTP request
                                                               → return result to JS
                                                               → log to journal

fs.writeFileSync("/output/x")   → intercept fs module         → VFS write (in-memory)
                                                               → check path allowed
                                                               → check storage budget
                                                               → log to journal

exec("git status")              → intercept child_process     → check "shell" capability
                                                               → dispatch to shell emulator
                                                               → log to journal
```

### Capability Token Structure:

```rust
struct CapabilityToken {
    network: Vec<NetworkPermission>,   // ["net:api.stripe.com", "net:*.github.com"]
    mcp_tools: Vec<String>,            // ["mcp:github:*", "mcp:postgres:query"]
    shell: bool,                       // can run shell commands
    javascript: bool,                  // can execute JS
    python: bool,                      // can execute Python
    paths_write: Vec<PathPattern>,     // ["/workspace/**", "/output/**"]
    paths_read: Vec<PathPattern>,      // ["/**"]  (read everything)
    spawn_types: Vec<String>,          // ["coder", "researcher"]
}
```

### Capability Attenuation:

When a parent spawns a child, the child's capabilities are a **subset** of the parent's. A parent with `["net:*.github.com"]` can give a child `["net:api.github.com"]` but never `["net:*.stripe.com"]`. This is enforced at the supervisor level, not by convention.

---

## 8. MCP Integration — Two Tiers

We explicitly **do not support** npx/uvx or stdio-based MCP servers. Spawning child processes contradicts our single-binary, zero-infrastructure philosophy. The MCP ecosystem is rapidly moving toward remote HTTP/SSE endpoints — we ride that wave.

```
┌──────────────────────────────────────────────────────────────┐
│                       MCP Manager                             │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │ Tier 1: Remote HTTP / Streamable HTTP                    │ │
│  │                                                          │ │
│  │ Rust HTTP client → URL endpoint.                         │ │
│  │ Covers: hosted services, cloud APIs, any MCP server      │ │
│  │ with an HTTP endpoint. GitHub, Stripe, databases,        │ │
│  │ anything accessible over the network.                    │ │
│  │                                                          │ │
│  │ No child processes. No binaries to install.              │ │
│  │ Just a URL in the config.                                │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │ Tier 2: WASM MCP Servers (Future)                        │ │
│  │                                                          │ │
│  │ MCP server compiled to WASM → runs in-process via        │ │
│  │ wasmtime. No subprocess. No network. Just a function     │ │
│  │ call with WASI VFS backing.                              │ │
│  │                                                          │ │
│  │ The endgame: portable, sandboxed, zero-latency tool      │ │
│  │ execution. Ship MCP servers as .wasm files.              │ │
│  └──────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

**From the agent's perspective, both tiers look identical.** The agent calls a tool, the capability proxy sends it to the MCP Manager, the manager routes it to the right server (HTTP or WASM), and the result comes back. The agent never knows how the tool was hosted.

---

## 9. Skills System

A **skill** is a bundle of: prompt fragment + code files + optional tool definitions.

```
skills/
├── rust-dev/
│   ├── skill.toml          # metadata, dependencies
│   ├── prompt.md           # appended to system prompt when active
│   ├── lib.js              # JS utilities mounted at /skills/rust-dev/
│   └── tools.toml          # additional tool definitions
├── code-review/
│   ├── skill.toml
│   ├── prompt.md
│   └── review.py           # Python script (Pyodide, phase 3)
```

When a skill is active for an agent:
1. `prompt.md` content is appended to the system prompt
2. Code files are mounted read-only into the VFS at `/skills/<name>/`
3. Tool definitions from `tools.toml` are registered with the model

Skills are **composable** — an agent can have multiple skills active simultaneously. They're **portable** — just a directory you can share, version, publish.

---

## 10. Observability — OpenTelemetry GenAI Semantic Conventions

Simulacra emits OpenTelemetry signals following the **OTel GenAI Semantic Conventions** (v1.37+) so any OTLP-compatible backend can observe the agent runtime using standard schemas. The journal is the *internal* state (recovery, replay, fork). OTLP is the *external* observability interface.

**Why standard conventions:** The OTel GenAI SIG has defined specific semantic conventions for LLM operations, agent spans, tool calls, and token usage. Datadog, Grafana, Jaeger, and your own Obsidian instance all understand these conventions. Agents know PromQL/LogQL/TraceQL from training data. Standard conventions, not custom schemas.

**GenAI span conventions we follow:**

Client spans (per LLM call):
- Span name: `{gen_ai.operation.name} {gen_ai.request.model}` (e.g. `chat claude-sonnet-4-20250514`)
- `gen_ai.operation.name`: `chat` for completions, `embeddings` for embeddings
- `gen_ai.request.model`: exact model string requested
- `gen_ai.response.model`: actual model used (may differ from requested)
- `gen_ai.provider.name`: `anthropic`, `openai`, `ollama`
- `gen_ai.response.id`: provider response ID (e.g. `msg_01XFDUDYJgAACzvnptvVoYEL`)
- `gen_ai.response.finish_reasons`: `["end_turn"]`, `["tool_use"]`, `["max_tokens"]`
- `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`: token counts from response
- `gen_ai.request.temperature`, `gen_ai.request.max_tokens`: request parameters
- `server.address`, `server.port`: API endpoint
- Span kind: `CLIENT`

Spec reference: https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/

Agent spans (per agent invocation):
- `gen_ai.operation.name`: `invoke_agent` for running an agent, `create_agent` for spawning sub-agents
- `gen_ai.agent.name`: agent type from config (e.g. `planner`, `coder`, `reviewer`)
- Span kind: `INTERNAL` for in-process agents (our case)

Spec reference: https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-agent-spans/

Tool call events:
- Event name: `gen_ai.tool.message` on tool invocation spans
- Tool name, arguments, result captured per convention

**Simulacra-specific metrics** (prefixed `simulacra.` to avoid collision with standard `gen_ai.` metrics):
- `simulacra.agent.vfs_bytes` (gauge, per agent)
- `simulacra.agent.capability_denials` (counter, per agent, per capability)
- `simulacra.agent.restarts` (counter, per agent type, per restart strategy)

**Standard GenAI metrics** (from the OTel convention):
- `gen_ai.client.token.usage` (histogram, by `gen_ai.operation.name`, `gen_ai.request.model`)
- `gen_ai.client.operation.duration` (histogram, by operation and model)
- `gen_ai.client.time_per_output_token` (histogram, decode latency — important for streaming)

Spec reference: https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/

**Implementation:** `tracing` crate for instrumentation + `tracing-opentelemetry` bridge + `opentelemetry-otlp` exporter. The OTLP endpoint is configurable — point it at Obsidian, Jaeger, Grafana, Datadog, or any OTLP-compatible backend.

**Relationship to journal:** The journal records *what happened* for replay. OTLP records *how it happened* for monitoring. They capture overlapping data but serve different purposes. The journal is authoritative for state recovery; OTLP is authoritative for performance analysis and live monitoring.

---

## 11. Configuration

```toml
# simulacra.toml — top-level project config

[project]
name = "my-automation"
description = "Automated code review pipeline"

# ── Agent Type Definitions ─────────────────────────────

[agent_types.planner]
model = "claude-sonnet-4-20250514"
system_prompt = "prompts/planner.md"
skills = []
max_turns = 50
max_tokens = 100_000
max_sub_agents = 5
can_spawn = ["coder", "reviewer"]
restart_policy = "retry_twice_then_fail"

[agent_types.planner.capabilities]
network = ["net:api.anthropic.com"]
mcp = ["mcp:github:*", "mcp:linear:*"]
shell = true
javascript = true

[agent_types.coder]
model = "claude-sonnet-4-20250514"
system_prompt = "prompts/coder.md"
skills = ["skills/rust-dev", "skills/testing"]
max_turns = 30
max_tokens = 50_000
max_sub_agents = 0
can_spawn = []
restart_policy = "snapshot_and_fail"

[agent_types.coder.capabilities]
network = []
mcp = []
shell = true
javascript = true
python = true

[agent_types.reviewer]
model = "claude-haiku-4-5-20251001"    # cheaper model for review
system_prompt = "prompts/reviewer.md"
skills = ["skills/code-review"]
max_turns = 20
max_tokens = 20_000
max_sub_agents = 0
can_spawn = []
restart_policy = "retry_once"

[agent_types.reviewer.capabilities]
network = ["net:*.github.com"]
mcp = ["mcp:github:get_pr", "mcp:github:list_files"]
shell = true
javascript = false

# ── MCP Server Definitions ─────────────────────────────

[[mcp.servers]]
name = "github"
transport = "sse"
url = "https://mcp.github.com/sse"
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }

[[mcp.servers]]
name = "postgres"
transport = "sse"
url = "http://localhost:3001/sse"

[[mcp.servers]]
name = "web-search"
transport = "wasm"                              # tier 2 (future)
module = "mcp-servers/web-search.wasm"

# ── Entry Point ─────────────────────────────────────────

[task]
entry_agent = "planner"
mode = "headless"                                # or "interactive"
task = "Review PR #42 and write tests for any untested code paths"
```

---

## 12. Filesystem as IPC

The virtual filesystem is the **primary data exchange mechanism** between agents:

```
Parent "planner" agent                    Child "coder" agent
─────────────────────                     ────────────────────

1. Writes task to its VFS:
   /workspace/sub-tasks/task-1.md

2. Sends RunTask command:
   context_files = [
     ("task.md", <content of task-1.md>),
     ("context/api.json", <relevant context>),
   ]
                                          3. VFS pre-seeded:
                                             /workspace/task.md
                                             /workspace/context/api.json

                                          4. Agent works, writes:
                                             /workspace/output/solution.rs
                                             /workspace/output/tests.rs

                                          5. Sends Completed:
                                             output_files = [
                                               ("output/solution.rs", ...),
                                               ("output/tests.rs", ...),
                                             ]

6. Receives results, writes to its VFS:
   /workspace/results/coder/solution.rs
   /workspace/results/coder/tests.rs

7. Continues planning with results...
```

This mirrors how humans delegate work: "here's a folder with requirements, put your deliverables here." Clean, intuitive, and the LLM already knows how to work with files.

---

## 13. Dependency Philosophy: Build vs. Borrow

### Why we don't depend on skelegent (or similar early-stage agent frameworks)

We studied skelegent (secbear, formerly neuron), yoagent, AutoAgents, and other Rust agent crates extensively. Skelegent in particular has excellent architectural ideas — layered protocol contracts, object-safe traits, effects system, specs-as-source-of-truth, turn decomposition, security hooks.

**However, we build our own thin implementations of these patterns rather than taking cargo dependencies, for these reasons:**

1. **Maturity risk.** These projects are weeks-to-months old with minimal adoption. API surfaces will change. Taking a dependency on experimental types that permeate your entire codebase (Message, ToolDefinition, Provider) is the worst possible layer to have instability.

2. **Abstraction mismatch.** Our Provider trait needs budget-checking and journal-logging baked into every call. Our Tool trait needs capability verification at the trait level. Our agent loop needs VFS snapshots at turn boundaries. These are load-bearing requirements, not afterthoughts — they need to be in the trait definitions, not bolted on.

3. **Solo maintainer risk.** A dead dependency at the types layer cascades everywhere. Forking types crates is painful because they permeate every module.

4. **The code is small.** Provider trait + Anthropic/OpenAI/Ollama impls: ~800 lines. Tool trait + registry + schema gen: ~300 lines. Agent loop: ~500 lines. Context management: ~400 lines. Session/guardrails: ~200 lines. **Total: ~2200 lines** — a week of focused work, perfectly fitted to our architecture.

### What we borrow as patterns (not code, not dependencies)

| Source | Pattern we adopt |
|---|---|
| **skelegent layer0/** | Standalone types crate as the leaf of the dep graph. Object-safe protocol traits (`Send + Sync`). `rust_decimal::Decimal` for cost tracking. |
| **skelegent turn/** | Turn decomposition: separating provider abstraction from context assembly. `ContextStrategy` trait. |
| **skelegent effects/** | Effects system concept: separating what-to-do from how-to-do-it. Informs our capability proxy design. |
| **skelegent specs/** | Specs-as-contracts pattern. Formal verification of trait implementations. |
| **skelegent hooks/** | Security hooks as composable middleware. Informs our guardrail design (Pass/Tripwire/Warn). |
| **skelegent op/** | Operator patterns: ReAct-style loops vs single-shot. Our agent loop is an operator. |
| **skelegent state/** | Session/state management as a pluggable trait. `InMemorySessionStorage` and `FileSessionStorage` patterns. |

### External dependencies we DO take (justified, mature, well-maintained)

| Dependency | Justification |
|---|---|
| `rmcp` | Official MCP Rust SDK from the Model Context Protocol team. Real adoption, active maintenance, protocol-correct. We use the HTTP/SSE transport only. |
| `rquickjs` | Only viable safe high-level QuickJS binding for Rust. No alternative. |
| `reqwest` | De facto Rust HTTP client. Needed for LLM API calls + capability-proxied HTTP. |
| `tokio` | Async runtime. Non-negotiable for concurrent agent execution. |
| `serde` / `serde_json` | Serialization. Non-negotiable. |
| `schemars` | JSON Schema generation for tool definitions. Mature, well-maintained. |
| `rust_decimal` | Precise cost tracking without floating-point accumulation errors (pattern from skelegent). |
| `ratatui` | TUI framework for headed mode. Mature, widely adopted. |
| `tracing-opentelemetry` | Bridge from `tracing` instrumentation to OpenTelemetry export. |
| `opentelemetry-otlp` | OTLP exporter — sends traces/metrics/logs to any OTLP backend (Obsidian, Jaeger, Grafana). |
| `eventsource-stream` | SSE parsing for streaming LLM responses. Thin, focused. |
| `toml` | Config file parsing. |

### Why rmcp is the right dependency but skelegent isn't

`rmcp` is maintained by the MCP specification team, has broad adoption across the ecosystem, has a narrow and stable API surface (it implements a spec), and sits at a clear boundary (MCP protocol handling). If rmcp changed its API, the migration is contained to `simulacra-mcp`.

Skelegent sits at the *foundation* layer — types, traits, the agent loop. If it changed its `Provider` trait signature, the migration touches every provider, every tool, the agent loop, the supervisor, the journal system. That's not a dependency — that's a marriage.

### Dependency auditing (cargo-deny)

`deny.toml` at workspace root (pattern adopted from skelegent). `cargo deny check` runs in CI to enforce a license allowlist (MIT, Apache-2.0, BSD-2/3, ISC) and flag duplicate crate versions. No dependency enters the tree without passing license and security checks.

---

## 14. Crate Structure

```
simulacra/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── simulacra-types/              # Message, ToolDefinition, ToolCall, ToolResult,
│   │                             # Provider trait, Tool trait, ContextStrategy trait,
│   │                             # CapabilityToken, ResourceBudget, AgentId,
│   │                             # TokenUsage, errors
│   │                             # deps: serde, schemars, rust_decimal
│   │                             # (leaf crate — zero internal deps)
│   │
│   ├── simulacra-provider/           # Provider implementations:
│   │                             # Anthropic (Messages API, streaming, tool_use)
│   │                             # OpenAI (Chat Completions, streaming)
│   │                             # Ollama (Chat API, NDJSON streaming)
│   │                             # Built-in: retry w/ backoff, rate limiting,
│   │                             #   budget check before call, journal logging,
│   │                             #   streaming token counting
│   │                             # deps: simulacra-types, reqwest, tokio,
│   │                             #        eventsource-stream
│   │
│   ├── simulacra-tool/               # Tool trait, ToolRegistry, middleware pipeline
│   │                             # Schema generation via schemars
│   │                             # Built-in: capability check before dispatch
│   │                             # Optional: #[simulacra_tool] proc macro
│   │                             # deps: simulacra-types, schemars, serde_json
│   │
│   ├── simulacra-context/            # ContextStrategy trait + implementations:
│   │                             # SlidingWindow, TieredCompaction
│   │                             # Token counting (estimate + actual reconciliation)
│   │                             # deps: simulacra-types
│   │
│   ├── simulacra-vfs/                # VirtualFs trait, MemoryFS, OverlayFS,
│   │                             # VfsSnapshot, path resolution, chroot semantics
│   │                             # deps: simulacra-types
│   │
│   ├── simulacra-shell/              # Shell parser, 15-20 builtins (cat, grep, etc.),
│   │                             # pipe/redirect execution, glob expansion,
│   │                             # env vars, command substitution
│   │                             # deps: simulacra-types, simulacra-vfs
│   │
│   ├── simulacra-quickjs/            # QuickJS runtime wrapper via rquickjs,
│   │                             # fs/fetch/process/console module bindings
│   │                             # (following AWS LLRT pattern: all APIs in Rust,
│   │                             #  not JS polyfills)
│   │                             # All JS side-effects route to host
│   │                             # deps: simulacra-types, simulacra-vfs, rquickjs
│   │
│   ├── simulacra-sandbox/            # AgentCell: composes VFS + shell + QuickJS
│   │                             # Capability enforcement proxy layer
│   │                             # All side effects mediated here
│   │                             # deps: simulacra-types, simulacra-vfs, simulacra-shell,
│   │                             #        simulacra-quickjs, simulacra-tool
│   │
│   ├── simulacra-mcp/                # MCP client bridging rmcp → simulacra Tool trait
│   │                             # Tier 1: Remote HTTP/SSE (streamable HTTP)
│   │                             # Tier 2: WASM in-process (future, via wasmtime)
│   │                             # NO stdio, NO npx/uvx — HTTP or WASM only
│   │                             # deps: simulacra-types, simulacra-tool, rmcp
│   │
│   ├── simulacra-runtime/            # Agent loop (with journal/budget/VFS hooks)
│   │                             # Supervisor (actor-style on raw tokio)
│   │                             # AgentJournal + durable context + replay
│   │                             # Session + SessionStorage trait
│   │                             # Guardrails (input/output hooks)
│   │                             # Restart strategies, resource metering
│   │                             # Capability attenuation on sub-agent spawn
│   │                             # OTLP emission (tracing-opentelemetry bridge)
│   │                             # deps: simulacra-types, simulacra-provider, simulacra-tool,
│   │                             #        simulacra-context, simulacra-mcp, simulacra-sandbox,
│   │                             #        tokio, tracing-opentelemetry,
│   │                             #        opentelemetry-otlp
│   │
│   ├── simulacra-config/             # TOML config parsing, agent type definitions,
│   │                             # skill loading, validation
│   │                             # deps: simulacra-types, toml, serde
│   │
│   └── simulacra-cli/                # Binary: TUI (ratatui) + headless task runner
│                                 # CLI arg parsing, signal handling
│                                 # deps: everything above, ratatui, clap
│
├── skills/                       # built-in skill library
│   ├── general-coding/
│   ├── code-review/
│   └── ...
│
└── prompts/                      # example system prompts
```

**Dependency graph flows strictly downward.** `simulacra-types` is the leaf (only serde/schemars/rust_decimal). `simulacra-cli` is the root. Each crate can be used independently — someone could use just `simulacra-vfs` + `simulacra-quickjs` without the actor system. The only external "framework-level" dependency is `rmcp` in `simulacra-mcp`, and it's contained to that one crate.

---

## 15. Build Phases

### Phase 1: Foundation (4–6 weeks)

**Goal:** A single agent that can read files, write files, run JS, pipe shell commands, and call one MCP server.

- [ ] `simulacra-types` — core types, Provider/Tool/ContextStrategy traits, capability token, resource budget
- [ ] `simulacra-provider` — Anthropic provider with streaming, retry, budget hooks
- [ ] `simulacra-tool` — Tool trait, registry, schema generation via schemars
- [ ] `simulacra-context` — SlidingWindow context strategy, token counting
- [ ] `simulacra-vfs` — MemoryFS with snapshot/restore
- [ ] `simulacra-shell` — parser + 15 builtins + pipes + redirects
- [ ] `simulacra-quickjs` — rquickjs wrapper with fs/fetch/console modules
- [ ] `simulacra-sandbox` — AgentCell composing the above with capability proxy
- [ ] `simulacra-runtime` — basic agent loop (journal + budget + VFS snapshot per turn)
- [ ] `simulacra-cli` — basic headed (readline) and headless modes

**Milestone:** Run an agent that processes a task using shell + JS, reads/writes files in the VFS, and produces output.

### Phase 2: Multi-Agent + MCP (3–4 weeks)

**Goal:** Parent agents can spawn sub-agents. MCP servers provide external tools.

- [ ] `simulacra-mcp` — MCP client (HTTP/SSE remote servers only)
- [ ] `simulacra-runtime` — supervisor, actor-style spawn/cancel/restart
- [ ] `simulacra-config` — full TOML config with agent types
- [ ] Agent journal + basic snapshotting
- [ ] Capability attenuation on sub-agent spawn
- [ ] Resource metering at host boundary
- [ ] OpenAI + Ollama providers

**Milestone:** A planner agent spawns coder + reviewer sub-agents, passes work via VFS, collects results, handles failures with restarts.

### Phase 3: Polish + Advanced (ongoing)

- [ ] Pyodide integration for Python execution
- [ ] WASM MCP servers (Tier 2) via wasmtime
- [ ] Deterministic replay from journal (Restate pattern)
- [ ] Time-travel debugging + fork from checkpoint (LangGraph pattern)
- [ ] TUI with agent tree visualization
- [ ] Skill marketplace / registry
- [ ] Container-mode escape hatch (swap VFS backend)
- [ ] SqliteJournalStorage for production persistence
- [ ] `ractor_cluster`-style distributed agents (far future)

---

## 16. Key Design References

| Project | What we learn from it |
|---|---|
| **Obsidian (tritonrc)** | **Single-binary OTLP observability backend, designed for agent introspection.** A Rust binary that ingests OpenTelemetry logs/metrics/traces and exposes PromQL/LogQL/TraceQL query interfaces — built specifically so agents can introspect the execution of code they're working on. Two impacts on Simulacra: (1) Simulacra should **emit OTLP** so tools like Obsidian can observe the agent runtime externally — every LLM call is a span, every tool invocation is a span, token consumption is a metric. (2) The design philosophy validates our approach: single binary, standard protocols, query interfaces the model already knows from training data (PromQL/LogQL/TraceQL), not custom APIs. Part of the harness engineering discipline: give agents the observability tools to diagnose problems autonomously. |
| **OpenAI Harness Engineering** | **The methodology for building with agents.** AGENTS.md as table of contents, not encyclopedia. 88 per-subsystem instruction files. Strict layered architecture enforced mechanically. Structured docs/ as system of record. "When the agent struggles, treat it as a signal — identify what's missing and feed it back into the repo." Constraints make agents more productive, not less. Background agents for documentation consistency. Built 1M lines in 5 months with 3 engineers. |
| **Restate** | **Journal-based durable execution at scale.** Append-only command log, conditional replay (skip completed steps, resume from frontier), bidirectional connection with executing handlers, 94K+ actions/sec with 10ms p50 per step. Single Rust binary. Proves our journal architecture is performant, not just theoretically clean. |
| **LangGraph** | **Checkpoint + time-travel as first-class primitives.** Fork execution from any checkpoint to explore alternatives. Pluggable CheckpointSaver (InMemory/SQLite/Postgres). Schema versioning for state evolution. Production patterns: guardrails before transitions, deterministic routing in critical stages, per-node checkpoint persistence. Battle-tested at Uber, LinkedIn, Klarna. |
| **Temporal** | **Durable execution pioneer.** Workflow-as-code with automatic retry and state recovery. Worker model where the service records execution history and replays to last known state on failure. Established the patterns Restate refined. |
| **Claude Code sandbox-runtime** | OS-level sandboxing patterns (bubblewrap/seatbelt), the insight that network + filesystem isolation together are essential. Open-sourced npm package. |
| **amla-sandbox** | WASM sandbox with VFS + shell builtins + capability tokens in a 13MB binary. Closest prior art to our sandbox architecture. Demonstrates "code mode" — agents write scripts instead of making individual tool calls. Capability attenuation for sub-agent delegation. |
| **skelegent (secbear, formerly neuron)** | **Patterns only, not a dependency.** Evolved from composable crates to a full layered architecture: `layer0/` protocol traits, `turn/` for provider+context, `op/` for operator loops (ReAct, single-shot), `orch/` for orchestration, `effects/` for side-effect execution, plus `auth/`, `crypto/`, `secret/`, `rules/`, `hooks/`. Has formal `SPECS.md` as source of truth. New patterns worth studying: effects system (separating what-to-do from how-to-do-it), specs-as-contracts, turn decomposition primitives, security hooks. Still early-stage for cargo dependency, but the architecture has matured significantly. |
| **yoagent** | Single-crate Rust agent with bash/file/edit tools, sub-agents, skills directories, 20+ provider support via quirk flags. Reference for tool implementations, provider quirk handling, and skills-as-directories pattern. |
| **Vercel just-bash** | Proof that 40+ Unix builtins in a virtual FS with <1ms startup is viable and useful. |
| **ractor** | Erlang-style supervision trees in Rust. Message priority, supervision events, restart strategies. We borrow the mental model without the dependency. |
| **rust-vfs crate** | MemoryFS, OverlayFS, AltrootFS patterns for virtual filesystem composition. |
| **AWS LLRT** | **Production Rust + QuickJS runtime from AWS.** Under 2MB binary, 10x faster startup than Node.js. All JS APIs implemented in Rust (not JS). No JIT — same trade-off we're making. Proves QuickJS-in-Rust is viable at production scale for IO-bound workloads. Key architectural lessons: bundle everything into native code, implement Node-compatible APIs (fs, fetch, crypto) as Rust host functions, ESM modules only. Their approach to bridging Rust↔QuickJS for the `fs`, `net`, `crypto` modules is directly reusable reference code for our VFS/fetch/shell bindings. |
| **rquickjs** | Safe high-level QuickJS bindings for Rust with async support. **Justified cargo dependency.** LLRT validates the Rust+QuickJS approach at scale. |
| **rmcp (official MCP Rust SDK)** | Native Rust MCP client/server, SSE + streamable HTTP transports, tool macros. **Justified cargo dependency** — maintained by the MCP team, protocol-correct, narrow API surface. We use only the HTTP/SSE transport, not stdio. |
| **wasmtime + WASI** | WasiFile/WasiDir traits map to our VFS. Future path for WASM MCP servers and plugin isolation. |

---

## 17. Open Questions

1. **Journal storage format.** JSON lines for simplicity? Compact binary (MessagePack/bincode) for performance? Start JSON, optimize later?

2. **VFS size limits.** What's a reasonable default? 100MB per agent? Need to benchmark MemoryFS clone performance at various sizes.

3. **Shell fidelity boundary.** Where exactly do we draw the line? Do we need `awk`? `xargs`? `curl` (as a builtin vs. capability-gated tool)?

4. **Headed mode UX.** TUI framework choice (ratatui?). How to visualize the agent tree, sub-agent progress, resource consumption in real-time?

5. **Skill packaging format.** Just directories? Or a proper package format (tarball with manifest)? Registry/discovery mechanism?

6. **Hot-reload.** Can we update an agent's system prompt or skills mid-session without restarting? Useful for interactive development.
