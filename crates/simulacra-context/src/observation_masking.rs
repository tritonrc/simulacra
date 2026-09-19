//! The observation-masking strategy: elide old tool results before falling
//! back to the sliding window.

use crate::budget::{enforce_token_budget, kept_window_start};
use crate::{ContextStrategy, Message, Role, message_tokens};

/// Observation-masking context strategy.
///
/// Tool result messages older than the recency window are replaced with
/// a short placeholder: `"[output elided — N chars]"`. All other message
/// types (System, User, Assistant) are preserved in full, keeping the
/// agent's complete reasoning and action history while dropping verbose
/// old tool outputs that dominate context usage.
///
/// After masking, if the result still exceeds `token_limit`, a sliding
/// window is applied to the remaining non-system messages (oldest first).
///
/// Rationale: JetBrains/NeurIPS 2025 research shows tool outputs are
/// ~84% of SE agent context. Masking them matches LLM summarization
/// accuracy at ~50% lower cost with zero additional LLM calls.
pub struct ObservationMaskingStrategy {
    /// Number of most-recent tool result messages to keep verbatim.
    keep_recent_tool_results: usize,
}

impl ObservationMaskingStrategy {
    pub fn new(keep_recent_tool_results: usize) -> Self {
        Self {
            keep_recent_tool_results,
        }
    }

    fn estimate_tokens(message: &Message) -> u64 {
        message_tokens(message)
    }
}

impl ContextStrategy for ObservationMaskingStrategy {
    fn compact(&self, messages: &[Message], token_limit: u64) -> Vec<Message> {
        if messages.is_empty() {
            return Vec::new();
        }

        // 1. Identify which tool messages are in the recency window.
        //    Walk backwards to find the last N tool results.
        let tool_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Tool)
            .map(|(i, _)| i)
            .collect();

        let cutoff = tool_indices
            .len()
            .saturating_sub(self.keep_recent_tool_results);
        let old_tool_indices: std::collections::HashSet<usize> =
            tool_indices[..cutoff].iter().copied().collect();

        // 2. Build masked messages.
        let mut masked: Vec<Message> = messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                if old_tool_indices.contains(&i) {
                    let original_len = msg.content.len();
                    Message {
                        role: Role::Tool,
                        content: format!("[output elided — {original_len} chars]"),
                        tool_calls: msg.tool_calls.clone(),
                        tool_call_id: msg.tool_call_id.clone(),
                        provider_content: msg.provider_content.clone(),
                    }
                } else {
                    msg.clone()
                }
            })
            .collect();

        // 3. Check if we fit within token_limit after masking.
        let total: u64 = masked.iter().map(Self::estimate_tokens).sum();
        if total <= token_limit {
            return masked;
        }

        // 4. Fallback: sliding window on non-system messages.
        let mut result = Vec::new();
        let mut remaining = token_limit;

        let (system, rest) = if masked[0].role == Role::System {
            let cost = Self::estimate_tokens(&masked[0]);
            // Always keep system; saturate; do not early-return system-only — the
            // kept-window fallback below keeps the most recent user turn.
            remaining = remaining.saturating_sub(cost);
            result.push(masked.remove(0));
            (true, masked)
        } else {
            (false, masked)
        };
        let _ = system;

        let mut start_idx = rest.len();
        for (i, msg) in rest.iter().enumerate().rev() {
            let cost = Self::estimate_tokens(msg);
            if cost > remaining {
                break;
            }
            remaining -= cost;
            start_idx = i;
        }

        let start_idx = kept_window_start(&rest, start_idx);
        result.extend_from_slice(&rest[start_idx..]);

        // The kept window is valid but not yet bounded — see
        // `enforce_token_budget`.
        enforce_token_budget(&mut result, token_limit);

        result
    }
}
