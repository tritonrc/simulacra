//! Gating of the atomic operations. `/proc` is read-only except
//! `/proc/mailbox`, which belongs to the inner stack (`MailboxFs`), so a call
//! touching the mailbox is forwarded and that layer answers for it.

use simulacra_types::{VfsError, VirtualFs};
use tracing::warn;

use super::{ProcFs, is_mailbox, is_proc};
use crate::atomic_paths::{Touch, is_root};

impl<V: VirtualFs> ProcFs<V> {
    /// Refuses a mutation before it reaches `inner` if any touched path is
    /// under the read-only part of `/proc`. It also refuses removing or
    /// moving away `/` with `PermissionDenied`, so a whole-tree removal
    /// through a read-only layer does not depend on what `inner` permits.
    pub(super) fn gate(&self, touches: &[Touch]) -> Result<(), VfsError> {
        for touch in touches {
            let path = touch.path.as_str();
            if is_proc(path) && !is_mailbox(path) {
                warn!(
                    simulacra.procfs.path = path,
                    "mutation attempt on read-only procfs path"
                );
                return Err(VfsError::PermissionDenied(format!("{path} is read-only")));
            }
            if touch.takes_subtree && is_root(path) {
                return Err(VfsError::PermissionDenied(format!(
                    "{path}: whole-tree removal through read-only /proc is refused"
                )));
            }
        }
        Ok(())
    }
}
