//! The sliding-window strategy: keep the system prefix, a pinned prefix behind
//! it, and as much of the recent tail as the token budget allows.

use crate::budget::{enforce_token_budget, kept_window_start};
use crate::{ContextStrategy, Message, Role, message_tokens};

/// Sliding-window context strategy.
///
/// Keeps the system message (first message if it has role System), a pinned
/// prefix of messages directly after it, plus as many recent messages as fit
/// within the token limit. Sizes the kept window with a real BPE token counter
/// (cl100k_base).
pub struct SlidingWindowStrategy {
    pinned_prefix: usize,
}

impl SlidingWindowStrategy {
    pub fn new() -> Self {
        Self::with_pinned_prefix(0)
    }

    /// The `n` messages directly after System are never evicted by the tail
    /// scan, leading normalisation, or the block-drop pass. Content-shrinking
    /// passes still apply to them.
    pub fn with_pinned_prefix(n: usize) -> Self {
        Self { pinned_prefix: n }
    }
}

impl Default for SlidingWindowStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextStrategy for SlidingWindowStrategy {
    fn compact(&self, messages: &[Message], token_limit: u64) -> Vec<Message> {
        if messages.is_empty() {
            return Vec::new();
        }

        let offset = usize::from(messages[0].role == Role::System);
        let mut head_end = offset
            .saturating_add(self.pinned_prefix)
            .min(messages.len());

        // A pinned call's results are not orphans: keep them with the head so
        // the boundary never lands between a tool_use and its tool_result.
        if head_end > 0 && !messages[head_end - 1].tool_calls.is_empty() {
            while head_end < messages.len() && messages[head_end].role == Role::Tool {
                head_end += 1;
            }
        }

        let (head, rest) = messages.split_at(head_end);

        // The head is always kept — its instructions and the frames rebuilt
        // behind them matter even when they alone exceed the budget — so
        // saturate rather than underflow, and do NOT early return: the
        // kept-window fallback below still restores the most recent user turn.
        let mut remaining = token_limit;
        for message in head {
            remaining = remaining.saturating_sub(message_tokens(message));
        }
        let mut result = head.to_vec();

        // Walk from the end to find the start index that fits within budget.
        let mut start = rest.len();
        for (i, message) in rest.iter().enumerate().rev() {
            let cost = message_tokens(message);
            if cost > remaining {
                break;
            }
            remaining -= cost;
            start = i;
        }

        // Anchored on `rest`, not the whole slice: a pinned synthetic user
        // frame must never act as the last-user anchor and drag the tail back
        // in behind it.
        let start = kept_window_start(rest, start);
        result.extend_from_slice(&rest[start..]);

        // The kept window is valid but not yet bounded — see
        // `enforce_token_budget`.
        enforce_token_budget(&mut result, token_limit, head_end);

        result
    }
}
