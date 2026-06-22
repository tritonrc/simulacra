# Test Suite Audit — 2026-03-21

Adversarial review of all 12 crates by GPT-5.4 and Gemini Pro 3, cross-checked against specs and `rules/R004-test-against-fakes.md`.

**GPT-5.4 Totals:** 46 CRITICAL, 30 GAP, 18 WEAK, 8 SMELL
**Gemini Pro 3:** 12/12 crates reviewed — 28 agreements with GPT-5.4, 40+ new findings (incl. 3 implementation bugs, 4 new GAPs per crate avg)

---

## Systemic Issues

### S1. Source-scanning tests (R004 violations)

Tests that use `include_str!` or `fs::read_to_string` to grep source code for strings.
These verify comments/dead code exist, not runtime behavior. **All must be replaced with behavioral tests.**

| Crate | File | What it scans for |
|-------|------|-------------------|
| simulacra-types | `src/journal.rs:143` | Forbidden substrings in journal source |
| simulacra-quickjs | `tests/s016_native_modules_red.rs:8` | `ModuleDef` registration via `.contains()` |
| simulacra-runtime | `tests/s019_activity_events_red.rs` (entire file) | ActivityEvent emission snippets |
| simulacra-cli | `tests/s019_activity_cli_red.rs` (entire file) | Activity rendering snippets |
| simulacra-provider | `src/anthropic/client.rs:971` | Backend selection via `include_str!` on lib.rs/Cargo.toml |
| simulacra-mcp | `tests/s008_mcp_red.rs:952` | Child-process command absence |

### S2. "Before return" ordering claims that only check post-completion

Multiple tests claim to verify "journal entry written before return" but only inspect journal contents after the entire operation completes. This pattern appears in simulacra-sandbox, simulacra-runtime, and simulacra-mcp.

### S3. Fake tools tested instead of production code

`simulacra-tool/tests/s018_spawn_tool_red.rs` defines `PendingSpawnAgentTool` in-test and tests that instead of the real `SpawnAgentTool`. Similarly, `simulacra-runtime/tests/s018_subagent_red.rs` intercepts supervisor messages rather than running the actor loop.

### S4. Interactive REPL loop never tested end-to-end

No test in simulacra-cli calls `run_interactive_loop()`. All S015 "behavioral" coverage hits helper methods directly.

---

## Per-Crate Findings

### simulacra-types (4 CRITICAL, 3 GAP, 1 WEAK)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| T1 | CRITICAL | Journal test is source-scanning (R004) | `src/journal.rs:143` |
| T2 | CRITICAL | `ProviderError::classify()` and `is_retryable()` have zero local tests | `src/provider.rs:45` |
| T3 | CRITICAL | `check_path_read/write` and path matcher untested | `src/capability.rs:208` |
| T4 | CRITICAL | `is_subset_of()` for `mcp_tools`, `paths_write`, `paths_read` untested | `src/capability.rs:87` |
| T5 | GAP | `check_skill()` never directly tested | `src/capability.rs:248` |
| T6 | GAP | VFS-byte budget exhaustion branch untested in `check_budget()` | `src/budget.rs:76` |
| T7 | GAP | No serde contract tests for `Message`, `FinishReason`, `ProviderResponse` | `src/provider.rs:15` |
| T8 | WEAK | ActivityEvent roundtrip test covers only one variant shape | `src/activity.rs:86` |

### simulacra-config (4 CRITICAL, 2 GAP, 1 WEAK, 1 SMELL)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| C1 | CRITICAL | `build_capability_token()` completely unverified | `src/lib.rs:181` |
| C2 | CRITICAL | `[vfs]` defaults from S020 have zero coverage | `src/lib.rs:138` |
| C3 | CRITICAL | `[[vfs.mounts]]` config shape entirely untested | `src/lib.rs:169` |
| C4 | CRITICAL | `from_file()` and `ConfigError` IO/parse paths untested | `src/lib.rs:38` |
| C5 | GAP | Omitted `can_spawn` default semantics untested | `src/lib.rs:68` |
| C6 | GAP | `skills` allow-list behavior untested | `src/lib.rs:60` |
| C7 | WEAK | Only test is one kitchen-sink happy path | `src/lib.rs:214` |
| C8 | SMELL | Assertions just echo literals from input TOML | `src/lib.rs:214` |

### simulacra-context (3 CRITICAL, 4 GAP, 2 WEAK)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| X1 | CRITICAL | Fallback test never actually masks any tool result | `src/lib.rs:291` |
| X2 | CRITICAL | S015 compaction test doesn't hit real strategies | `simulacra-cli/tests/s015_interactive_red.rs:1221` |
| X3 | CRITICAL | Runtime compaction test uses private fake, proves nothing | `simulacra-runtime/src/agent_loop.rs:1218` |
| X4 | GAP | No-system-message branch untested | `src/lib.rs:52` |
| X5 | GAP | System prompt exceeding budget preservation untested | `src/lib.rs:43` |
| X6 | GAP | Token boundary math (div_ceil(4)) untested at boundaries | `src/lib.rs:20` |
| X7 | GAP | ObservationMasking system-over-budget fallback untested | `src/lib.rs:156` |
| X8 | WEAK | Sliding window test doesn't assert exact length/order | `src/lib.rs:200` |
| X9 | WEAK | Masking tests only check `starts_with("[output elided")` | `src/lib.rs:233` |

