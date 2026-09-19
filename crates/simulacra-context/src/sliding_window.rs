//! The sliding-window strategy: keep the system prefix and as much of the
//! recent tail as the token budget allows.

use crate::budget::{enforce_token_budget, kept_window_start};
use crate::{ContextStrategy, Message, Role, message_tokens};

/// Sliding-window context strategy.
///
/// Keeps the system message (first message if it has role System)
/// plus as many recent messages as fit within the token limit.
/// Sizes the kept window with a real BPE token counter (cl100k_base).
pub struct SlidingWindowStrategy;

impl SlidingWindowStrategy {
    pub fn new() -> Self {
        Self
    }

    /// Estimate tokens for a message with the shared BPE encoder.
    fn estimate_tokens(message: &Message) -> u64 {
        message_tokens(message)
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

        let mut result = Vec::new();
        let mut remaining = token_limit;

        // Preserve the system message if present.
        let rest = if messages[0].role == Role::System {
            let cost = Self::estimate_tokens(&messages[0]);
            // System is always kept (its instructions matter even when it alone
            // exceeds budget); saturate so we never underflow. We do NOT early
            // return here — the kept-window fallback below still keeps the most
            // recent user turn, so the result is never system-only / empty.
            remaining = remaining.saturating_sub(cost);
            result.push(messages[0].clone());
            &messages[1..]
        } else {
            messages
        };

        // Walk from the end to find the start index that fits within budget.
        let mut start_idx = rest.len();
        for (i, msg) in rest.iter().enumerate().rev() {
            let cost = Self::estimate_tokens(msg);
            if cost > remaining {
                break;
            }
            remaining -= cost;
            start_idx = i;
        }

        // Never start with orphaned tool results.
        let start_idx = kept_window_start(rest, start_idx);
        result.extend_from_slice(&rest[start_idx..]);

        // The kept window is valid but not yet bounded — see
        // `enforce_token_budget`.
        enforce_token_budget(&mut result, token_limit);

        result
    }
}
