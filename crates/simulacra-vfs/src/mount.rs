//! Host-filesystem mount logic for the VFS.
//!
//! Provides functions to copy host directories into a [`VirtualFs`],
//! resolve mount source paths, and detect project roots.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use simulacra_config::SimulacraConfig;
use simulacra_types::VirtualFs;

pub(crate) mod path_resolution;

use path_resolution::resolve_mount_source;

/// Errors that can occur during VFS mount operations.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// An I/O operation failed (reading host files, stat, canonicalize, etc.).
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },

    /// A VFS write operation failed.
    #[error("failed to write to VFS at {path}: {detail}")]
    VfsWrite { path: String, detail: String },

    /// A mount exceeded the per-mount file count limit.
    #[error("mount '{mount_target}' exceeds file limit: {actual} files > {limit}")]
    FileLimitExceeded {
        mount_target: String,
        actual: usize,
        limit: usize,
    },

    /// A mount exceeded the per-mount byte size limit.
    #[error("mount '{mount_target}' exceeds size limit: {actual} bytes > {limit}")]
    SizeLimitExceeded {
        mount_target: String,
        actual: u64,
        limit: u64,
    },

    /// A config path has no parent directory.
    #[error("config path has no parent directory")]
    NoParentDirectory,

    /// A system prompt resolved outside the project root (path traversal).
    #[error(
        "system prompt '{prompt_path}' resolves to {resolved} which is outside the project root {root}"
    )]
    PathTraversal {
        prompt_path: String,
        resolved: String,
        root: String,
    },

    /// A system prompt file exceeds the size limit.
    #[error("system prompt '{prompt_path}' is {size} bytes, which exceeds the 1 MB limit")]
    PromptTooLarge { prompt_path: String, size: u64 },

    /// A mount target path is invalid.
    #[error("{0}")]
    InvalidMountTarget(String),

    /// A mount source path does not exist.
    #[error("mount source does not exist: {source_path} (for mount target '{target}')")]
    SourceNotFound { source_path: String, target: String },

    /// An entry agent's system prompt file was not found.
    #[error("entry agent system prompt not found: {prompt_path} (resolved to {resolved})")]
    EntryPromptNotFound {
        prompt_path: String,
        resolved: String,
    },
}

/// Determine the "project root" used for resolving relative mount paths.
///
/// - Config-based: parent directory of the resolved config path.
/// - Ad-hoc: current working directory.
pub fn detect_project_root(config_path: &str, is_adhoc: bool) -> Result<PathBuf, MountError> {
    if is_adhoc {
        // Use dunce-style canonicalization to avoid /private/var on macOS:
        // get the raw cwd path without resolving symlinks.
        let cwd = std::env::current_dir().map_err(|e| MountError::Io {
            context: "failed to determine current working directory".into(),
            source: e,
        })?;
        // Strip /private prefix if the original temp_dir doesn't have it
        Ok(strip_private_prefix(&cwd))
    } else {
        let path = std::path::Path::new(config_path);
        // Resolve to absolute without canonicalizing symlinks (which would
        // turn /var into /private/var on macOS, breaking path matching).
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let cwd = std::env::current_dir().map_err(|e| MountError::Io {
                context: "failed to get cwd for config resolution".into(),
                source: e,
            })?;
            let cwd = strip_private_prefix(&cwd);
            cwd.join(path)
        };
        resolved
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or(MountError::NoParentDirectory)
    }
}

/// On macOS, `std::env::current_dir()` can return paths prefixed with
/// `/private/var` when the original path was `/var` (since `/var` is a
/// symlink to `/private/var`). Strip the prefix to match user-visible paths.
#[cfg(target_os = "macos")]
fn strip_private_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("/private/var/") {
        PathBuf::from(format!("/var/{rest}"))
    } else if let Some(rest) = s.strip_prefix("/private/tmp/") {
        PathBuf::from(format!("/tmp/{rest}"))
    } else if let Some(rest) = s.strip_prefix("/private/etc/") {
        PathBuf::from(format!("/etc/{rest}"))
    } else {
        path.to_path_buf()
    }
}