### simulacra-vfs (4 CRITICAL, 4 GAP, 4 WEAK)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| V1 | CRITICAL | S020 project-root and source-resolution zero coverage | `src/mount.rs:80` |
| V2 | CRITICAL | Auto-mount and system-prompt mounting untested | `src/mount.rs:309` |
| V3 | CRITICAL | `copy_host_dir_to_vfs` (recursive copy, limits, symlinks) untested | `src/mount.rs:170` |
| V4 | CRITICAL | Mount observability (spans, events, thresholds) untested | `src/mount.rs:326` |
| V5 | GAP | `normalize()` only tested through one read path | `src/path.rs:3` |
| V6 | GAP | Core MemoryFs error branches untested (write "/", read dir, etc.) | `src/memory.rs:67` |
| V7 | GAP | Overlay `list_dir` merge semantics untested | `src/overlay.rs:100` |
| V8 | GAP | Snapshot/restore malformed-data error path untested | `src/memory.rs:185` |
| V9 | WEAK | Whiteout-restore test checks only one symptom | `src/tests.rs:438` |
| V10 | WEAK | Concurrency test barely stresses concurrency | `src/tests.rs:471` |
| V11 | WEAK | Observability coverage is MemoryFs-only, not overlay | `src/tests.rs:320` |
| V12 | WEAK | `metadata()` is happy-path only | `src/tests.rs:306` |

### simulacra-shell (3 CRITICAL, 2 GAP, 3 WEAK, 2 SMELL)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| SH1 | CRITICAL | `${VAR}` brace-style expansion untested | `src/executor.rs:213` |
| SH2 | CRITICAL | Counter test never observes an actual counter | `src/tests.rs:331` |
| SH3 | CRITICAL | Virtual stdout/stderr invariant has no test | `src/tests.rs:105` |
| SH4 | GAP | Redirect failure paths untested | `src/executor.rs:115` |
| SH5 | GAP | Parser has no direct or property tests | `src/parser.rs:77` |
| SH6 | WEAK | Pipeline parent-span test checks names not lineage | `src/tests.rs:357` |
| SH7 | WEAK | `wc` test passes with wrong output (`starts_with("3")`) | `src/tests.rs:537` |
| SH8 | WEAK | `find` test only checks substrings | `src/tests.rs:554` |
| SH9 | SMELL | "never touches real filesystem" test touches real `/tmp` | `src/tests.rs:655` |
| SH10 | SMELL | Observability tests use global mutable state + suite lock | `src/tests.rs:110` |

### simulacra-quickjs (8 CRITICAL, 2 GAP, 2 WEAK, 1 SMELL)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| Q1 | CRITICAL | Native-module registration is source-scanning (R004) | `tests/s016_native_modules_red.rs:8` |
| Q2 | CRITICAL | Console real-stdout test never observes real stdout | `src/tests.rs:571` |
| Q3 | CRITICAL | "Native not polyfills" test can't distinguish native from JS wrapper | `src/tests.rs:588` |
| Q4 | CRITICAL | "Via host proxy" tests never install a proxy | `src/tests.rs:1003` |
| Q5 | CRITICAL | Process default-export test never covers `cwd()` | `tests/s016_native_modules_red.rs:343` |
| Q6 | CRITICAL | Runtime-cache test confounded by import dedup | `src/tests.rs:1227` |
| Q7 | CRITICAL | Fetch-counter-on-cache-miss has same confounder | `src/tests.rs:1587` |
| Q8 | CRITICAL | Spec-cited proxy/capability tests are `#[ignore]`d | `src/tests.rs:1125` |
| Q9 | GAP | `existsSync`/`mkdirSync` never tested through proxy path | `tests/s016_native_modules_red.rs:198` |
| Q10 | GAP | Namespace behavior only tested for `simulacra:fs` | `tests/s016_native_modules_red.rs:363` |
| Q11 | WEAK | "No extra spans" test only compares named operations | `tests/s016_native_modules_red.rs:577` |
| Q12 | SMELL | Tests coupled to concrete `MemoryFs` instead of `VirtualFs` trait | `tests/s016_native_modules_red.rs:231` |

