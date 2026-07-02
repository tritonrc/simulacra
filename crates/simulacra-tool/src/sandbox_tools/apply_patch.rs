use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_sandbox::{AgentCell, SandboxError, VfsMutation, VfsWritePrecondition};
use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError, ToolOutput};

use super::{map_sandbox_error, require_str};

pub(crate) struct ApplyPatchTool {
    pub(crate) cell: Arc<AgentCell>,
}

impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: "Apply a Simulacra-style patch to the VFS.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Patch text using the Simulacra patch grammar."
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let patch = require_str(&args, "patch")?;
            match plan_patch(&self.cell, &patch) {
                Ok(plan) => apply_plan(&self.cell, plan),
                Err(PatchError::User(message)) => Ok(ToolOutput::error(message).to_value()),
                Err(PatchError::Sandbox(err)) => Err(map_sandbox_error(err)),
            }
        })
    }
}

#[derive(Debug)]
enum PatchError {
    User(String),
    Sandbox(SandboxError),
}

impl From<String> for PatchError {
    fn from(value: String) -> Self {
        Self::User(value)
    }
}

impl From<SandboxError> for PatchError {
    fn from(value: SandboxError) -> Self {
        Self::Sandbox(value)
    }
}

#[derive(Debug, Clone)]
enum PatchOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, Clone)]
struct Hunk {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone)]
enum PlannedChange {
    Write {
        path: String,
        content: String,
        precondition: VfsWritePrecondition,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    MoveAndWrite {
        from: String,
        to: String,
        content: String,
        from_precondition: Option<Vec<u8>>,
    },
}

fn plan_patch(cell: &AgentCell, patch: &str) -> Result<Vec<PlannedChange>, PatchError> {
    let ops = parse_patch(patch)?;
    let mut changes = Vec::new();
    let mut touched_paths = HashSet::new();

    for op in ops {
        match op {
            PatchOp::Add { path, content } => {
                mark_touched_path(&mut touched_paths, &path)?;
                ensure_missing(cell, &path, "add")?;
                changes.push(PlannedChange::Write {
                    path,
                    content,
                    precondition: VfsWritePrecondition::Missing,
                });
            }
            PatchOp::Delete { path } => {
                mark_touched_path(&mut touched_paths, &path)?;
                ensure_present_for_write(cell, &path, "delete")?;
                changes.push(PlannedChange::Delete { path });
            }
            PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                if let Some(to) = move_to.as_ref()
                    && hunks.is_empty()
                {
                    mark_touched_path(&mut touched_paths, &path)?;
                    mark_touched_path(&mut touched_paths, to)?;
                    ensure_present_for_write(cell, &path, "move")?;
                    ensure_missing(cell, to, "move")?;
                    changes.push(PlannedChange::Move {
                        from: path,
                        to: to.clone(),
                    });
                    continue;
                }

                let original = read_utf8(cell, &path).map_err(|err| match err {
                    SandboxError::CapabilityDenied(_) => PatchError::Sandbox(err),
                    SandboxError::BudgetExhausted(_) => PatchError::Sandbox(err),
                    other => PatchError::User(format!("update failed for {path}: {other}")),
                })?;
                let original_bytes = original.as_bytes().to_vec();
                let updated = apply_hunks(&path, original, &hunks)?;
                match move_to {
                    Some(to) => {
                        mark_touched_path(&mut touched_paths, &path)?;
                        mark_touched_path(&mut touched_paths, &to)?;
                        ensure_missing(cell, &to, "move")?;
                        changes.push(PlannedChange::MoveAndWrite {
                            from: path,
                            to,
                            content: updated,
                            from_precondition: Some(original_bytes),
                        });
                    }
                    None => {
                        mark_touched_path(&mut touched_paths, &path)?;
                        changes.push(PlannedChange::Write {
                            path,
                            content: updated,
                            precondition: VfsWritePrecondition::Matches(original_bytes),
                        });
                    }
                }
            }
        }
    }

    Ok(changes)
}

fn apply_plan(cell: &AgentCell, changes: Vec<PlannedChange>) -> Result<Value, ToolError> {
    let mut touched = Vec::new();
    let mut mutations = Vec::new();
    for change in &changes {
        match change {
            PlannedChange::Write {
                path,
                content,
                precondition,
            } => {
                mutations.push(VfsMutation::Write {
                    path: path.clone(),
                    data: content.as_bytes().to_vec(),
                    precondition: precondition.clone(),
                });
                touched.push(path.clone());
            }
            PlannedChange::Delete { path } => {
                mutations.push(VfsMutation::Delete { path: path.clone() });
                touched.push(path.clone());
            }
            PlannedChange::Move { from, to } => {
                mutations.push(VfsMutation::Move {
                    from: from.clone(),
                    to: to.clone(),
                });
                touched.push(format!("{from} -> {to}"));
            }
            PlannedChange::MoveAndWrite {
                from,
                to,
                content,
                from_precondition,
            } => {
                mutations.push(VfsMutation::MoveAndWrite {
                    from: from.clone(),
                    to: to.clone(),
                    data: content.as_bytes().to_vec(),
                    from_precondition: from_precondition.clone(),
                });
                touched.push(format!("{from} -> {to}"));
            }
        }
    }

    cell.apply_vfs_mutations("apply_patch", &mutations)
        .map_err(map_sandbox_error)?;

    let message = if touched.is_empty() {
        "patch applied with no file changes".to_string()
    } else {
        format!("patch applied: {}", touched.join(", "))
    };
    Ok(ToolOutput::success(message)
        .with_structured(json!({ "changed": touched }))
        .to_value())
}

