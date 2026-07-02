# S015 — Interactive Mode

**Status:** Active
**Crate:** `simulacra-cli`

## Dependencies

- **S005** — Journal (session persistence via journal checkpoints)
- **S006** — Resource budgets (surfacing budget state to the user)
- **S007** — Provider (streaming responses for token-by-token display)
- **S009** — Supervisor (agent cancellation on user interrupt)
- **S011** — Sandbox composition (tool calls flow through AgentCell proxy)
- **S012** — Built-in tools (tool registry, tool definitions shown to the user)
- **S013** — CLI (argument parsing, bootstrap, config loading — S015 extends S013)
- **S049** — Agent Turn Runtime Foundation (runtime-owned turn state and cancellation checks)

## Context

S013 defined headless mode as Phase 1. Interactive mode is Phase 2: a terminal REPL where the user has a multi-turn conversation with an agent. The user types prompts, the agent responds with streamed text and tool calls, the user approves or denies tool use, and the conversation accumulates context across turns.

Interactive mode is built on the same runtime primitives as headless mode — same agent loop, same provider, same tool registry, same journal. The difference is the I/O layer: instead of a single task string piped to stdout, interactive mode reads from stdin, renders streaming output to the terminal via ratatui, and gates tool calls on user approval.

This spec does NOT cover a full TUI layout (panels, split views, etc.). It covers the core interactive loop: prompt input, streaming output, tool approval, cancellation, budget display, and session persistence. The ratatui rendering details are implementation concerns, not behavioral spec.

S049 provides the shared runtime turn structure and cooperative cancellation
checks used by interactive mode. S015 continues to own terminal rendering,
streaming display semantics, approval prompts, and session persistence.

## Design

```
  User types prompt
       │
       ▼
  ┌──────────────────────────────────────────────┐
  │             InteractiveSession                │
  │                                               │
  │  1. Accept user input (readline-style)        │
  │  2. Append User message to conversation       │
  │  3. Call provider (streaming)                 │
  │  4. Render tokens as they arrive              │
  │  5. On tool_use: present approval prompt      │
  │     ├─ approved → execute tool, continue      │
  │     └─ denied → send denial to model, loop    │
  │  6. On end_turn: display final text, prompt   │
  │  7. On Ctrl-C: cancel in-flight request       │
  │  8. On /exit or Ctrl-D: save session, quit    │
  │                                               │
  │  Budget bar: [tokens: 12,340 / 200,000]       │
  │  Turn counter: [turn 3 / 50]                  │
  └──────────────────────────────────────────────┘
```

## Behavior

### Session Startup

1. `simulacra --mode interactive` starts an interactive session. If `--task` is also provided, the task string is sent as the first user message automatically.
2. On startup, the CLI displays: the project name (from config), the model name, and the budget limits (max_tokens, max_turns). This is the session header.
3. If `--session <id>` is provided and a saved session exists with that ID, the session is resumed: prior messages are loaded and the conversation continues from where it left off. The journal is replayed to restore state.
4. If `--session <id>` is provided but no saved session exists, a new session is created with that ID.
5. If `--session` is not provided, a new session is created with a generated UUID. `--session` always requires a value.

### Input Handling

