# S020 — VFS Host Mounts

**Status:** Active
**Crates involved:** `simulacra-vfs`, `simulacra-config`, `simulacra-cli`

## Dependencies

- **ARCHITECTURE.md** — Golden Rule (all side effects mediated by host), single-binary philosophy, capability attenuation
- **S001** — VFS data structures (MemoryFs, VirtualFs trait)
- **S004** — Capability tokens (path read/write policies gate VFS access)
- **S010** — Observability conventions (span/event naming)
- **S013** — CLI bootstrap (where VFS construction and mounting happen)
- **S017** — Skills (depends on host mounts for external skill roots)

## Scope

This spec covers **Phase 1** (mount host paths into the VFS at bootstrap) and **Phase 2** (size limits and error handling). It does NOT cover:

- **Artifact extraction** (copying VFS files back to the host after execution). Future spec.
- **OverlayFs integration** (layered read-only lower / read-write upper). Future spec. Mounts copy into a plain `MemoryFs`. Agents write to VFS paths normally; writes never propagate to the host.
- **Real-time filesystem synchronization** (inotify, fswatch). Mounts are point-in-time copies.
- **FUSE or OS-level mount semantics.** "Mount" here means "copy host files into the VFS."
- **Agent-initiated host filesystem access.** Agents work within the VFS only.

## Context

The VFS (S001) defines an in-memory filesystem that agents operate within. But a VFS with no connection to the host filesystem is useless for real work. The host needs to populate the VFS with project files, skill directories, configuration artifacts, and workspace context before agent execution begins.

This spec defines the mounting protocol: how host filesystem paths are copied into the VFS before agent execution, how project roots are detected, what gets mounted automatically vs. via configuration, and how size limits protect against runaway mounts.

Mounting is a **host-side operation** that happens during bootstrap, before the agent loop starts. Agents never mount or unmount -- they see a pre-populated VFS and work within it. This preserves the Golden Rule: the host controls what the agent can see.

Mount operations are **not journaled**. They are host-side setup that occurs before the agent loop begins, and are therefore outside the journal's scope (S005).

## Design

```text
Host filesystem                          VFS (MemoryFs, in-memory)
                                         /
simulacra.toml (project root)                |
  |                                      +-- /workspace/
  +-- skills/                            |     +-- task.md (from --task)
  |     +-- rust-dev/                    |     +-- ... (from workspace_paths)
  |     +-- code-review/                 |
  +-- prompts/                           +-- /skills/
  |     +-- planner.md                   |     +-- rust-dev/
  +-- src/                               |     +-- code-review/
  +-- ...                                |     +-- (external skill roots)
                                         |
External skill root                      +-- /prompts/
  ~/simulacra-skills/                              +-- planner.md
    +-- my-custom-skill/
```

```text
Bootstrap sequence (extends S013 step 14):

  1. Create MemoryFs
  2. Detect project root (location of simulacra.toml or explicit --config parent dir)
  3. Validate mount size limits (see behavior 27-30)
  4. Process automatic mounts:
       - /skills/ from project_root/skills/ (if exists and auto_mount_skills != false)
       - system prompt files referenced by agent_types
  5. Process [vfs.mounts] from config:
       for each mount in config:
         - resolve source path relative to project root
         - validate source exists on host filesystem
         - copy host directory tree into VFS at mount.target
  6. Pre-seed /workspace/task.md with task text
  7. Record all mounts in bootstrap span
  8. Agent loop begins -- VFS is now sealed from host changes
```

## Config Types

The following Rust struct shapes are added to `simulacra-config`:

