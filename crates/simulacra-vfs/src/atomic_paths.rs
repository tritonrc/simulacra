//! The paths an atomic call touches, in one shape every gating decorator
//! reads. A decorator that owns a prefix answers for `write_if`, `rename`
//! and `apply_batch` by checking these against the prefix before it forwards,
//! so a refused call changes nothing below it.
//!
//! Every path here is normalized (`..`, `.` and repeated slashes resolved), so
//! a gate judges the path the layers below will act on, not the spelling the
//! caller chose. A decorator must forward the normalized call (see
//! [`normalize_mutations`]) for the same reason.

use simulacra_types::FsMutation;

use crate::path::normalize;

pub(crate) struct Touch {
    /// The normalized path.
    pub(crate) path: String,
    /// The call removes this path or moves it away, which takes everything
    /// beneath it too. Such a path can destroy a prefix it merely contains.
    pub(crate) takes_subtree: bool,
}

impl Touch {
    fn new(path: &str, takes_subtree: bool) -> Self {
        Self {
            path: normalize(path),
            takes_subtree,
        }
    }
}

/// A write that leaves `path` in place, as `write_if` does.
pub(crate) fn written(path: &str) -> [Touch; 1] {
    [Touch::new(path, false)]
}

/// A removal of `path`, as plain `remove` does.
pub(crate) fn removed(path: &str) -> [Touch; 1] {
    [Touch::new(path, true)]
}

pub(crate) fn renamed(from: &str, to: &str) -> [Touch; 2] {
    [Touch::new(from, true), Touch::new(to, false)]
}

pub(crate) fn batched(mutations: &[FsMutation]) -> Vec<Touch> {
    let mut touches = Vec::new();
    for mutation in mutations {
        // `paths()` lists a rename's source first.
        for (index, path) in mutation.paths().into_iter().enumerate() {
            let takes_subtree = match mutation {
                FsMutation::Remove { .. } => true,
                FsMutation::Rename { .. } => index == 0,
                FsMutation::Write { .. } | FsMutation::Mkdir { .. } => false,
            };
            touches.push(Touch::new(path, takes_subtree));
        }
    }
    touches
}

/// True when `path` is a strict ancestor of `prefix` (`/` and every parent of
/// it), so removing or moving `path` away takes `prefix` with it. Both are
/// normalized here; `prefix` may carry a trailing slash.
pub(crate) fn is_ancestor(path: &str, prefix: &str) -> bool {
    let path = normalize(path);
    let prefix = normalize(prefix);
    path == "/" || (prefix != path && prefix.starts_with(&format!("{path}/")))
}

pub(crate) fn is_root(path: &str) -> bool {
    normalize(path) == "/"
}

/// The batch with every path normalized, as the single-path calls pass theirs.
pub fn normalize_mutations(mutations: &[FsMutation]) -> Vec<FsMutation> {
    mutations
        .iter()
        .map(|mutation| match mutation.clone() {
            FsMutation::Write {
                path,
                data,
                precondition,
            } => FsMutation::Write {
                path: normalize(&path),
                data,
                precondition,
            },
            FsMutation::Mkdir { path } => FsMutation::Mkdir {
                path: normalize(&path),
            },
            FsMutation::Remove { path, precondition } => FsMutation::Remove {
                path: normalize(&path),
                precondition,
            },
            FsMutation::Rename {
                from,
                to,
                precondition,
            } => FsMutation::Rename {
                from: normalize(&from),
                to: normalize(&to),
                precondition,
            },
        })
        .collect()
}