#[cfg(not(target_os = "macos"))]
fn strip_private_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Recursively copy a host directory tree into the VFS.
/// Returns (file_count, total_bytes).
/// Tracks visited inodes to detect symlink loops.
pub(crate) fn copy_host_dir_to_vfs(
    host_path: &Path,
    vfs_target: &str,
    vfs: &Arc<dyn VirtualFs>,
    max_files: usize,
    max_bytes: u64,
    mount_target_for_errors: &str,
) -> Result<(usize, u64), MountError> {
    let mut file_count: usize = 0;
    let mut total_bytes: u64 = 0;
    let mut warned_files = false;
    let mut warned_bytes = false;
    let file_threshold = (max_files as f64 * 0.8) as usize;
    let byte_threshold = (max_bytes as f64 * 0.8) as u64;

    // Track visited inodes for symlink loop detection
    #[cfg(unix)]
    let mut visited_inodes: std::collections::HashSet<(u64, u64)> =
        std::collections::HashSet::new();

    // Stack-based recursive walk (avoids actual recursion)
    let mut stack: Vec<(PathBuf, String)> = vec![(host_path.to_path_buf(), vfs_target.to_string())];

    while let Some((dir_path, vfs_dir)) = stack.pop() {
        // Create the VFS directory
        let _ = vfs.mkdir(&vfs_dir);

        let entries = match std::fs::read_dir(&dir_path) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    path = %dir_path.display(),
                    error = %e,
                    "failed to read host directory during mount"
                );
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read directory entry");
                    continue;
                }
            };

            let entry_path = entry.path();
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();
            let vfs_path = if vfs_dir == "/" {
                format!("/{file_name_str}")
            } else {
                format!("{vfs_dir}/{file_name_str}")
            };

            // Follow symlinks: get metadata (which follows symlinks)
            let metadata = match std::fs::metadata(&entry_path) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        path = %entry_path.display(),
                        error = %e,
                        "failed to stat entry during mount (broken symlink?)"
                    );
                    continue;
                }
            };

            // Symlink loop detection via inode tracking
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let inode_key = (metadata.dev(), metadata.ino());
                if metadata.is_dir() && !visited_inodes.insert(inode_key) {
                    tracing::warn!(
                        "simulacra.vfs.loop_path" = %entry_path.display(),
                        "symlink loop detected, skipping"
                    );
                    continue;
                }
            }

            if metadata.is_dir() {
                stack.push((entry_path, vfs_path));
            } else if metadata.is_file() {
                // Check file count limit
                file_count += 1;
                if file_count > max_files {
                    return Err(MountError::FileLimitExceeded {
                        mount_target: mount_target_for_errors.to_string(),
                        actual: file_count,
                        limit: max_files,
                    });
                }

                // Check byte limit via metadata BEFORE reading the file
                let file_size = metadata.len();
                if total_bytes + file_size > max_bytes {
                    return Err(MountError::SizeLimitExceeded {
                        mount_target: mount_target_for_errors.to_string(),
                        actual: total_bytes + file_size,
                        limit: max_bytes,
                    });
                }
                let data = std::fs::read(&entry_path).map_err(|e| MountError::Io {
                    context: format!("failed to read file for mount: {}", entry_path.display()),
                    source: e,
                })?;
                total_bytes += data.len() as u64;

                vfs.write(&vfs_path, &data)
                    .map_err(|e| MountError::VfsWrite {
                        path: vfs_path.clone(),
                        detail: e.to_string(),
                    })?;

                // Emit 80% threshold warnings (once per limit type per mount)
                if !warned_files && file_count >= file_threshold {
                    warned_files = true;
                    tracing::warn!(
                        "mount '{mount_target_for_errors}' approaching file limit: {file_count}/{max_files}"
                    );
                }
                if !warned_bytes && total_bytes >= byte_threshold {
                    warned_bytes = true;
                    tracing::warn!(
                        "mount '{mount_target_for_errors}' approaching size limit: {total_bytes}/{max_bytes}"
                    );
                }
            }
        }
    }

    Ok((file_count, total_bytes))
}