### simulacra-sandbox (6 CRITICAL, 2 GAP, 2 WEAK, 1 SMELL)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| SB1 | CRITICAL | `read_file` journaling has zero coverage | `tests/s011_sandbox_red.rs:858` |
| SB2 | CRITICAL | `read_file` budget enforcement has zero coverage | `tests/s011_sandbox_red.rs:691` |
| SB3 | CRITICAL | Capability-denial tests never verify "no budget consumed" | `tests/s011_sandbox_red.rs:691` |
| SB4 | CRITICAL | Module-fetch delegation test is false confidence | `tests/s011_sandbox_red.rs:1846` |
| SB5 | CRITICAL | JS host-function "routes through AgentCell" doesn't prove routing | `tests/s011_sandbox_red.rs:1714` |
| SB6 | CRITICAL | "Budget exhausted does not execute" doesn't prove non-execution | `tests/s011_sandbox_red.rs:845` |
| SB7 | GAP | `list_dir` denied path and "no journal" invariant untested | `tests/s011_sandbox_red.rs:1477` |
| SB8 | GAP | Path-capability security edges (traversal, normalization) untested | `tests/s011_sandbox_red.rs:1052` |
| SB9 | WEAK | Journal-entry assertions use substring matching | `tests/s011_sandbox_red.rs:934` |
| SB10 | WEAK | ModuleFetcher-ownership test only checks absence of one string | `tests/s011_sandbox_red.rs:1822` |
| SB11 | SMELL | Bespoke fake journals instead of `InMemoryJournalStorage` | `tests/s011_sandbox_red.rs:237` |

### simulacra-tool (5 CRITICAL, 3 GAP, 3 WEAK, 2 SMELL)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| FT1 | CRITICAL | Entire `SkillTool` surface has zero crate-local coverage | `src/lib.rs:550` |
| FT2 | CRITICAL | Skill frontmatter parsing and discovery untested | `src/lib.rs:759` |
| FT3 | CRITICAL | `spawn_agent` tests use a fake tool defined in-test | `tests/s018_spawn_tool_red.rs:8` |
| FT4 | CRITICAL | Failed `spawn_agent` boundary behavior untested | `tests/s018_spawn_tool_red.rs:301` |
| FT5 | CRITICAL | Successful `spawn_agent` assertions pass for any stub | `tests/s018_spawn_tool_red.rs:272` |
| FT6 | GAP | `file_edit` missing `old_string`/`new_string` error paths untested | `tests/s012_builtins_red.rs:499` |
| FT7 | GAP | `list_dir` on existing file untested | `tests/s012_builtins_red.rs:695` |
| FT8 | GAP | Capability-denial only tested for `file_read` | `tests/s012_builtins_red.rs:732` |
| FT9 | WEAK | Schema validation only checks shallowest shape | `tests/s012_builtins_red.rs:359` |
| FT10 | WEAK | `list_dir` output-format tests too loose | `tests/s012_builtins_red.rs:680` |
| FT11 | WEAK | `js_exec` only covers one success branch | `tests/s012_builtins_red.rs:645` |
| FT12 | SMELL | Tests against `MemoryFs` directly instead of `VirtualFs` trait | `tests/s012_builtins_red.rs:85` |
| FT13 | SMELL | `spawn_agent` fixture uses time-based IDs | `tests/s018_spawn_tool_red.rs:86` |

### simulacra-runtime (8 CRITICAL, 1 GAP, 2 WEAK)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| R1 | RESOLVED | `FileJournalStorage` stub deleted; source-scanning test removed; SQLite wired as default | -- |
| R2 | CRITICAL | "Journal before return" test doesn't prove ordering | `src/agent_loop.rs:1144` |
| R3 | CRITICAL | S005 cites tests that don't exist in simulacra-runtime | `specs/S005-journal.md:31` |
| R4 | CRITICAL | Replay tool error state test never runs replay | `src/agent_loop.rs:1571` |
| R5 | CRITICAL | Real spawn-agent path not tested e2e | `tests/s018_subagent_red.rs:693` |
| R6 | CRITICAL | Spawn-agent result shape tests use fakes, not real tool | `tests/s018_subagent_red.rs:1014` |
| R7 | CRITICAL | Subagent journal ordering tests don't check before/after order | `tests/s018_subagent_red.rs:1710` |
| R8 | CRITICAL | S019 activity events suite is pure source-scanning (R004) | `tests/s019_activity_events_red.rs:8` |
| R9 | GAP | No backward-compat test for older journal schemas | `src/journal.rs:23` |
| R10 | WEAK | `query_token_usage` tests don't verify "without loading full journal" | `src/lib.rs:1195` |
| R11 | WEAK | Roundtrip tests check counts not fidelity | `src/lib.rs:1168` |