6. The input prompt is displayed as `> ` at the bottom of the terminal. The user types a message and presses Enter to send.
7. Empty input (just Enter) is ignored — no message is sent to the model.
8. Multi-line input is supported via a trailing `\` at the end of a line, which continues input on the next line.
9. Input history is navigable with Up/Down arrow keys within the current session (not persisted across sessions).

### Slash Commands

10. `/exit` or `/quit` gracefully ends the session. The session is saved (if session storage is available) and the process exits with code 0.
11. `/clear` clears the visible terminal output but does NOT clear the conversation history. The model retains full context.
12. `/budget` displays the current budget state: used tokens, remaining tokens, used turns, remaining turns, and estimated cost (if cost tracking is configured).
13. `/tools` lists the currently registered tools by name and description.
14. `/session` displays the current session ID.
15. `/help` lists all available slash commands with brief descriptions.
16. Unknown slash commands display an error message: "unknown command: /<name>. Type /help for available commands."

### Streaming Output

17. When the provider returns a streaming response, tokens are rendered to the terminal as they arrive — not buffered until the full response is complete. This gives the user real-time feedback.
18. The streaming display differentiates assistant text from tool call blocks. Assistant text is rendered inline. Tool calls are rendered as a distinct block showing the tool name and arguments.
19. If the provider does not support streaming (returns a complete response), the full response is displayed at once. The interactive loop does not require streaming — it adapts to what the provider offers.

### Tool Call Approval

20. When the model emits a tool call, execution is paused and the user is presented with an approval prompt showing: the tool name, the arguments (formatted as JSON), and the prompt `[a]pprove / [d]eny / approve [A]ll?`.
21. Approve (`a` then Enter, or just Enter as default) executes the tool and returns the result to the model.
22. Deny (`d` then Enter) does NOT execute the tool. Instead, a tool result with `is_error: true` and content "Tool call denied by user" is returned to the model so it can adjust its approach.
23. Approve All (`A` then Enter) approves the current tool call AND all subsequent tool calls for the remainder of the current assistant turn (until the model produces a final text response or the turn ends). The next user message resets approval to per-call mode.
24a. Invalid input (anything other than `a`, `d`, `A`, or empty Enter) re-displays the approval prompt.
24. If the model emits multiple tool calls in a single response, each is presented for approval sequentially unless Approve All is active.
25. Tool call approval is a user-facing gate only. It does NOT replace capability checks — a tool call that passes user approval can still be denied by the capability token. In that case, the capability denial is surfaced to the user as an error message and the denial result is sent to the model.

### Cancellation

26. Ctrl-C during an in-flight LLM request cancels the request. The partial response (if any tokens were received) is displayed with a `[cancelled]` indicator. The conversation retains any complete messages but discards the partial assistant message. The user can then type a new prompt.
27. Ctrl-C during tool execution cancels the tool (cooperative cancellation per S009). The tool result is `is_error: true` with content "Cancelled by user".
28. Ctrl-C at the input prompt (when no request is in-flight) displays "Press Ctrl-C again to exit, or type /exit". A second Ctrl-C within 2 seconds exits the session gracefully (equivalent to `/exit`).
29. Two rapid Ctrl-C presses (within 500ms) during an in-flight request force-quit the process immediately without saving the session. This is the escape hatch for a hung process that doesn't respond to cooperative cancellation.

### Budget Surfacing

30. A persistent status line at the bottom of the terminal shows: `tokens: <used>/<limit> | turns: <used>/<limit>`. This updates after each LLM response.
31. When budget reaches 80% of any limit, the status line changes color (or adds a warning indicator) to alert the user.
32. When budget is exhausted, the agent loop exits with `ExitReason::BudgetExhausted`. The interactive session displays the exhaustion reason and returns to the input prompt. The user can start a new session or increase the budget in config.

### Multi-turn Conversation

33. Each user message is appended to the conversation history. The full history (system prompt + all user/assistant/tool messages) is sent to the provider on each turn. If the conversation exceeds the provider's context window, older messages are truncated (oldest-first, preserving the system prompt).
34. The conversation accumulates across the entire interactive session. There is no implicit context window — the context strategy handles compaction when the conversation exceeds the token budget.
35. Tool results from previous turns remain in the conversation history and are visible to the model in subsequent turns.

### Session Persistence

36. When the user exits gracefully (`/exit`, `/quit`, Ctrl-D, or double Ctrl-C at prompt), the session is saved. The save format is a journal checkpoint (per S005) written via `SessionStorage`. The checkpoint is the sole source of truth for session state — it contains conversation messages, VFS snapshot, budget counters, and session metadata (status, turn count).
37. A saved session can be resumed with `simulacra --mode interactive --session <id>`. On resume, the journal checkpoint is loaded (not replayed event-by-event — it is a snapshot). The user sees a summary: "Resumed session <id> (N messages, M turns used)".
38. `SessionStorage` trait (from simulacra-runtime) abstracts save/load. The default backend is file-based, stored at `~/.simulacra/sessions/<session-id>/checkpoint.json`.
39. Sessions that have reached budget exhaustion are marked as `exhausted`. Sessions ended with `/exit` are marked as `completed`. Resuming either kind restores conversation history but resets the budget to the configured limits (not the saved budget counters). This lets the user continue a conversation without hitting the same budget wall.

### Error Handling

40. Provider errors (rate limit, auth, server error) are displayed to the user with the error classification and message. Rate-limit errors (429) are retried up to 3 times with exponential backoff (1s, 2s, 4s) and display a "Retrying in Ns..." indicator. Auth errors and other non-retryable errors return to the input prompt immediately.
41. Tool execution errors (SandboxError) are displayed to the user AND sent to the model as error tool results.
42. If the journal write fails, the error is logged at WARN but the session continues. Journal failures are never fatal to the interactive session.

### Terminal Behavior

43a. If stdin is not a TTY (piped input), interactive mode reads all input as a single message and runs one turn, then exits. No approval prompts — all tool calls are auto-approved. This enables scripted usage like `echo "fix the bug" | simulacra --mode interactive`.
43b. Ctrl-D at the input prompt (EOF) is equivalent to `/exit`.
43c. On exit (graceful or forced), the terminal is restored to its original state (raw mode disabled, alternate screen exited if used, cursor visible). This also applies on panic via a drop guard.
43d. Terminal resize events cause the UI to reflow. No content is lost on resize.
43e. Slash commands are only processed at the input prompt. They cannot be issued while a request is in-flight.

### Integration with Agent Loop

43. Interactive mode uses the same `AgentLoop` from simulacra-runtime but drives it one turn at a time rather than running to completion. Each user message triggers one iteration: send messages to provider, process tool calls (with approval gates), return result.
44. The `ExitReason::AwaitingApproval` variant (already defined in simulacra-types) is used when the agent loop yields a tool call that requires user approval. The interactive session handles the approval and resumes the loop.

## Assertions

### Session startup

- [x] `simulacra --mode interactive` starts an interactive session (no error, no "not yet implemented"). **Behavioral test in `interactive_mode_starts_an_interactive_session`.**
- [x] `simulacra --mode interactive --task "hello"` sends "hello" as the first user message automatically. **Behavioral test in `interactive_task_is_sent_as_the_first_user_message`.**
- [x] Session header displays project name, model name, and budget limits. **Behavioral test in `session_header_displays_project_name_model_name_and_budget_limits`.**
- [x] `--session <id>` with an existing session resumes conversation from saved state. **Behavioral test in `existing_session_id_resumes_conversation_from_saved_state`.**
- [x] `--session <id>` with no existing session creates a new session with that ID. **Behavioral test in `missing_session_id_creates_a_new_session_with_the_requested_id`.**
- [x] `--session` requires a value; omitting `--session` generates a UUID session ID. **Behavioral tests in `session_flag_requires_a_value` and `omitting_session_generates_a_uuid_session_id`.**

### Input handling

- [x] Empty input (just Enter) does not send a message to the model. **Behavioral test in `empty_input_does_not_send_a_message_to_the_model`.**
- [x] Multi-line input via trailing `\` concatenates lines into a single message. **Behavioral test in `trailing_backslash_concatenates_multiline_input_into_a_single_message`.**
- [x] Up/Down arrow keys navigate input history within the session. **Behavioral test in `up_and_down_arrows_navigate_input_history_within_the_session`.**

### Slash commands

- [x] `/exit` saves the session and exits with code 0. **Behavioral test in `exit_saves_the_session_and_exits_with_code_zero`.**
- [x] `/clear` clears terminal output but conversation history is retained (model still has context). **Behavioral test in `clear_clears_visible_output_without_discarding_conversation_history`.**
- [x] `/budget` displays current token and turn usage with limits. **Behavioral test in `budget_displays_current_token_and_turn_usage_with_limits`.**
- [x] `/tools` lists all registered tool names and descriptions. **Behavioral test in `tools_lists_registered_tool_names_and_descriptions`.**
- [x] `/session` displays the current session ID. **Behavioral test in `session_displays_the_current_session_id`.**
- [x] `/help` lists all slash commands. **Behavioral test in `help_lists_all_supported_slash_commands`.**
- [x] Unknown slash command displays "unknown command" error. **Behavioral test in `unknown_slash_command_displays_an_unknown_command_error`.**

### Streaming output

- [x] Streaming tokens are rendered incrementally as they arrive (not buffered until complete). **Behavioral test in `streaming_tokens_are_rendered_incrementally_as_they_arrive`.**
- [x] Tool call blocks are visually distinct from assistant text in the output. **Behavioral test in `tool_call_blocks_are_rendered_distinctly_from_assistant_text`.**
- [x] Non-streaming provider responses are displayed in full without error. **Behavioral test in `non_streaming_provider_responses_are_displayed_in_full_without_error`.**

### Tool call approval

- [x] Tool call pauses execution and displays tool name and arguments to the user. **Behavioral test in `tool_call_pauses_execution_and_displays_tool_name_and_arguments`.**
- [x] Approve (Enter or `a`) executes the tool and returns result to the model. **Behavioral test in `approve_executes_the_tool_and_returns_the_result_to_the_model`.**
- [x] Deny (`d`) returns an error tool result "Tool call denied by user" without executing. **Behavioral test in `deny_returns_a_tool_error_result_without_executing_the_tool`.**
- [x] Approve All (`A`) approves current and all subsequent tool calls in the same turn. **Behavioral test in `approve_all_covers_the_current_and_subsequent_tool_calls_in_the_same_turn`.**
- [x] Invalid approval input re-displays the prompt without executing or denying. **Behavioral test in `invalid_approval_input_redisplays_the_prompt_without_executing_or_denying`.**
- [x] Approve All resets to per-call mode on the next user message. **Behavioral test in `approve_all_resets_to_per_call_mode_on_the_next_user_message`.**
- [x] Multiple tool calls in one response are presented sequentially for approval. **Behavioral test in `multiple_tool_calls_are_presented_sequentially_for_approval`.**
- [x] A tool call that passes user approval but fails capability check surfaces the denial to the user and sends denial result to model. **Behavioral test in `capability_denials_are_surfaced_to_the_user_and_sent_back_to_the_model`.**

### Cancellation

- [x] Ctrl-C during LLM request cancels the request and displays `[cancelled]`. **Behavioral test in `ctrl_c_during_llm_request_cancels_the_request_and_displays_cancelled`.**
- [x] Ctrl-C during LLM request preserves complete prior messages but discards the partial response. **Behavioral test in `ctrl_c_during_llm_request_discards_the_partial_response_but_keeps_prior_messages`.**
- [x] Ctrl-C during tool execution cancels the tool with "Cancelled by user" error result. **Behavioral test in `ctrl_c_during_tool_execution_returns_cancelled_by_user_error_result`.**
- [x] Ctrl-C at idle input prompt displays "Press Ctrl-C again to exit" warning; second Ctrl-C within 2s exits gracefully. **Behavioral test in `ctrl_c_at_the_prompt_warns_then_exits_gracefully_on_a_second_press_within_two_seconds`.**
- [x] Double Ctrl-C within 500ms during in-flight request force-quits without saving. **Behavioral test in `double_ctrl_c_within_five_hundred_ms_during_a_request_force_quits_without_saving`.**

### Budget surfacing

- [x] Status line displays `tokens: <used>/<limit> | turns: <used>/<limit>` and updates after each LLM response. **Behavioral test in `status_line_displays_used_and_total_tokens_and_turns`.**
- [x] Status line changes appearance when any budget resource reaches 80% usage. **Behavioral test in `status_line_changes_appearance_when_any_budget_resource_reaches_eighty_percent`.**
- [x] Budget exhaustion displays the reason and returns to input prompt (does not crash). **Behavioral test in `budget_exhaustion_is_displayed_and_returns_to_the_input_prompt`.**

### Multi-turn conversation

- [x] User messages accumulate in conversation history across turns. **Behavioral test in `user_messages_accumulate_in_conversation_history_across_turns`.**
- [x] The model receives full conversation history (subject to context compaction) on each turn. **Behavioral test in `provider_receives_the_full_conversation_history_on_each_turn_subject_to_compaction`.**
- [x] Tool results from previous turns are present in the conversation for subsequent model calls. **Behavioral test in `tool_results_from_previous_turns_remain_visible_to_the_model_on_later_turns`.**

### Session persistence

- [x] Graceful exit (`/exit`) writes a journal checkpoint with conversation state. **Behavioral test in `graceful_exit_writes_a_journal_checkpoint_with_conversation_state`.**
- [x] Resumed session restores conversation history and VFS state from journal checkpoint. **Behavioral test in `resumed_session_restores_conversation_history_and_vfs_state_from_the_checkpoint`.**
- [x] Resumed session displays summary with message count and turns used. **Behavioral test in `resumed_session_displays_a_summary_with_message_count_and_turns_used`.**
- [x] Default session storage is file-based at `~/.simulacra/sessions/<session-id>/checkpoint.json`. **Behavioral test in `default_session_storage_path_is_under_the_users_simulacra_directory`.**
- [x] Resuming a completed or exhausted session resets budget to configured limits but retains conversation history. **Behavioral test in `resuming_completed_or_exhausted_sessions_resets_budget_to_configured_limits_but_keeps_history`.**

### Error handling

- [x] Provider rate-limit error retries up to 3 times with exponential backoff and displays "Retrying in Ns...". **Behavioral test in `provider_rate_limit_errors_retry_three_times_with_exponential_backoff_and_feedback`.**
- [x] Provider auth error displays the error and returns to input prompt. **Behavioral test in `provider_auth_errors_are_displayed_and_return_to_the_input_prompt`.**
- [x] Tool execution error is displayed to user AND sent to model as error tool result. **Behavioral test in `tool_execution_errors_are_displayed_to_the_user_and_sent_back_to_the_model`.**
- [x] Journal write failure does not crash the session. **Behavioral test in `journal_write_failures_are_not_fatal_to_the_session`.**

### Terminal behavior

- [x] Non-TTY stdin reads all input as one message, auto-approves tools, runs one turn, then exits. **Behavioral test in `non_tty_stdin_reads_all_input_as_one_message_auto_approves_tools_runs_one_turn_and_exits`.**
- [x] Ctrl-D at input prompt exits gracefully (equivalent to `/exit`). **Behavioral test in `ctrl_d_at_the_input_prompt_exits_gracefully`.**
- [x] Terminal state is restored on graceful exit, forced exit, and panic. **Behavioral test in `terminal_state_is_restored_on_graceful_exit_forced_exit_and_panic`.**

### Integration

- [x] Interactive mode uses `AgentLoop` from simulacra-runtime (not a reimplementation). **Behavioral test in `interactive_mode_uses_the_shared_runtime_agent_loop_type`.**
- [x] `ExitReason::AwaitingApproval` is used to yield tool calls for user approval. **Behavioral test in `awaiting_approval_exit_reason_is_used_to_yield_tool_calls_for_user_approval`.**
- [x] Interactive mode reuses the same `ToolRegistry`, `AgentCell`, and provider as headless mode (shared bootstrap path). **Behavioral test in `interactive_mode_reuses_the_headless_bootstrap_provider_tool_registry_and_agent_cell_path`.**

## Observability (see S010 for conventions)

- [x] Interactive session start produces a span with `simulacra.operation.name` = `interactive_session` and `simulacra.session.id`. **Behavioral test in `interactive_session_start_emits_an_interactive_session_span_with_session_id`.**
- [x] Each user turn produces a child span with `simulacra.operation.name` = `interactive_turn` and `simulacra.turn.number`. **Behavioral test in `each_user_turn_emits_an_interactive_turn_child_span_with_turn_number`.**
- [x] Tool approval decisions are logged at `INFO` with `simulacra.tool.name`, `simulacra.tool.approval` = `approved` | `denied` | `approved_all`. **Behavioral test in `tool_approval_decisions_are_logged_at_info_with_tool_name_and_approval_state`.**
- [x] Cancellation events are logged at `INFO` with `simulacra.cancel.target` = `llm_request` | `tool_execution` | `session`. **Behavioral test in `cancellation_events_are_logged_at_info_with_the_cancellation_target`.**
- [x] Session save produces a span with `simulacra.operation.name` = `session_save` and `simulacra.session.id`. **Behavioral test in `session_save_emits_a_session_save_span_with_session_id`.**
- [x] Session resume produces a span with `simulacra.operation.name` = `session_resume`, `simulacra.session.id`, and `simulacra.session.message_count`. **Behavioral test in `session_resume_emits_a_session_resume_span_with_session_id_and_message_count`.**
- [x] `simulacra.interactive.turns` counter tracks completed interactive turns per session. **Behavioral test in `interactive_turn_counter_tracks_completed_interactive_turns_per_session`.**
- [x] Budget warning threshold crossing (80%) is logged at `WARN` with `simulacra.budget.resource` and `simulacra.budget.percent_used`. **Behavioral test in `budget_warning_threshold_crossings_are_logged_at_warn_with_resource_and_percent_used`.**