fn mark_touched_path(touched_paths: &mut HashSet<String>, path: &str) -> Result<(), PatchError> {
    if !touched_paths.insert(path.to_string()) {
        return Err(PatchError::User(format!("duplicate path in patch: {path}")));
    }
    Ok(())
}

fn read_utf8(cell: &AgentCell, path: &str) -> Result<String, SandboxError> {
    let bytes = cell.read_file(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn ensure_missing(cell: &AgentCell, path: &str, operation: &str) -> Result<(), PatchError> {
    match cell.path_exists_for_write(path) {
        Ok(true) => Err(PatchError::User(format!(
            "{operation} target already exists: {path}"
        ))),
        Ok(false) => Ok(()),
        Err(SandboxError::CapabilityDenied(err)) => {
            Err(PatchError::Sandbox(SandboxError::CapabilityDenied(err)))
        }
        Err(SandboxError::BudgetExhausted(err)) => {
            Err(PatchError::Sandbox(SandboxError::BudgetExhausted(err)))
        }
        Err(err) => Err(PatchError::User(format!(
            "{operation} target check failed for {path}: {err}"
        ))),
    }
}

fn ensure_present_for_write(
    cell: &AgentCell,
    path: &str,
    operation: &str,
) -> Result<(), PatchError> {
    match cell.path_exists_for_write(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(PatchError::User(format!(
            "{operation} source does not exist: {path}"
        ))),
        Err(SandboxError::CapabilityDenied(err)) => {
            Err(PatchError::Sandbox(SandboxError::CapabilityDenied(err)))
        }
        Err(SandboxError::BudgetExhausted(err)) => {
            Err(PatchError::Sandbox(SandboxError::BudgetExhausted(err)))
        }
        Err(err) => Err(PatchError::User(format!(
            "{operation} source check failed for {path}: {err}"
        ))),
    }
}

fn parse_patch(patch: &str) -> Result<Vec<PatchOp>, String> {
    let lines: Vec<&str> = patch.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") {
        return Err("malformed patch: missing *** Begin Patch".into());
    }
    if lines.last() != Some(&"*** End Patch") {
        return Err("malformed patch: missing *** End Patch".into());
    }

    let mut ops = Vec::new();
    let mut index = 1;
    while index + 1 < lines.len() {
        let line = lines[index];
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut added = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                let Some(content) = lines[index].strip_prefix('+') else {
                    return Err(format!("malformed add for {path}: expected + line"));
                };
                added.push(content.to_string());
                index += 1;
            }
            ops.push(PatchOp::Add {
                path: normalize_vfs_path(path),
                content: join_patch_lines(&added),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete {
                path: normalize_vfs_path(path),
            });
            index += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let mut move_to = None;
            if index + 1 < lines.len()
                && let Some(path) = lines[index].strip_prefix("*** Move to: ")
            {
                move_to = Some(normalize_vfs_path(path));
                index += 1;
            }
            let mut hunks = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                if lines[index].starts_with("@@") {
                    index += 1;
                    let (hunk, next) = parse_hunk(&lines, index)?;
                    hunks.push(hunk);
                    index = next;
                } else {
                    return Err(format!("malformed update for {path}: expected @@ hunk"));
                }
            }
            if move_to.is_none() && hunks.is_empty() {
                return Err(format!("malformed update for {path}: expected @@ hunk"));
            }
            ops.push(PatchOp::Update {
                path: normalize_vfs_path(path),
                move_to,
                hunks,
            });
        } else if line == "*** End of File" {
            index += 1;
        } else {
            return Err(format!("malformed patch line: {line}"));
        }
    }

    Ok(ops)
}

fn parse_hunk(lines: &[&str], mut index: usize) -> Result<(Hunk, usize), String> {
    let mut old_lines = Vec::new();
    let mut new_lines = Vec::new();
    while index + 1 < lines.len() {
        let line = lines[index];
        if line.starts_with("*** ") || line.starts_with("@@") {
            break;
        }
        if let Some(context) = line.strip_prefix(' ') {
            old_lines.push(context.to_string());
            new_lines.push(context.to_string());
        } else if let Some(removed) = line.strip_prefix('-') {
            old_lines.push(removed.to_string());
        } else if let Some(added) = line.strip_prefix('+') {
            new_lines.push(added.to_string());
        } else {
            return Err(format!("malformed hunk line: {line}"));
        }
        index += 1;
    }

    if old_lines.is_empty() && new_lines.is_empty() {
        return Err("malformed hunk: empty hunk".into());
    }

    Ok((
        Hunk {
            old_text: join_patch_lines(&old_lines),
            new_text: join_patch_lines(&new_lines),
        },
        index,
    ))
}

fn apply_hunks(path: &str, mut content: String, hunks: &[Hunk]) -> Result<String, String> {
    for hunk in hunks {
        if hunk.old_text.is_empty() {
            return Err(format!("stale hunk for {path}: empty old text"));
        }
        let count = content.matches(&hunk.old_text).count();
        if count != 1 {
            return Err(format!(
                "stale hunk for {path}: expected one match, found {count}"
            ));
        }
        content = content.replacen(&hunk.old_text, &hunk.new_text, 1);
    }
    Ok(content)
}

fn join_patch_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn normalize_vfs_path(path: &str) -> String {
    let mut parts = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}