### simulacra-provider (3 CRITICAL, 4 GAP, 2 WEAK, 1 SMELL)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| P1 | CRITICAL | Backend-selection test is source-scanning (R004) | `src/anthropic/client.rs:971` |
| P2 | CRITICAL | All o11y tests are Anthropic-only; OpenAI has zero | `specs/S007-provider.md:30` |
| P3 | CRITICAL | OpenAI missing `server.address`, `server.port`, `finish_reasons` | `src/openai/mod.rs` |
| P4 | GAP | OpenAI request serialization branches mostly untested | `tests/s007_openai_red.rs:426` |
| P5 | GAP | Streaming tests only cover plain text, not tool use | `tests/s007_openai_red.rs:624` |
| P6 | GAP | Parser failure paths (empty choices, malformed JSON) barely exercised | `src/openai/mod.rs:220` |
| P7 | GAP | Standard 5xx retryability not proven across both backends | `specs/S007-provider.md:23` |
| P8 | WEAK | "Required gen_ai attributes" test overclaims coverage | `src/anthropic/client.rs:1179` |
| P9 | WEAK | Error-log tests only assert loose substrings | `src/anthropic/client.rs:1760` |
| P10 | SMELL | OpenAI tests rely on process-global env mutation + static mutex | `tests/s007_openai_red.rs:19` |

### simulacra-mcp (4 CRITICAL, 4 GAP, 1 WEAK, 1 SMELL)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| M1 | CRITICAL | Child-process prohibition is source-scanning (R004) | `tests/s008_mcp_red.rs:952` |
| M2 | CRITICAL | Initialize-then-tools ordering not actually asserted | `tests/s008_mcp_red.rs:1151` |
| M3 | CRITICAL | "Journal before return" test doesn't verify ordering | `tests/s008_mcp_red.rs:1409` |
| M4 | CRITICAL | Capability tests use non-spec token shapes | `tests/s008_mcp_red.rs:38` |
| M5 | GAP | `notifications/initialized` has zero coverage | `specs/S008-mcp.md:15` |
| M6 | GAP | Multi-server behavior untested | `specs/S008-mcp.md:9` |
| M7 | GAP | SSE laziness not verified | `specs/S008-mcp.md:12` |
| M8 | GAP | Protocol-error and malformed-response paths barely exercised | `tests/s008_mcp_red.rs:1058` |
| M9 | WEAK | Counter test proves existence, not "once per call" | `tests/s008_mcp_red.rs:1546` |
| M10 | SMELL | Custom `FakeJournalStorage` instead of repo-standard fake | `tests/s008_mcp_red.rs:48` |

### simulacra-cli (9 CRITICAL, 3 GAP)

| # | Sev | Finding | Location |
|---|-----|---------|----------|
| CL1 | CRITICAL | S019 activity tests are pure source-scanning (R004) | `tests/s019_activity_cli_red.rs:8` |
| CL2 | CRITICAL | Default-model test locks in wrong spec value | `tests/s013_cli_red.rs:531` |
| CL3 | CRITICAL | "OTLP flush" test doesn't observe real flush | `tests/s013_cli_red.rs:1000` |
| CL4 | CRITICAL | Interactive behavioral claims bypass the real REPL loop | `src/interactive/session.rs:812` |
| CL5 | CRITICAL | Default session-storage backend wiring untested | `tests/s015_interactive_red.rs:1325` |
| CL6 | CRITICAL | Non-TTY scripted mode test never exercises stdin handling | `tests/s015_interactive_red.rs:1432` |
| CL7 | CRITICAL | Rate-limit retry test doesn't test retries | `tests/s015_interactive_red.rs:1366` |
| CL8 | CRITICAL | Budget-warning appearance change not tested | `tests/s015_interactive_red.rs:1172` |
| CL9 | CRITICAL | Terminal-restore coverage is a stub | `tests/s015_interactive_red.rs:1468` |
| CL10 | GAP | Terminal resize reflow untested | `specs/S015-interactive.md:129` |
| CL11 | GAP | Slash commands during in-flight request untested | `specs/S015-interactive.md:130` |
| CL12 | GAP | Cancellation o11y only covers `llm_request`, not `tool_execution`/`session` | `tests/s015_interactive_red.rs:1579` |

---

## Gemini Pro 3 Cross-Review

Second adversarial pass by Gemini Pro 3. Findings are annotated with whether they **agree** with GPT-5.4 (strengthening confidence) or are **new** (caught only by Gemini).

### simulacra-types (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GT1 | CRITICAL | Journal test is source-scanning (R004) | **Agrees** with T1 |
| GT2 | CRITICAL | `check_path_read/write` and path matching untested | **Agrees** with T3 |
| GT3 | CRITICAL | `is_subset_of` for capability fields untested | **Agrees** with T4 |
| GT4 | CRITICAL | `ProviderError::classify()` / `is_retryable()` zero local tests | **Agrees** with T2 |
| GT5 | **NEW** | `is_subset_of` uses exact string matching for `mcp_tools`/`paths_read`/`paths_write` — breaks wildcard/glob support | **Implementation bug** — not just a test gap |

### simulacra-config (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GC1 | CRITICAL | `build_capability_token()` completely unverified | **Agrees** with C1 |
| GC2 | CRITICAL | VFS defaults and mount config untested | **Agrees** with C2, C3 |
| GC3 | CRITICAL | `from_file()` and error paths untested | **Agrees** with C4 |