```rust
/// Optional `[vfs]` section in simulacra.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsConfig {
    /// Whether to auto-mount the project `skills/` directory. Default: true.
    #[serde(default = "default_true")]
    pub auto_mount_skills: bool,

    /// Maximum number of files per mount. Default: 10_000.
    #[serde(default = "default_max_files_per_mount")]
    pub max_files_per_mount: usize,

    /// Maximum total bytes per mount. Default: 104_857_600 (100 MiB).
    #[serde(default = "default_max_bytes_per_mount")]
    pub max_bytes_per_mount: u64,

    /// Configured mount entries.
    #[serde(default)]
    pub mounts: Vec<MountConfig>,
}

/// A single `[[vfs.mounts]]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// Host filesystem path (relative to project root, or absolute, or ~-prefixed).
    pub source: String,

    /// Absolute VFS path where the source is mounted.
    pub target: String,
}
```

`VfsConfig` is optional on `SimulacraConfig`. When absent, all defaults apply (auto-mount skills, no configured mounts, default size limits).

## Behavior

### Project root detection

1. The project root is the parent directory of the resolved `--config` path. If `--config` is `simulacra.toml` (default), the project root is the current working directory.
2. If `--config` is an absolute path (e.g., `/home/user/project/simulacra.toml`), the project root is `/home/user/project/`.
3. If `--config` is a relative path (e.g., `../other/simulacra.toml`), it is resolved to an absolute path first, then the parent directory is used.
4. The project root is recorded in the CLI root span as `simulacra.project.root`.
5. When no config file exists (ad-hoc mode from S013), the project root is the current working directory.

### Mount configuration

6. `simulacra.toml` gains an optional `[vfs]` section with a `[[vfs.mounts]]` array:

```toml
[vfs]
auto_mount_skills = true        # default: true
max_files_per_mount = 10000     # default: 10,000
max_bytes_per_mount = 104857600 # default: 100 MiB

[[vfs.mounts]]
source = "prompts"              # relative to project root, or absolute
target = "/prompts"             # absolute VFS path

