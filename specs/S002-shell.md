# S002 — Shell Emulator

**Status:** Active
**Crate:** `simulacra-shell`

## Behavior

1. Commands execute against a `&dyn VirtualFs`, never the real filesystem.
2. Pipes (`|`) connect stdout of left to stdin of right. Exit code is from the rightmost command.
3. Redirects: `>` truncates and writes, `>>` appends. Target file is created if it doesn't exist.
4. `&&` executes right only if left exits 0. `||` executes right only if left exits non-zero.
5. `$VAR` and `${VAR}` expand environment variables. Undefined vars expand to empty string.
6. `$(cmd)` captures stdout of cmd as a string value (command substitution).
7. Unknown commands return exit code 127 and stderr "command not found: <name>".
8. All builtins write to virtual stdout/stderr, never to real file descriptors.

## Phase 1 Builtins

`echo`, `cat`, `ls`, `mkdir`, `cp`, `mv`, `rm`, `head`, `tail`, `grep`, `sed`, `wc`, `find`, `sort`, `uniq`, `cut`, `tr`, `tee`

## Fidelity Builtins

These are small compatibility commands that prevent agents from wasting turns
on common shell probes and script fragments:

`touch`, `test`, `[`, `printf`, `basename`, `dirname`

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
- [x] `$VAR` expansion replaces with env value; undefined expands to empty string. **Tested in `dollar_var_expansion_replaces_with_env_value` and `undefined_variable_expands_to_empty_string`.**
- [x] `$(echo inner)` command substitution captures stdout. **Tested in `command_substitution_captures_stdout`.**
- [x] Each Phase 1 builtin (`cat`, `mkdir`, `cp`, `mv`, `rm`, `head`, `tail`, `sed`, `wc`, `find`, `sort`, `uniq`, `cut`, `tr`, `tee`) has at least one test. **All builtins covered with dedicated tests.**
- [x] Shell commands against VFS never touch real filesystem. **Tested in `shell_commands_never_touch_real_filesystem`.**
- [x] Pipe exit code comes from rightmost command. **Tested in `pipe_exit_code_comes_from_rightmost_command`.**
- [x] Path-bearing builtins and redirects resolve relative paths against the current shell `cwd`. **Tested in `cat_after_cd_resolves_relative_path_against_cwd`, `redirects_after_cd_write_relative_targets_under_cwd`, and existing `ls_after_cd_lists_relative_to_cwd`.**
- [x] `touch`, `test`, `[`, `printf`, `basename`, and `dirname` cover common agent script fragments. **Tested in `touch_and_test_bracket_work_with_relative_paths`, `printf_supports_common_string_newline_format`, and `basename_and_dirname_cover_common_path_splitting`.**

## Observability (see S010 for conventions)

- [x] Every shell command execution produces a span with `simulacra.operation.name` = `shell_command`, `simulacra.shell.command`, and `simulacra.shell.exit_code`.
- [x] `simulacra.shell.commands` counter is incremented per command execution. **Note: validated via span count, not an actual OTel counter metric.**
- [x] Pipe chains produce a parent span with child spans per pipeline stage.