*(Gemini output was truncated — partial coverage only)*

### simulacra-context (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GX1 | CRITICAL | Fallback test never masks a tool result | **Agrees** with X1 |
| GX2 | GAP | No-system-message branch untested | **Agrees** with X4 |
| GX3 | GAP | System overflow preservation untested | **Agrees** with X5 |
| GX4 | GAP | Token boundary math untested at boundaries | **Agrees** with X6 |
| GX5 | **NEW** | Code duplication between `ObservationMaskingStrategy` fallback and `SlidingWindowStrategy` | SMELL — architectural, not test gap |

### simulacra-vfs (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GV1 | CRITICAL | S020 mount.rs has zero test coverage | **Agrees** with V1-V4 |
| GV2 | GAP | Overlay `list_dir` merge semantics untested | **Agrees** with V7 |
| GV3 | **NEW** | `std::fs` dependency in `mount.rs` prevents R004-compliant testing | Architectural smell — needs trait extraction |
| GV4 | **NEW** | Overlay type-conflict handling gap (file vs dir at same path) | New edge case |

### simulacra-shell (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GS1 | CRITICAL | `${VAR}` brace-style expansion untested | **Agrees** with SH1 |
| GS2 | GAP | Parser has no direct or property tests | **Agrees** with SH5 |
| GS3 | **NEW BUG** | Escaped quotes not handled in parser | Implementation bug |
| GS4 | **NEW BUG** | Single quotes don't suppress variable expansion | Implementation bug |
| GS5 | **NEW BUG** | Pipeline stderr/exit codes discarded silently | Implementation bug |

### simulacra-quickjs (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GQ1 | CRITICAL | Native-module registration is source-scanning (R004) | **Agrees** with Q1 |
| GQ2 | CRITICAL | "Via host proxy" tests never install a proxy | **Agrees** with Q4 |
| GQ3 | CRITICAL | Spec-cited proxy/capability tests are `#[ignore]`d | **Agrees** with Q8 |
| GQ4 | **NEW** | Inconsistent `resolve_relative` path behavior when traversing past root | Edge case |
| GQ5 | **NEW** | Native modules coupled to legacy globals | Architectural smell |

### simulacra-sandbox (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GSB1 | CRITICAL | `list_dir` only tests success path — no test for capability denial | **NEW** — GPT-5.4 had SB7 as GAP, Gemini escalates to CRITICAL |
| GSB2 | GAP | `read_file` budget enforcement has zero coverage | **Agrees** with SB2 |
| GSB3 | GAP | `list_dir` denied path untested | **Agrees** with SB7 |
| GSB4 | GAP | No test verifies journal entry kind for successful `read_file` (spec §8) | **NEW** |
| GSB5 | GAP | No test verifies journal write ordering relative to VFS execution (spec §6: writes journal before, reads after) | **NEW** |
| GSB6 | GAP | "Before execution" timing test for JS doesn't prove what it claims — tests nested fetch budget check, not "increment before execution" | **NEW** — finding #4 from retry |
| GSB7 | WEAK | Journal-entry assertions use loose string matching on serialized JSON instead of structural `matches!()` | **Agrees** with SB9 |
| GSB8 | WEAK | Module-fetch delegation test only checks `used_turns` increment — `execute_js` always increments +1 before execution, so test passes even if fetch never happens (needs to check +2) | **NEW** — strengthens SB4 with specific mechanism |
| GSB9 | WEAK | ModuleFetcher ownership test only checks absence of one string | **Agrees** with SB10 |
| GSB10 | WEAK | `shell_command_not_found` assertion depends on platform-specific error message | **NEW** |
| GSB11 | WEAK | `fetch_http_network_error` has TOCTOU race on port binding and platform-dependent error text | **NEW** |
| GSB12 | WEAK | `fs_readFileSync`/`writeFileSync` from JS only test denial path, never success path (no journal/span verification) | **NEW** |
| GSB13 | SMELL | `ExpectedSandboxError` mapping silently converts `Http` → `Shell` and `Internal` → `Shell`, could mask real issues | **NEW** |
| GSB14 | SMELL | `AgentCellHttpExt` trait with stub default impl is dead code — never used by any test | **NEW** |
| GSB15 | SMELL | `capability_denials_increment_counter` creates unnecessary HTTP server that's never contacted | **NEW** |
| GSB16 | — | No R004 source-scanning violations found | **Agrees** — clean |

