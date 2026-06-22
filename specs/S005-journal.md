# S005 — Journal

**Status:** Active
**Crate:** `simulacra-runtime`

## Behavior

1. The journal is append-only. Entries are never modified or deleted.
2. Every side-effecting operation produces a `JournalEntry` **before** the result returns to the agent.
3. Checkpoints are periodic full-state snapshots (VFS + conversation + budget counters). They are NOT taken every turn — journal entries between checkpoints are deltas.
4. **Replay:** Walk the journal. For each entry, check "has this step been executed?" If yes, substitute the recorded result. If no (frontier reached), execute live. (Restate pattern.)
5. **Fork:** Given a checkpoint index, create a new journal that starts from that checkpoint's state. The new journal can diverge from the original. (LangGraph pattern.)
6. Journal entries are schema-versioned. The `schema_version` field increments when the `JournalEntry` enum changes. Older journals must be readable by newer runtimes.
7. `JournalStorage` is a trait with pluggable backends (InMemory, File, SQLite).

## Entry Types

```
TurnStart, LlmRequest, LlmResponse, ToolCall, ShellCommand,
CodeExecution, SubAgentSpawned, SubAgentCompleted, FileWrite, HttpRequest
```

## Assertions

- [x] Append + load roundtrip preserves all entries. **Tested in `journal_append_and_read_all_roundtrip`.**
- [x] Checkpoint + fork creates an independent journal that shares history up to the fork point. **Tested in `checkpoint_fork_creates_independent_journal` and `fork_from_checkpoint_creates_storage_independent_of_original_mutations`.**
- [x] Replay with recorded LLM response does not make a real API call. **Tested in `replay_with_recorded_llm_response_skips_provider`.**
- [x] Replay from checkpoint skips all entries before the checkpoint. **Tested in `replay_from_checkpoint_skips_earlier_entries`.**
- [x] `query_token_usage` returns accurate running total without loading full journal. **Tested in `journal_query_token_usage` and `journal_query_token_usage_no_entries`.**
- [x] Schema version mismatch produces a clear error, not silent corruption. **Tested in `schema_version_mismatch_produces_error` and `read_from_rejects_schema_mismatch_on_individual_entry`.**
- [x] Journal entries are append-only — no API exists to modify or delete entries. **Tested in `journal_storage_trait_exposes_append_only_api_surface`.**
- [x] Every side-effecting operation in the agent loop writes a journal entry BEFORE returning. **Tested in `journal_entries_written_before_return`, `js_execution_failure_still_records_code_execution_entry_before_return`, and `http_failure_still_records_http_request_entry_before_return`.**
- [x] Replay with recorded ToolResult preserves is_error state. **Test exists (`replay_tool_result_preserves_error_state`) but it only checks serde roundtrip of the entry, not actual replay behavior in the agent loop.**
- [x] SQLite-backed JournalStorage implementation provides file-backed persistence. **Implemented in `journal_sqlite.rs`; wired as the default backend in the CLI. Tested in `file_backed_persistence` and other `SqliteJournalStorage` tests.**
- [x] VFS snapshot is included in checkpoint data and restored on fork. **Tested in `fork_from_checkpoint_restores_vfs_snapshot_state`.**
- [x] Replay divergence (entry kind mismatch) produces an error or logs, does not silently continue. **Tested in o11y tests (`replay_divergence_is_logged_at_error`) but the agent loop does not actually halt on divergence — it just logs.**

## Observability (see S010 for conventions)

- [x] Journal append produces a span with `simulacra.operation.name` = `journal_append` and `simulacra.journal.entry_kind`. **Tested in `journal_append_span_records_entry_kind_and_live_mode`.**
- [x] Replay entries are tagged with `simulacra.journal.mode` = `replayed`; live entries with `live`. **Tested in `replayed_journal_entries_are_tagged_replayed` and `journal_append_span_records_entry_kind_and_live_mode`.**
- [x] `simulacra.journal.entries` counter tracks entries by kind. **Tested in `journal_entries_counter_tracks_entries_by_kind`.**
- [x] `simulacra.journal.replay.ratio` gauge reports fraction of entries replayed vs. total. **Tested in `journal_replay_ratio_gauge_reports_fraction_replayed`.**
- [x] Replay divergence (expected entry kind differs from actual) is logged at `ERROR`. **Tested in `replay_divergence_is_logged_at_error`.**
- [x] Schema version mismatch is logged at `ERROR` with expected and found versions. **Tested in `schema_version_mismatch_is_logged_at_error_with_expected_and_found_versions`.**
