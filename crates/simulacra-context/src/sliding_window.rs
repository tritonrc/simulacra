//! The sliding-window strategy: keep the system prefix, a pinned prefix behind
//! it, and as much of the recent tail as the token budget allows.

use crate::budget::{enforce_token_budget, exchange_edge, kept_window_start};
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
    ///
    /// The caller's pinned prefix must be provider-valid on its own, i.e. begin
    /// with a `Role::User` turn. More than `n` messages are protected when `n`
    /// lands inside a tool exchange: the boundary moves to the end of that
    /// exchange, which enlarges the protected residual accordingly.
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

        // Snap to an exchange edge: a boundary anywhere inside a call/result
        // run would strand one half of it. Only pinning moves the boundary, so
        // a prefix of zero keeps the System-only head untouched.
        if self.pinned_prefix > 0 {
            head_end = exchange_edge(messages, head_end);
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