### simulacra-tool (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GFT1 | CRITICAL | **Entire `s018_spawn_tool_red.rs` tests a local mock — every behavioral assertion tests hardcoded fake, not real `SpawnAgentTool`** (5 CRITICAL tests identified individually) | **Agrees** with FT3-FT5 — Gemini enumerated each: success shape, failure shape, budget_exhausted, empty message all prove nothing |
| GFT2 | GAP | `SpawnAgentTool` implementation may be missing from `simulacra-tool/src/lib.rs` entirely | **NEW** |
| GFT3 | GAP | No test for `list_dir` without `path` argument (`InvalidArguments` case) | **NEW** — every other tool has this negative test |
| GFT4 | GAP | Capability-denial only tested for `file_read` — no `file_write`, `shell_exec`, `js_exec`, `list_dir` denial tests | **Agrees** with FT8 — Gemini more specific |
| GFT5 | GAP | No `file_write` budget exhaustion test for `max_turns` (only `max_vfs_bytes` tested) | **NEW** |
| GFT6 | GAP | No test for missing required S018 arguments (`agent_type`, `task`, `budget`) | **NEW** |
| GFT7 | GAP | No capability attenuation test at `SpawnAgentTool::call` level (`can_spawn` config) | **NEW** |
| GFT8 | WEAK | `register_builtins` count assertion is brittle — breaks when tools are added with no regression | **NEW** |
| GFT9 | WEAK | Tool definition tests assert hardcoded description strings — dangerously close to testing literals | **NEW** |
| GFT10 | WEAK | `file_write` byte count assertion uses `contains('6')` — single char, could match path | **NEW** |
| GFT11 | WEAK | `shell_exec` tests run real `echo` on host, bypassing `MemoryFs` isolation | **NEW** |
| GFT12 | SMELL | S018 definition/schema tests exercise `PendingSpawnAgentTool`, not the real tool | **Agrees** with FT5 |
| GFT13 | — | S012 builtin tests are **solid** — architecture correct (ToolRegistry → AgentCell → MemoryFs) | Positive signal |
| GFT14 | — | No R004 source-scanning violations | Clean |

### simulacra-runtime (Gemini)

**S019 Activity Events:** All 17 tests are R004 source-scanning violations — zero behavioral coverage.

**S018 Subagent:** Solid quality (44 tests), mostly behavioral with real objects. Some gaps:

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GR1 | CRITICAL | **All 17 S019 tests are source-scanning** — `crate_sources()` loads `.rs` files, `assert_contains_all()` checks for substrings. Would pass if strings appeared only in comments. | **Agrees** with R8 — Gemini enumerated all 17 individually |
| GR2 | GAP | Journal ordering not verified — `SubAgentSpawned` and `SubAgentCompleted` both exist by assertion time, but order not proven | **Agrees** with R7 |
| GR3 | GAP | Only `budget_exhausted` exit reason tested for snake_case — `"completed"` and `"max_turns"` exit reasons untested | **NEW** |
| GR4 | GAP | No test for `ToolOutput` streaming events (S019 spec item 17) | **NEW** |
| GR5 | GAP | No test for `ThinkStart`/`ThinkDelta`/`ThinkEnd` emission from agent loop (S019 spec items 12-14) | **NEW** |
| GR6 | GAP | No test for auto-approval (spec item 5b) — `spawn_agent` should be auto-approved without user confirmation | **NEW** |
| GR7 | WEAK | `failed_spawn_agent_calls_return_error_tool_results` uses string containment instead of structured JSON parsing | **NEW** |
| GR8 | WEAK | `SummarySpawnTool` and `ErrorSpawnTool` are fake tools substituting for real `SpawnAgentTool` in some tests | **Agrees** with R5, R6 |
| GR9 | SMELL | Environment variable mutation with `unsafe { std::env::set_var(...) }` and `OPENAI_ENV_MUTEX` is inherently racy | **NEW** |
| GR10 | — | S018 tests are **solid** — 44 tests constructing real `AgentSupervisor`, `AgentTaskFactory`, `SpawnAgentTool`, `AgentLoop` | Positive signal |

### simulacra-provider (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GP1 | CRITICAL | Retry test only verifies error *mapping* (429→RateLimit), not that provider actually retries | **NEW** — GPT-5.4 caught retryability gap (P7) but Gemini is more specific: test is false confidence |
| GP2 | CRITICAL | No explicit 500 ServerError test despite spec requiring it | **Agrees** with P7 (strengthens) |
| GP3 | WEAK | Streaming test doesn't verify incremental chunk delivery — blocking impl would pass | **Agrees** with P5 (strengthens) |
| GP4 | GAP | Zero observability tests for OpenAI backend | **Agrees** with P2 |
| GP5 | GAP | Provider selection by configuration untested | **NEW** |
| GP6 | SMELL | Hardcoded timestamps without clock injection | **NEW** — minor |

### simulacra-mcp (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GM1 | CRITICAL | Source-scanning: `simulacra_mcp_never_uses_child_process_commands` reads lib.rs as string | **Agrees** with M1 |
| GM2 | GAP | `notifications/initialized` not sent in handshake test despite spec requiring it | **Agrees** with M5 |
| GM3 | WEAK | Schema bridging test only checks `required` field, not `properties`/`type` | **NEW** — schema could be mangled and test passes |
| GM4 | WEAK | Tool call event assertions use substring matching on JSON | **NEW** — structured data verified with `contains()` |
| GM5 | WEAK | Connection failure error assertion too loose — any error passes | **NEW** |

