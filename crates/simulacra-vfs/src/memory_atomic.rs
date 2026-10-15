//! Atomic operations for `MemoryFs`. All of them run under the write lock, so
//! readers never see half of one. Every item validates fully before it mutates,
//! which makes a single item atomic on its own. A batch of two or more runs
//! against a scratch copy of the map that is swapped in only if every item
//! succeeded; the copy exists solely for rollback and costs O(total bytes),
//! which suits an in-memory filesystem used for agent scratch space and tests.

use std::collections::BTreeMap;

use simulacra_types::{FsMutation, Precondition, VfsError};

use crate::memory::{Entry, State, ensure_parents};
use crate::path::normalize;

pub(crate) fn apply(state: &mut State, mutations: &[FsMutation]) -> Result<(), VfsError> {
    match mutations {
        [] => return Ok(()),
        [one] => return apply_one(state, one),
        _ => {}
    }
    let mut scratch = State {
        map: state.map.clone(),
        next_revision: state.next_revision,
    };
    for mutation in mutations {
        apply_one(&mut scratch, mutation)?;
    }
    *state = scratch;
    Ok(())
}

fn check(
    map: &BTreeMap<String, Entry>,
    path: &str,
    precondition: Precondition,
) -> Result<(), VfsError> {
    match (precondition, map.get(path)) {
        (Precondition::Unconditional, _) => Ok(()),
        (Precondition::MustBeAbsent, None) => Ok(()),
        (Precondition::MustBeAbsent, Some(_)) => Err(VfsError::Conflict(format!("{path} exists"))),
        (Precondition::MustMatch(want), Some(Entry::File { revision, .. }))
            if *revision == want =>
        {
            Ok(())
        }
        (Precondition::MustMatch(want), _) => Err(VfsError::Conflict(format!(
            "{path} is not at revision {want}"
        ))),
    }
}

fn subtree(map: &BTreeMap<String, Entry>, path: &str) -> Vec<String> {
    let prefix = format!("{path}/");
    map.keys()
        .filter(|k| *k == path || k.starts_with(&prefix))
        .cloned()
        .collect()
}

/// Err(NotADirectory) if any ancestor of `path` is a file.
fn reject_file_ancestor(map: &BTreeMap<String, Entry>, path: &str) -> Result<(), VfsError> {
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur = format!("{cur}/{seg}");
        if cur == path {
            break;
        }
        if matches!(map.get(&cur), Some(Entry::File { .. })) {
            return Err(VfsError::NotADirectory(cur));
        }
    }
    Ok(())
}

/// A conditional write. Takes ownership of `data` so callers that already
/// hold a `Vec` do not copy it again.
pub(crate) fn write_if(
    state: &mut State,
    path: &str,
    data: Vec<u8>,
    precondition: Precondition,
) -> Result<(), VfsError> {
    let path = normalize(path);
    if path == "/" {
        return Err(VfsError::NotAFile(path));
    }
    check(&state.map, &path, precondition)?;
    if matches!(state.map.get(&path), Some(Entry::Dir)) {
        return Err(VfsError::NotAFile(path));
    }
    reject_file_ancestor(&state.map, &path)?;
    ensure_parents(&mut state.map, &path);
    let revision = state.take_revision();
    state.map.insert(path, Entry::File { data, revision });
    Ok(())
}

/// Every arm validates completely before it mutates, so a failing item leaves
/// `state` untouched.
fn apply_one(state: &mut State, mutation: &FsMutation) -> Result<(), VfsError> {
    match mutation {
        FsMutation::Write {
            path,
            data,
            precondition,
        } => write_if(state, path, data.clone(), *precondition)?,
        FsMutation::Mkdir { path } => {
            let path = normalize(path);
            match state.map.get(&path) {
                Some(Entry::Dir) => {}
                Some(Entry::File { .. }) => return Err(VfsError::NotAFile(path)),
                None => {
                    reject_file_ancestor(&state.map, &path)?;
                    ensure_parents(&mut state.map, &path);
                    state.map.insert(path, Entry::Dir);
                }
            }
        }
        FsMutation::Remove { path, precondition } => {
            let path = normalize(path);
            if path == "/" {
                return Err(VfsError::Io("cannot remove root".to_owned()));
            }
            if !state.map.contains_key(&path) {
                return Err(VfsError::NotFound(path));
            }
            check(&state.map, &path, *precondition)?;
            for key in subtree(&state.map, &path) {
                state.map.remove(&key);
            }
        }
        FsMutation::Rename {
            from,
            to,
            precondition,
        } => {
            let (from, to) = (normalize(from), normalize(to));
            if !state.map.contains_key(&from) {
                return Err(VfsError::NotFound(from));
            }
            check(&state.map, &from, *precondition)?;
            if state.map.contains_key(&to) {
                return Err(VfsError::Conflict(format!("{to} exists")));
            }
            if from == "/" || to.starts_with(&format!("{from}/")) {
                return Err(VfsError::Io(format!("cannot move {from} into itself")));
            }
            reject_file_ancestor(&state.map, &to)?;
            ensure_parents(&mut state.map, &to);
            for key in subtree(&state.map, &from) {
                let entry = state.map.remove(&key).expect("key came from the map");
                let moved = format!("{to}{}", &key[from.len()..]);
                let entry = match entry {
                    Entry::File { data, .. } => Entry::File {
                        data,
                        revision: state.take_revision(),
                    },
                    Entry::Dir => Entry::Dir,
                };
                state.map.insert(moved, entry);
            }
        }
    }
    Ok(())
}
