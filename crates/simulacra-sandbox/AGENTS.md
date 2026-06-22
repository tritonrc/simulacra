# AGENTS.md — simulacra-sandbox

## Purpose

Composes VFS, shell, and QuickJS into `AgentCell` — the sandboxed execution
environment for a single agent. All side-effecting operations are gated by
`CapabilityToken` checks before execution.

## Key Types

- `AgentCell` — holds `Arc<dyn VirtualFs>` and `CapabilityToken`; entry point for shell/JS execution.
- `SandboxError` — typed error enum covering capability denials, shell errors, and JS errors.

## Invariants

- Every operation checks the capability token before executing.
- Shell execution delegates to `simulacra_shell::ShellExecutor` against the virtual filesystem.
- JS execution is stubbed pending `simulacra-quickjs` implementation.
- No direct filesystem or network access — everything goes through VFS.

## Dependencies

`simulacra-types`, `simulacra-vfs`, `simulacra-shell`, `simulacra-quickjs`, `simulacra-tool`, `thiserror`, `tokio`.