### simulacra-cli (Gemini)

| # | Sev | Finding | Agreement |
|---|-----|---------|-----------|
| GCL1 | CRITICAL | **All 13 tests in `s019_activity_cli_red.rs` are R004 source-scanning violations** — read source files to find string patterns, never execute code | **Agrees** with CL1 — Gemini enumerated all 13 individually |
| GCL2 | GAP | Headless mode `run()` execution untested — tests verify arg parsing but never run the agent loop | **NEW** |
| GCL3 | GAP | VFS host mounts from `[[vfs.mounts]]` config processing untested | **NEW** — spec assertion with zero coverage |
| GCL4 | GAP | Project root detection from `--config` path untested | **NEW** — spec assertion with zero coverage |
| GCL5 | GAP | Terminal resize/reflow untested — `TestIo` doesn't simulate resize events | **Agrees** with CL10 |
| GCL6 | GAP | All S019 behavioral assertions effectively untested due to invalid test suite | **Agrees** with CL1 — systemic consequence |
| GCL7 | GAP | VFS restoration test (`resumed_session_restores...`) — nothing in setup writes "persisted file" to VFS, test proves mock works, not real restore | **NEW** |
| GCL8 | GAP | Terminal panic recovery calls `simulate_terminal_restore(true)` — real panic unwind not tested, drop-guard behavior unverified | **NEW** — strengthens CL9 |
| GCL9 | WEAK | Interactive mode integration only tests startup, not session loop driving | **Agrees** with CL4 |
| GCL10 | WEAK | `interactive_mode_uses_the_shared_runtime_agent_loop_type` is a type identity check (`type_name`), not behavioral | **NEW** |
| GCL11 | WEAK | `awaiting_approval_exit_reason` just tests a constant getter | **NEW** |
| GCL12 | WEAK | `reuses_headless_bootstrap_path` is a boolean getter, not runtime behavior proof | **NEW** |
| GCL13 | WEAK | History navigation test doesn't verify boundary cases (past oldest/newest entry) | **NEW** |
| GCL14 | WEAK | `cli_shutdown_flushes_otlp` checks self-reported boolean, not actual OTLP flush | **Agrees** with CL3 |
| GCL15 | SMELL | Many S015 tests call test-only APIs (`handle_tool_approval`, `simulate_terminal_restore`) — testing harness, not real interactive flow | **NEW** — systemic |
| GCL16 | SMELL | Global tracing subscriber via `OnceLock` shared across tests — may cause interference | **NEW** |
| GCL17 | — | S013 tests are **generally solid** — use `FakeProvider`, exercise real `bootstrap()` and `run_with_provider()` | Positive signal |

---

## Cross-Model Agreement Summary

High-confidence findings (both models agree):

| Crate | Finding | GPT-5.4 | Gemini |
|-------|---------|---------|--------|
| simulacra-types | Source-scanning R004 | T1 | GT1 |
| simulacra-types | Path capability untested | T3 | GT2 |
| simulacra-types | `is_subset_of` untested | T4 | GT3 |
| simulacra-config | `build_capability_token` unverified | C1 | GC1 |
| simulacra-config | VFS defaults untested | C2,C3 | GC2 |
| simulacra-context | Fallback test is false confidence | X1 | GX1 |
| simulacra-vfs | mount.rs zero coverage | V1-V4 | GV1 |
| simulacra-shell | Brace expansion untested | SH1 | GS1 |
| simulacra-quickjs | Source-scanning R004 | Q1 | GQ1 |
| simulacra-quickjs | Proxy tests never install proxy | Q4 | GQ2 |
| simulacra-provider | 5xx retryability not proven | P7 | GP2 |
| simulacra-provider | OpenAI o11y zero coverage | P2 | GP4 |
| simulacra-provider | Streaming doesn't prove incremental delivery | P5 | GP3 |
| simulacra-mcp | Source-scanning R004 | M1 | GM1 |
| simulacra-mcp | `notifications/initialized` untested | M5 | GM2 |
| simulacra-sandbox | `read_file` budget enforcement zero coverage | SB2 | GSB2 |
| simulacra-sandbox | Journal assertions use loose string matching | SB9 | GSB7 |
| simulacra-sandbox | ModuleFetcher delegation is false confidence | SB4 | GSB8 |
| simulacra-tool | `s018_spawn_tool_red.rs` tests a local mock, not real code | FT3 | GFT1 |
| simulacra-tool | Capability-denial only tested for `file_read` | FT8 | GFT4 |
| simulacra-runtime | S019 all source-scanning | R8 | GR1 |
| simulacra-runtime | Journal ordering not verified | R7 | GR2 |
| simulacra-runtime | Spawn-agent uses fake tools, not real | R5,R6 | GR8 |
| simulacra-cli | S019 activity tests are all source-scanning | CL1 | GCL1 |
| simulacra-cli | Terminal resize untested | CL10 | GCL5 |
| simulacra-cli | Interactive mode doesn't test session loop | CL4 | GCL9 |
| simulacra-cli | OTLP flush is self-reported | CL3 | GCL14 |
| simulacra-cli | Terminal restore is a stub | CL9 | GCL8 |