/// Process all host mounts (automatic + configured) into the VFS.
pub fn process_host_mounts(
    vfs: &Arc<dyn VirtualFs>,
    config: &SimulacraConfig,
    project_root: &Path,
    entry_agent: &str,
) -> Result<(), MountError> {
    let vfs_config = &config.vfs;
    let max_files = vfs_config.max_files_per_mount;
    let max_bytes = vfs_config.max_bytes_per_mount;
    let mut total_mount_count: usize = 0;
    let mut total_file_count: usize = 0;

    // 1. Auto-mount skills/ if it exists and auto_mount_skills is true
    if vfs_config.auto_mount_skills {
        let skills_dir = project_root.join("skills");
        if skills_dir.exists() && skills_dir.is_dir() {
            let _span = tracing::info_span!(
                "vfs_mount",
                "simulacra.operation.name" = "vfs_mount",
                "simulacra.vfs.mount.source" = %skills_dir.display(),
                "simulacra.vfs.mount.target" = "/skills",
                "simulacra.vfs.mount.origin" = "auto",
                "simulacra.vfs.mount.file_count" = tracing::field::Empty,
            )
            .entered();

            let (files, _bytes) =
                copy_host_dir_to_vfs(&skills_dir, "/skills", vfs, max_files, max_bytes, "/skills")?;
            tracing::Span::current().record("simulacra.vfs.mount.file_count", files as u64);
            total_mount_count += 1;
            total_file_count += files;
        }
    }

    // 2. Auto-mount system prompt files referenced by agent types
    for (agent_name, agent_type) in &config.agent_types {
        if let Some(ref prompt_path) = agent_type.system_prompt {
            // Only mount relative paths (not absolute, not inline strings)
            let is_relative = !prompt_path.starts_with('/')
                && !prompt_path.starts_with('~')
                && prompt_path.contains('/');

            if is_relative {
                let host_path = project_root.join(prompt_path);
                let vfs_path = format!("/{prompt_path}");

                if host_path.exists() {
                    // BLOCKER 3: Path traversal check — resolved path must stay
                    // within the project root (prevents ../secret.txt escapes).
                    let canonical_host = host_path.canonicalize().map_err(|e| MountError::Io {
                        context: format!(
                            "failed to canonicalize system prompt path: {}",
                            host_path.display()
                        ),
                        source: e,
                    })?;
                    let canonical_root =
                        project_root.canonicalize().map_err(|e| MountError::Io {
                            context: format!(
                                "failed to canonicalize project root: {}",
                                project_root.display()
                            ),
                            source: e,
                        })?;
                    if !canonical_host.starts_with(&canonical_root) {
                        return Err(MountError::PathTraversal {
                            prompt_path: prompt_path.to_string(),
                            resolved: canonical_host.display().to_string(),
                            root: canonical_root.display().to_string(),
                        });
                    }

                    let _span = tracing::info_span!(
                        "vfs_mount",
                        "simulacra.operation.name" = "vfs_mount",
                        "simulacra.vfs.mount.source" = %host_path.display(),
                        "simulacra.vfs.mount.target" = %vfs_path,
                        "simulacra.vfs.mount.origin" = "auto",
                        "simulacra.vfs.mount.file_count" = 1u64,
                    )
                    .entered();

                    // BLOCKER 2: Size-check system prompt before reading.
                    // System prompts should never be large; cap at 1 MB.
                    const MAX_SYSTEM_PROMPT_BYTES: u64 = 1_048_576; // 1 MB
                    let prompt_meta =
                        std::fs::metadata(&host_path).map_err(|e| MountError::Io {
                            context: format!(
                                "failed to stat system prompt: {}",
                                host_path.display()
                            ),
                            source: e,
                        })?;
                    if prompt_meta.len() > MAX_SYSTEM_PROMPT_BYTES {
                        return Err(MountError::PromptTooLarge {
                            prompt_path: prompt_path.to_string(),
                            size: prompt_meta.len(),
                        });
                    }

                    let data = std::fs::read(&host_path).map_err(|e| MountError::Io {
                        context: format!("failed to read system prompt: {}", host_path.display()),
                        source: e,
                    })?;
                    // Ensure parent directories exist in VFS
                    if let Some(parent) = Path::new(&vfs_path).parent() {
                        let parent_str = parent.to_string_lossy();
                        if parent_str != "/" {
                            let _ = vfs.mkdir(&parent_str);
                        }
                    }
                    vfs.write(&vfs_path, &data)
                        .map_err(|e| MountError::VfsWrite {
                            path: vfs_path.clone(),
                            detail: e.to_string(),
                        })?;
                    total_mount_count += 1;
                    total_file_count += 1;
                } else if agent_name == entry_agent {
                    return Err(MountError::EntryPromptNotFound {
                        prompt_path: prompt_path.to_string(),
                        resolved: host_path.display().to_string(),
                    });
                } else {
                    tracing::warn!(
                        "simulacra.vfs.mount.source" = %prompt_path,
                        "message" = "missing non-entry-agent system prompt skipped",
                        "agent" = %agent_name,
                    );
                }
            }
        }
    }

    // 3. Process configured [[vfs.mounts]]
    for mount in &vfs_config.mounts {
        // Validate target
        if !mount.target.starts_with('/') {
            tracing::error!(
                "simulacra.vfs.mount.source" = %mount.source,
                "simulacra.vfs.mount.target" = %mount.target,
                "message" = "mount target must be an absolute path",
            );
            return Err(MountError::InvalidMountTarget(format!(
                "mount target '{}' must be an absolute path (start with '/')",
                mount.target
            )));
        }
        if mount.target == "/" {
            tracing::error!(
                "simulacra.vfs.mount.source" = %mount.source,
                "simulacra.vfs.mount.target" = "/",
                "message" = "mounting to root is not allowed",
            );
            return Err(MountError::InvalidMountTarget(
                "mount target '/' is the root \u{2014} mounting to root is not allowed".to_string(),
            ));
        }

        // Resolve source path
        let source_path = resolve_mount_source(&mount.source, project_root);
        if !source_path.exists() {
            tracing::error!(
                "simulacra.vfs.mount.source" = %source_path.display(),
                "simulacra.vfs.mount.target" = %mount.target,
                "message" = "mount source does not exist",
            );
            return Err(MountError::SourceNotFound {
                source_path: source_path.display().to_string(),
                target: mount.target.clone(),
            });
        }

        let _span = tracing::info_span!(
            "vfs_mount",
            "simulacra.operation.name" = "vfs_mount",
            "simulacra.vfs.mount.source" = %source_path.display(),
            "simulacra.vfs.mount.target" = %mount.target,
            "simulacra.vfs.mount.origin" = "config",
            "simulacra.vfs.mount.file_count" = tracing::field::Empty,
        )
        .entered();

        let (files, _bytes) = copy_host_dir_to_vfs(
            &source_path,
            &mount.target,
            vfs,
            max_files,
            max_bytes,
            &mount.target,
        )?;
        tracing::Span::current().record("simulacra.vfs.mount.file_count", files as u64);
        total_mount_count += 1;
        total_file_count += files;
    }

    // Emit bootstrap completion info event
    tracing::info!(
        "simulacra.vfs.mount.count" = total_mount_count,
        "simulacra.vfs.mount.file_total" = total_file_count,
        "VFS host mounts complete"
    );

    Ok(())
}