[[vfs.mounts]]
source = "~/simulacra-skills"       # tilde expansion supported
target = "/skills/external"
```

7. `source` is a host filesystem path. Relative paths are resolved against the project root. Tilde (`~`) is expanded to the user's home directory. Tilde expansion is Unix-only; on Windows it is a no-op (the literal `~` is kept). Environment variables are NOT expanded (deterministic config).
8. `target` is an absolute VFS path where the source directory tree is mounted. It must start with `/`.
9. If `target` does not start with `/`, bootstrap fails with a startup error: `"mount target '{target}' must be an absolute path (start with '/')"`.
10. If `source` does not exist on the host filesystem, bootstrap fails with an error naming the missing path and the mount entry.
11. If two mounts target overlapping VFS path prefixes, the merge semantics are:
    - **Directory-level:** union merge. Files from both mounts coexist. If mount A writes `/skills/a.md` and mount B writes `/skills/b.md`, both survive.
    - **File-level:** last-writer-wins. If mount A and mount B both write `/skills/readme.md`, the mount processed later (per ordering in behavior 31) wins silently.
12. Mount targets must not be `/` (root). Attempting to mount to `/` is a startup error.
13. An empty `[[vfs.mounts]]` array (i.e., `[vfs]` section present but no mount entries) is valid. Only automatic mounts apply.

### Automatic mounts

14. If the project root contains a `skills/` directory and `auto_mount_skills` is not `false`, its contents are mounted at `/skills/` in the VFS before any configured mounts are processed.
15. Automatic mounts happen before configured mounts, so configured mounts can overlay or extend automatic ones.
16. System prompt files referenced by `agent_type.system_prompt` (e.g., `"prompts/planner.md"`) are resolved relative to the project root and copied into the VFS at the same relative path under `/` (e.g., `/prompts/planner.md`). This happens only for paths that are relative -- absolute paths or inline prompt strings are not mounted.
17. If a system prompt path does not exist on the host:
    - If the agent type is the entry agent (`[task].entry_agent`): startup error. The entry agent must have its prompt.
    - Otherwise: emit a `WARN`-level event and skip. The agent type may never be spawned.
18. If a system prompt path resolves (via symlink or `..`) to a location outside the project root, bootstrap fails with a path-traversal error. This prevents prompt injection via symlinks pointing outside the project boundary.
19. System prompt files exceeding 1 MB are rejected with a startup error. This prevents accidental inclusion of large binary or generated files as prompts.
20. A "relative prompt path" is one that does not start with `/` or `~` and contains at least one `/`. Bare filenames without path separators (e.g., `"system.md"`) are treated as inline prompt names, not filesystem paths, and are not mounted.
21. The `/workspace/` directory is always created. `task.md` is pre-seeded per S013 step 14.
22. In ad-hoc mode (no `simulacra.toml`), the only automatic mount is `/workspace/task.md`. No skills directory, no prompt files, no configured mounts.

### Mount execution (copy semantics)

23. Mounting copies the host directory tree into the VFS (`MemoryFs`) recursively. Files become VFS file entries. Directories become VFS directory entries.
24. Host symlinks are resolved (followed) before copying. The VFS does not have symlink semantics. Symlink loops are detected and skipped with a `WARN`-level event identifying the loop path. Detection uses a visited-inode set per mount operation.
25. Binary files are copied as-is (the VFS stores `Vec<u8>`).
26. Empty directories on the host are created as empty directories in the VFS.
27. Hidden files (starting with `.`) are included in the copy. Filtering is not the mount layer's job -- capability tokens and agent-type config control what the agent can access.
28. Mount copies are point-in-time snapshots. Changes to the host filesystem after mount are not reflected in the VFS. Changes to the VFS are not reflected on the host filesystem.
29. Large directory trees are mounted eagerly (full copy at bootstrap). Lazy mounting is a future optimization not covered by this spec.

### Mount size limits

30. Each mount (automatic or configured) is subject to size limits:
    - `max_files_per_mount` (default: 10,000 files)
    - `max_bytes_per_mount` (default: 100 MiB / 104,857,600 bytes)
31. When a mount reaches 80% of either limit, a `WARN`-level event is emitted: `"mount '{target}' approaching file limit: {count}/{max}"` or `"mount '{target}' approaching size limit: {bytes}/{max}"`.
32. When a mount exceeds either limit, bootstrap fails with an error: `"mount '{target}' exceeds file limit: {count} files > {max}"` or `"mount '{target}' exceeds size limit: {bytes} bytes > {max}"`.
33. Limits are per-mount, not global. Two mounts of 9,000 files each are both valid under the default 10,000 limit.

### Mount ordering

34. Mount processing order:
    1. Automatic skill mount (`skills/` -> `/skills/`)
    2. Automatic system prompt mounts
    3. Configured `[[vfs.mounts]]` in declaration order
    4. Pre-seed `/workspace/task.md`
35. Later mounts overwrite earlier entries at the same VFS path without error (file-level last-writer-wins per behavior 11).

### Interaction with capabilities

36. Mounts do not bypass capability tokens. Mounting a directory into the VFS makes it visible, but the agent can only read/write paths allowed by its `CapabilityToken.paths_read` and `paths_write` patterns (S004). For example: a mount places files at `/prompts/planner.md`, but if the agent's `paths_read` is `["/workspace/**"]`, the agent cannot read `/prompts/planner.md`. The mount exists; the capability gate blocks access.
37. All mounts are copies into `MemoryFs`. The agent can write to any VFS path its capability token allows. Writes modify the in-memory VFS only and never propagate to the host filesystem.

## Assertions

### Project root detection

- [x] Project root is the parent directory of the resolved `--config` path. **`detect_project_root()` calls `resolved.parent()` on the config path.**
- [x] Relative `--config` paths are resolved to absolute before determining project root. **Non-absolute paths are joined with `cwd` before calling `.parent()`.**
- [x] Ad-hoc mode (no config file) uses the current working directory as project root. **`if is_adhoc { std::env::current_dir() }` path in `detect_project_root()`.**
- [x] Project root is recorded in the CLI root span as `simulacra.project.root`. **`simulacra-cli/src/lib.rs` sets `"simulacra.project.root" = project_root_str.as_str()` on the CLI run span.**

### Mount configuration

- [x] `[[vfs.mounts]]` entries with relative `source` resolve against the project root. **`resolve_mount_source()` joins relative paths with `project_root.join(&expanded)`.**
- [x] `[[vfs.mounts]]` entries with absolute `source` use the path directly. **`resolve_mount_source()` returns absolute paths unchanged when `path.is_absolute()`.**
- [x] Tilde in `source` is expanded to the user's home directory (Unix only; no-op on Windows). **`expand_tilde()` replaces leading `~` with `$HOME` on Unix; `#[cfg(not(unix))]` returns unchanged.**
- [x] `target` must start with `/`; a target without leading `/` is a startup error with message naming the invalid target. **`process_host_mounts()` checks `!mount.target.starts_with('/')` and returns `MountError::InvalidMountTarget` with the target name.**
- [x] A mount with a non-existent `source` path fails startup with an error naming the path. **`!source_path.exists()` check returns `MountError::SourceNotFound { source_path, target }`.**
- [x] Mounting to `/` (root) is a startup error. **`mount.target == "/"` check returns `MountError::InvalidMountTarget("mounting to root is not allowed")`.**
- [x] Overlapping mount targets at directory level produce a union merge (files from both survive). **`copy_host_dir_to_vfs()` calls `vfs.write()` per file and `vfs.mkdir()` per dir — existing files from earlier mounts are not deleted.**
- [x] Overlapping mount targets at file level use last-writer-wins (later mount's file wins silently). **`vfs.write()` overwrites existing content at the same path; no duplicate detection.**
- [x] An empty `[[vfs.mounts]]` array is valid; only automatic mounts apply. **`vfs_config.mounts` defaults to `Vec::new()`; the loop simply doesn't execute.**
- [x] `VfsConfig` deserializes from TOML with correct defaults when `[vfs]` section is absent. **`VfsConfig` has `impl Default` with `auto_mount_skills: true`, `max_files: 10_000`, `max_bytes: 100 MiB`, `mounts: Vec::new()`; field on `SimulacraConfig` is `#[serde(default)]`.**

### Automatic mounts

- [x] A project `skills/` directory is auto-mounted at `/skills/` when `auto_mount_skills` is not `false`. **`process_host_mounts()`: `if vfs_config.auto_mount_skills { ... skills_dir.exists() ... copy_host_dir_to_vfs(&skills_dir, "/skills", ...) }`.**
- [x] Setting `auto_mount_skills = false` suppresses the automatic skill mount. **The `if vfs_config.auto_mount_skills` guard skips the entire auto-mount block.**
- [x] Relative system prompt paths in agent type config are mounted into the VFS. **Iterates `config.agent_types`, checks `is_relative`, reads host file, writes to VFS at `"/{prompt_path}"`.**
- [x] Missing system prompt path for entry agent is a startup error. **`if agent_name == entry_agent { return Err(MountError::EntryPromptNotFound { ... }) }`.**
- [x] Missing system prompt path for non-entry agent emits a WARN and is skipped. **`else { tracing::warn!("missing non-entry-agent system prompt skipped") }`.**
- [x] Ad-hoc mode produces no automatic mounts beyond `/workspace/task.md`. **CLI bootstrap skips `process_host_mounts()` entirely when `is_adhoc` is true; only `/workspace` mkdir and task.md seeding occur.**
- [x] System prompt paths that resolve outside the project root (via symlink or `..`) fail with a path-traversal error. **`process_host_mounts()` canonicalizes the prompt path and checks `!canonical.starts_with(project_root)`, returning `MountError::PathTraversal`.**
- [x] System prompt files exceeding 1 MB are rejected with a startup error. **`process_host_mounts()` checks `content.len() > 1_048_576` and returns `MountError::PromptTooLarge`.**
- [x] Bare filenames without path separators (e.g., `"system.md"`) are treated as inline prompt names and are not mounted as filesystem paths. **The `is_relative` heuristic requires `!starts_with('/') && !starts_with('~') && contains('/')`; bare names without `/` are skipped.**

### Mount execution

- [x] Mount copies the full host directory tree recursively into the VFS. **`copy_host_dir_to_vfs()` uses a stack-based recursive walk, calling `vfs.write()` for files and `vfs.mkdir()` for directories.**
- [x] Host symlinks are resolved (followed) before copying into the VFS. **Uses `std::fs::metadata()` (which follows symlinks) instead of `symlink_metadata()`.**
- [x] Symlink loops are detected and skipped with a WARN-level event. **On Unix: tracks `(dev, ino)` pairs in `visited_inodes` set; emits `tracing::warn!("symlink loop detected, skipping")` on revisit.**
- [x] Empty host directories are created as empty VFS directories. **`vfs.mkdir(&vfs_dir)` is called for every directory in the stack, even if it contains no files.**
- [x] Hidden files are included in the mount copy. **No filename filtering in `copy_host_dir_to_vfs()` — all entries from `read_dir()` are processed.**
- [x] Mount is a point-in-time snapshot; subsequent host changes are not reflected. **Files are read with `std::fs::read()` and written to in-memory VFS; no filesystem watches or lazy loading.**
- [x] Large files are copied as raw bytes without transformation. **`std::fs::read()` returns `Vec<u8>` which is passed directly to `vfs.write()`.**
- [x] Mount operations are not journaled (host-side setup before agent loop). **`process_host_mounts()` runs during bootstrap before the agent loop starts; no journal writes in mount code.**

### Mount size limits

- [x] A mount exceeding `max_files_per_mount` (default 10,000) fails bootstrap with an error. **`if file_count > max_files { return Err(MountError::FileLimitExceeded { ... }) }`.**
- [x] A mount exceeding `max_bytes_per_mount` (default 100 MiB) fails bootstrap with an error. **`if total_bytes + file_size > max_bytes { return Err(MountError::SizeLimitExceeded { ... }) }`.**
- [x] A mount at 80% of the file limit emits a WARN-level event. **`if !warned_files && file_count >= file_threshold` (where threshold = `max_files * 0.8`) emits `tracing::warn!`.**
- [x] A mount at 80% of the byte limit emits a WARN-level event. **`if !warned_bytes && total_bytes >= byte_threshold` (where threshold = `max_bytes * 0.8`) emits `tracing::warn!`.**
- [x] Limits are per-mount, not global across all mounts. **`copy_host_dir_to_vfs()` receives `max_files` and `max_bytes` per call; counters reset per mount invocation.**
- [x] Custom limits in `[vfs]` config override defaults. **`VfsConfig.max_files_per_mount` and `max_bytes_per_mount` are read from config and passed to `copy_host_dir_to_vfs()`.**

### Mount ordering

- [x] Automatic skill mount runs before configured mounts. **`process_host_mounts()` processes auto skill mount first, then system prompts, then configured `[[vfs.mounts]]`.**
- [x] Configured mounts run in declaration order. **`for mount in &vfs_config.mounts` iterates in Vec order (TOML declaration order).**
- [x] `/workspace/task.md` pre-seeding runs after all mounts. **In CLI bootstrap, `process_host_mounts()` is called before task.md seeding.**
- [x] Later mounts silently overwrite files at the same VFS path. **`vfs.write()` overwrites; no conflict detection or error on existing paths.**

### Capability interaction

- [x] Mounts do not bypass `CapabilityToken.paths_read` restrictions. **Mount code writes to VFS without capability checks, but agent access goes through `AgentCell::read_file()` which calls `capability.check_path_read()` in `simulacra-sandbox/src/lib.rs`.**
- [x] Mounts do not bypass `CapabilityToken.paths_write` restrictions. **Agent writes go through `AgentCell::write_file()` which calls `capability.check_path_write()`; mount writes are host-side setup only.**
- [x] A mounted path outside `paths_read` is inaccessible to the agent despite existing in the VFS. **`AgentCell::read_file()` returns `CapabilityDenied` when path does not match `paths_read` patterns, regardless of VFS content.**

## Observability (see S010 for conventions)

- [x] Bootstrap mount processing produces a span with `simulacra.operation.name` = `vfs_mount` for each mount entry. **`tracing::info_span!("vfs_mount", "simulacra.operation.name" = "vfs_mount", ...)` created for auto skill mount, system prompt mounts, and configured mounts.**
- [x] Each mount span includes `simulacra.vfs.mount.source` (host path), `simulacra.vfs.mount.target` (VFS path), and `simulacra.vfs.mount.file_count`. **All three attributes set on each `vfs_mount` span; `file_count` recorded after copy completes.**
- [x] Automatic mounts are distinguished from configured mounts via `simulacra.vfs.mount.origin` = `auto` or `config`. **Auto mounts use `"simulacra.vfs.mount.origin" = "auto"`, configured mounts use `"config"`.**
- [x] Mount failures emit an `ERROR`-level event with the source path and the error reason. **`tracing::error!` emitted for invalid target and non-existent source before returning `MountError`.**
- [x] Mount size limit warnings (80% threshold) emit a `WARN`-level event with current count/bytes and limit. **`tracing::warn!("mount '...' approaching file limit: {file_count}/{max_files}")` and byte equivalent.**
- [x] Symlink loop detection emits a `WARN`-level event with the loop path. **`tracing::warn!("simulacra.vfs.loop_path" = %entry_path.display(), "symlink loop detected, skipping")`.**
- [x] Missing non-entry-agent system prompt emits a `WARN`-level event. **`tracing::warn!("missing non-entry-agent system prompt skipped", "agent" = %agent_name)`.**
- [x] An `INFO`-level event at bootstrap completion reports total mount count and total files mounted. **`tracing::info!("simulacra.vfs.mount.count" = total_mount_count, "simulacra.vfs.mount.file_total" = total_file_count, "VFS host mounts complete")`.**
