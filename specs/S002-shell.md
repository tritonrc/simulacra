# S002 — Shell Emulator

**Status:** Active
**Crate:** `simulacra-shell`

## Behavior

1. Commands execute against a `&dyn VirtualFs`, never the real filesystem.
2. Pipes (`|`) connect stdout of left to stdin of right. Exit code is from the rightmost command.
3. Redirects: `>` and `1>` truncate and write stdout, `>>` appends stdout, and `2>` redirects stderr. Target file is created if it doesn't exist. Compatibility redirects `2>&1`, `1>&2`, `&>`, and `&>>` are supported for common agent shell fragments.
4. Heredocs (`<<EOF` and `<<'EOF'`) provide stdin to the command that declares them, including when that command is part of a pipeline.
5. `&&` executes right only if left exits 0. `||` executes right only if left exits non-zero. `;` and newlines separate commands and always continue to the next command.
6. `$VAR` and `${VAR}` expand environment variables. Undefined vars expand to empty string. `$?` and `${?}` expand to the previous executed pipeline's exit code.
7. `$(cmd)` captures stdout of cmd as a string value (command substitution).
8. Unknown commands return exit code 127 and stderr "command not found: <name>".
9. All builtins write to virtual stdout/stderr, never to real file descriptors.
10. `/dev/null` is a shell device path: reads return EOF, writes are discarded, and redirects to it do not require a VFS file to exist.

## Phase 1 Builtins

`echo`, `cat`, `ls`, `mkdir`, `cp`, `mv`, `rm`, `head`, `tail`, `grep`, `rg`, `sed`, `wc`, `find`, `sort`, `uniq`, `cut`, `tr`, `tee`

## Fidelity Builtins

These are small compatibility commands that prevent agents from wasting turns
on common shell probes and script fragments:

`touch`, `test`, `[`, `printf`, `basename`, `dirname`, `awk`, `jq`, `sleep`

The `awk` fidelity subset supports `print` expressions made from `NR`, `$0`,
`$N`, `$NF`, double-quoted string literals adjacent to those atoms, and
comma-separated print groups. AWK input may come from stdin or VFS file operands
resolved against the current shell `cwd`.

The `jq` fidelity subset supports common coding-agent JSON inspection snippets:
`-r`/`--raw-output`, identity `.`, dotted field paths, and `keys[]` over objects
or arrays. JSON input may come from stdin or VFS file operands resolved against
the current shell `cwd`.

## Network Builtins (S022)

`curl`, `wget` — routed through `ShellHttpProxy` for Golden Rule enforcement (capability, budget, journal). When executed inside `AgentCell`, the proxy is `AgentCellShellHttpProxy` which delegates to `fetch_http_inner`. See [S022](S022-shell-http.md) for full design.

## Assertions

