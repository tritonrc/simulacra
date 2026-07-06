use simulacra_types::VirtualFs;

use crate::parser::{Redirect, RedirectKind, RedirectStream, RedirectTarget};
use crate::{CommandResult, DEV_NULL};

pub(crate) fn apply_redirects(
    result: &mut CommandResult,
    redirects: &[Redirect],
    vfs: &dyn VirtualFs,
    cwd: &str,
    mut expand: impl FnMut(&str) -> String,
) -> Result<(), String> {
    if redirects.is_empty() {
        return Ok(());
    }

    let mut stdout_dest = RedirectDestination::Stdout;
    let mut stderr_dest = RedirectDestination::Stderr;

    for redirect in redirects {
        let target = match &redirect.target {
            RedirectTarget::Stdout => stdout_dest.clone(),
            RedirectTarget::Stderr => stderr_dest.clone(),
            RedirectTarget::File(target, literal) => {
                let target = if *literal {
                    target.clone()
                } else {
                    expand(target)
                };
                let target = crate::executor::resolve_against_cwd(&target, cwd);
                if target == DEV_NULL {
                    RedirectDestination::Null
                } else {
                    RedirectDestination::File {
                        target,
                        kind: redirect.kind,
                    }
                }
            }
        };

        match redirect.stream {
            RedirectStream::Stdout => stdout_dest = target,
            RedirectStream::Stderr => stderr_dest = target,
            RedirectStream::StdoutAndStderr => {
                stdout_dest = target.clone();
                stderr_dest = target;
            }
        }
    }

    let original_stdout = std::mem::take(&mut result.stdout);
    let original_stderr = std::mem::take(&mut result.stderr);
    let mut file_writes = Vec::new();

    emit_redirected_output(
        result,
        &mut file_writes,
        stdout_dest,
        original_stdout.clone(),
    );
    emit_redirected_output(
        result,
        &mut file_writes,
        stderr_dest,
        original_stderr.clone(),
    );

    if let Err(message) = write_redirect_files(vfs, file_writes) {
        result.stdout = original_stdout;
        result.stderr = original_stderr;
        return Err(message);
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RedirectDestination {
    Stdout,
    Stderr,
    Null,
    File { target: String, kind: RedirectKind },
}

struct FileRedirectWrite {
    target: String,
    kind: RedirectKind,
    content: String,
}

fn emit_redirected_output(
    result: &mut CommandResult,
    file_writes: &mut Vec<FileRedirectWrite>,
    destination: RedirectDestination,
    content: String,
) {
    match destination {
        RedirectDestination::Stdout => result.stdout.push_str(&content),
        RedirectDestination::Stderr => result.stderr.push_str(&content),
        RedirectDestination::Null => {}
        RedirectDestination::File { target, kind } => {
            file_writes.push(FileRedirectWrite {
                target,
                kind,
                content,
            });
        }
    }
}

fn write_redirect_files(
    vfs: &dyn VirtualFs,
    file_writes: Vec<FileRedirectWrite>,
) -> Result<(), String> {
    let mut merged: Vec<FileRedirectWrite> = Vec::new();
    for write in file_writes {
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| existing.target == write.target && existing.kind == write.kind)
        {
            existing.content.push_str(&write.content);
        } else {
            merged.push(write);
        }
    }

    for write in merged {
        match write.kind {
            RedirectKind::Truncate => {
                vfs.write(&write.target, write.content.as_bytes())
                    .map_err(|e| format!("redirect: {}: {e}\n", write.target))?;
            }
            RedirectKind::Append => {
                let existing = match vfs.read(&write.target) {
                    Ok(d) => String::from_utf8_lossy(&d).to_string(),
                    Err(simulacra_types::VfsError::NotFound(_)) => String::new(),
                    Err(e) => return Err(format!("redirect: {}: {e}\n", write.target)),
                };
                let combined = format!("{}{}", existing, write.content);
                vfs.write(&write.target, combined.as_bytes())
                    .map_err(|e| format!("redirect: {}: {e}\n", write.target))?;
            }
        }
    }

    Ok(())
}
