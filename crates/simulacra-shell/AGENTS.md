# simulacra-shell

Virtual shell that parses and executes command lines against `&dyn VirtualFs`.
No real file descriptors are ever touched.

## Modules

- `parser` — tokenizer + recursive-descent parser producing `ShellLine` AST
- `executor` — `ShellExecutor` walks the AST, pipes stdout, handles `&&`/`||`
- `builtins` — shell builtins implemented against the VFS, including core
  file/text commands plus small compatibility probes such as `touch`, `test`,
  `[`, `printf`, `basename`, and `dirname`

## Key rules

- All I/O goes through the VFS trait — never `std::fs` or real FDs.
- Unknown commands return exit 127 with "command not found" on stderr.
- Pipes connect stdout of left to stdin of right; exit code comes from the rightmost command.
- Environment variables: `$VAR` and `${VAR}` expand; undefined vars become empty string.
- `$(cmd)` runs a sub-shell and captures stdout (trailing newline stripped).

## Testing

```bash
cargo test -p simulacra-shell
```

Tests use `simulacra_vfs::MemoryFs` — no real filesystem access.