Gemini-only new findings (potential implementation bugs):
- **simulacra-types GT5:** `is_subset_of` exact string matching breaks glob support
- **simulacra-shell GS3-GS5:** Three parser/pipeline bugs (escaped quotes, single quotes, stderr)
- **simulacra-vfs GV3:** `std::fs` in mount.rs prevents testability
- **simulacra-quickjs GQ4:** `resolve_relative` path traversal past root
- **simulacra-provider GP1:** Retry test is false confidence — only checks error mapping, not retry behavior
- **simulacra-provider GP5:** Provider selection by configuration untested
- **simulacra-mcp GM3:** Schema bridging only checks `required`, not `properties`/`type`
- **simulacra-mcp GM4-GM5:** Substring matching on structured JSON data
- **simulacra-sandbox GSB1:** `list_dir` capability denial untested (escalated from GAP to CRITICAL)
- **simulacra-sandbox GSB7:** Module-fetch delegation test passes even if fetch never happens (`execute_js` always increments `used_turns`)
- **simulacra-tool GFT2:** `SpawnAgentTool` implementation may be entirely missing from `simulacra-tool`
- **simulacra-tool GFT3-GFT7:** Missing `list_dir` invalid-args test, limited capability denial coverage, no `file_write` budget test for `max_turns`, missing S018 required-args and `can_spawn` tests
- **simulacra-tool GFT8-GFT11:** Brittle count assertion, hardcoded description strings, weak byte-count match, `shell_exec` bypasses MemoryFs
- **simulacra-sandbox GSB4-GSB6:** Missing `read_file` journal entry test, missing journal-ordering-vs-VFS-execution test, JS "before execution" timing test doesn't prove its claim
- **simulacra-sandbox GSB8:** Module-fetch delegation needs +2 check (not +1) since `execute_js` itself increments
- **simulacra-sandbox GSB13-GSB14:** `ExpectedSandboxError` silently converts Http→Shell; `AgentCellHttpExt` is dead code
- **simulacra-runtime GR3:** Only `budget_exhausted` exit reason tested — `"completed"` and `"max_turns"` untested
- **simulacra-runtime GR4-GR6:** No `ToolOutput` streaming, no ThinkStart/Delta/End emission, no auto-approval tests
- **simulacra-runtime GR9:** `unsafe { std::env::set_var }` with mutex is inherently racy
- **simulacra-cli GCL2:** Headless mode `run()` never executed in tests
- **simulacra-cli GCL3-GCL4:** VFS host mounts and project root detection are spec assertions with zero coverage
- **simulacra-cli GCL7:** VFS restoration test proves mock works, not real restore
- **simulacra-cli GCL10-GCL12:** Type identity checks and constant getters masquerading as behavioral tests
- **simulacra-cli GCL15:** S015 tests use test-only APIs, not real interactive flow

---

## Recommended Fix Order

### Wave 1: Source-scanning eradication (S1)
Delete all `include_str!`/source-scanning tests and replace with behavioral equivalents.
**Crates:** simulacra-types, simulacra-quickjs, simulacra-runtime, simulacra-cli, simulacra-provider, simulacra-mcp

### Wave 2: Zero-coverage spec surfaces
Write tests for entirely untested production code:
- **simulacra-types:** path capability enforcement (T3), provider error classification (T2)
- **simulacra-config:** `build_capability_token()` (C1), VFS/mount defaults (C2, C3)
- **simulacra-vfs:** mount.rs (V1-V4) — entire S020 surface
- **simulacra-tool:** SkillTool (FT1, FT2)
- **simulacra-sandbox:** `read_file` journaling + budget (SB1, SB2)

### Wave 3: False-confidence replacements
Replace tests that claim to verify behavior they don't actually exercise:
- **simulacra-quickjs:** proxy tests (Q4), cache tests (Q6, Q7), un-ignore spec tests (Q8)
- **simulacra-runtime:** spawn-agent e2e (R5, R6), journal ordering (R2, R7)
- **simulacra-cli:** REPL loop e2e (CL4), retry behavior (CL7)
- **simulacra-mcp:** initialize ordering (M2), journal ordering (M3)

### Wave 4: Edge cases and hardening
Fill in GAPs and strengthen WEAK tests:
- Context strategy boundary math and fallback paths
- Shell parser property tests
- Provider streaming tool-use and error paths
- Sandbox path-traversal security edges