- [x] `echo hello` → stdout "hello\n", exit 0.
- [x] `echo hello | grep hello` → stdout "hello\n", exit 0.
- [x] `echo hello | grep world` → stdout "", exit 1.
- [x] `echo hello > /file.txt && cat /file.txt` → stdout "hello\n".
- [x] `echo a >> /f.txt && echo b >> /f.txt && cat /f.txt` → "a\nb\n".
- [x] `ls /` on empty VFS → lists nothing, exit 0.
- [x] `ls /` after creating files → lists them sorted.
- [x] `nonexistent_cmd` → exit 127, stderr contains "command not found".
- [x] `false && echo yes` → no output (short-circuit).
- [x] `false || echo fallback` → stdout "fallback\n".
- [x] Mixed `&&`/`||` chains are evaluated left-to-right using the last executed pipeline status, so `false && echo x || echo y` prints "y\n" and `true || echo x && echo y` prints "y\n". **Tested in `false_and_echo_then_or_echo_runs_or_fallback`, `true_or_echo_then_and_echo_runs_final_and_rhs`, `skipped_and_rhs_does_not_block_following_or_chain`, `skipped_or_rhs_does_not_block_following_and_chain`, `executed_failure_in_mixed_chain_runs_following_or_rhs`, and `executed_success_in_mixed_chain_runs_following_and_rhs`.**
- [x] `$VAR` expansion replaces with env value; undefined expands to empty string. **Tested in `dollar_var_expansion_replaces_with_env_value` and `undefined_variable_expands_to_empty_string`.**
- [x] `$?` and `${?}` expansion report the previous executed pipeline status, including rightmost-command pipeline status and skipped short-circuit behavior, and single quotes suppress them. **Tested in `dollar_question_tracks_true_and_false_across_semicolons`, `dollar_question_after_pipeline_uses_rightmost_exit_code`, `dollar_question_preserves_status_across_skipped_short_circuit_commands`, `brace_dollar_question_expands_last_status`, and `single_quoted_dollar_question_stays_literal`.**
- [x] `$(echo inner)` command substitution captures stdout. **Tested in `command_substitution_captures_stdout`.**
- [x] Each Phase 1 builtin (`cat`, `mkdir`, `cp`, `mv`, `rm`, `head`, `tail`, `grep`, `rg`, `sed`, `wc`, `find`, `sort`, `uniq`, `cut`, `tr`, `tee`) has at least one test. **All builtins covered with dedicated tests.**
- [x] Shell commands against VFS never touch real filesystem. **Tested in `shell_commands_never_touch_real_filesystem`.**
- [x] Pipe exit code comes from rightmost command. **Tested in `pipe_exit_code_comes_from_rightmost_command`.**
- [x] Path-bearing builtins and redirects resolve relative paths against the current shell `cwd`. **Tested in `cat_after_cd_resolves_relative_path_against_cwd`, `redirects_after_cd_write_relative_targets_under_cwd`, and existing `ls_after_cd_lists_relative_to_cwd`.**
- [x] `touch`, `test`, `[`, `printf`, `basename`, and `dirname` cover common agent script fragments. **Tested in `touch_and_test_bracket_work_with_relative_paths`, `printf_supports_common_string_newline_format`, and `basename_and_dirname_cover_common_path_splitting`.**
- [x] `/dev/null`, `2>/dev/null`, `2>&1`, `1>&2`, `&>`, and `&>>` cover common agent shell probes and source-search recovery commands. **Tested in `agent_shell_fidelity.rs`.**
- [x] `awk '{print $N}'`, `awk '{print NR": "$0}'`, `awk '{print NR, $0}'`, and AWK file operands cover common field-extraction and line-inspection snippets. **Tested in `agent_shell_fidelity.rs`.**
- [x] `jq -r '.name' package.json`, `jq -r '.scripts | keys[]' package.json`, `jq -r 'keys[]' items.json` from a non-root cwd, and `printf ... | jq '.'` cover common JSON/package inspection snippets. **Tested in `json_fidelity.rs` and `headless_tool_fidelity.rs`.**
- [x] `sleep 0` and `sleep 1` cover common telemetry-wait snippets without leaving the shell emulator. **Tested in `builtin_commands.rs`.**
- [x] `grep -rn`, `find -type f (...) -name`, `sed -n 's///p'`, and `grep -oP '(?<=\\s)\\S+'` cover common source-search and shell-recovery snippets. **Tested in `agent_shell_fidelity.rs`.**
- [x] `rg`, `rg --files`, `rg -l`, and `rg -g '*.rs'` cover Codex-style source search snippets without leaving the VFS. **Tested in `agent_shell_fidelity.rs`.**
- [x] Multiline shell fragments split on newlines like `;`, while backslash-newline continues a command. **Tested in `parser_newline_splits_into_separate_items`, `newline_runs_rhs_after_lhs_like_semicolon`, and existing continuation tests.**
- [x] Heredocs feed command stdin for file-writing and pipeline fragments. **Tested in `heredoc_writes_file_through_redirect`, `heredoc_feeds_pipeline_stdin`, and `headless_tool_fidelity.rs`.**

## Observability (see S010 for conventions)

- [x] Every shell command execution produces a span with `simulacra.operation.name` = `shell_command`, `simulacra.shell.command`, and `simulacra.shell.exit_code`.
- [x] `simulacra.shell.commands` counter is incremented per command execution. **Note: validated via span count, not an actual OTel counter metric.**
- [x] Pipe chains produce a parent span with child spans per pipeline stage.
