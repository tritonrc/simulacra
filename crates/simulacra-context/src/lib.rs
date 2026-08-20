//! Simulacra context crate.
//!
//! Strategies for compacting conversation history to fit within
//! a provider's token limit.

pub use simulacra_types::{ContextStrategy, Message, Role};

/// After trimming, skip forward past any leading `Role::Tool` messages so we
/// never start the kept window with orphaned tool results (which would produce
/// invalid transcripts for provider APIs).
fn adjust_tool_boundary(msgs: &[Message], start: usize) -> usize {
    let mut idx = start;
    while idx < msgs.len() && msgs[idx].role == Role::Tool {
        idx += 1;
    }
    idx
}

/// Pick the kept-window start index. Skips leading orphaned `Role::Tool`
/// results (invalid without their parent tool_use), but never drops the entire
/// tail: if skipping forward would leave nothing, anchor on the most recent
/// user message so the compacted transcript is always non-empty and
/// provider-valid. Mirrors the "always keep the system message" escape hatch —
/// the provider must receive at least one coherent message block.
fn kept_window_start(msgs: &[Message], start: usize) -> usize {
    let adjusted = adjust_tool_boundary(msgs, start);
    if adjusted < msgs.len() {
        return adjusted;
    }
    // Nothing fit within budget (or only orphaned tool results remain). Anchor on
    // the most recent user message so the kept window is non-empty AND begins
    // with a user turn — providers require the first non-system message to be a
    // user turn, so an assistant- or tool-first window is invalid. If there is no
    // user message at all (a malformed transcript), fall back to `adjusted`
    // rather than starting the window on an orphaned tool result.
    msgs.iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(adjusted)
}

/// Estimated cost of a single message under the stub heuristic (4 chars = 1
/// token), covering `content`, tool-call arguments, and provider-native
/// content blocks. `provider_content` must round-trip unchanged, so
/// `enforce_token_budget` counts it but can only reclaim space from `content`;
/// the `MIN_KEPT_CONTENT_BYTES` floor keeps that pressure from gutting short
/// turns.
fn message_tokens(message: &Message) -> u64 {
    let mut bytes = message.content.len() as u64;
    // Tool-call arguments and provider-native blocks (`thinking` etc.) are
    // sent to the provider too; leaving them uncounted is how an "in budget"
    // window overshoots the real limit. They are estimated at the same 4:1
    // ratio via their serialized size.
    for call in &message.tool_calls {
        bytes += call.name.len() as u64;
        bytes += call.arguments.to_string().len() as u64;
    }
    for block in &message.provider_content {
        bytes += block.value.to_string().len() as u64;
    }
    bytes.div_ceil(4)
}

fn window_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

/// Content at or below this size is left alone by the budget pass: shrinking it
/// reclaims nothing and risks emitting an empty (provider-invalid) block.
const MIN_KEPT_CONTENT_BYTES: usize = 256;

/// Largest byte index `<= idx` that is a UTF-8 char boundary.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Marker left in place of a tool result whose body was dropped. Mirrors
/// `ObservationMaskingStrategy`'s wording so a transcript reads the same
/// whichever path elided it.
fn elided_marker(original_len: usize) -> String {
    format!("[output elided — {original_len} chars]")
}

/// Shrink `content` to at most `target_tokens`, keeping a leading slice and
/// recording how much was cut. Returns an empty string when not even the marker
/// fits — the message itself still stays in the transcript, so provider
/// validity is unaffected.
fn truncate_to_tokens(content: &str, target_tokens: u64) -> String {
    let budget_bytes = (target_tokens.saturating_mul(4)) as usize;
    if content.len() <= budget_bytes {
        return content.to_string();
    }
    let marker = format!("\n[…truncated — {} chars total]", content.len());
    if marker.len() > budget_bytes {
        // The allowance cannot even hold the marker. Emit the marker alone: it
        // records what was dropped and keeps the block non-empty, which the
        // provider requires.
        return marker;
    }
    let keep = floor_char_boundary(content, budget_bytes - marker.len());
    let mut out = String::with_capacity(keep + marker.len());
    out.push_str(&content[..keep]);
    out.push_str(&marker);
    out
}

/// Hard-bound an already-selected window so it cannot exceed `token_limit`.
///
/// `kept_window_start` guarantees a provider-VALID transcript (never
/// system-only, never leading with an orphaned tool result) but says nothing
/// about SIZE: when the tail after the last user message is itself larger than
/// the budget, it returns that tail whole. In production that produced a
/// 2,870,192-token prompt against a 1,000,000 cap. The provider rejects that
/// non-retryably, so every later turn rebuilt the same oversized prompt and the
/// conversation wedged permanently.
///
/// This pass closes the hole by shrinking CONTENT in place rather than choosing
/// between "keep the message whole" and "drop it":
///   1. tool results are elided oldest-first (they dominate context), then
///   2. remaining non-system content is truncated oldest-first, so the most
///      recent turns keep their detail longest.
///
/// No message is ever removed, so `tool_use`/`tool_result` pairing and
/// `provider_content` round-tripping both survive untouched. The system message
/// is kept whole, matching the existing escape hatch.
///
/// Guarantee: the window is bounded by `token_limit` plus an irreducible
/// residual of the system message and `MIN_KEPT_CONTENT_BYTES` per short turn.
/// It is never bounded by tool-output volume, which is the term that actually
/// runs away. Whenever the overflow comes from oversized content — every real
/// case of this bug — the result lands inside `token_limit`.
fn enforce_token_budget(messages: &mut [Message], token_limit: u64) {
    if window_tokens(messages) <= token_limit {
        return;
    }

    // Elide tool results oldest-first, but never the most recent one: the
    // model usually needs it verbatim to act (a spawn ack, a command result).
    // If that last result is itself oversized, the truncation pass below still
    // bounds it — truncation keeps a prefix instead of destroying the message.
    let last_tool = messages.iter().rposition(|m| m.role == Role::Tool);
    for i in 0..messages.len() {
        if window_tokens(messages) <= token_limit {
            return;
        }
        if messages[i].role != Role::Tool || Some(i) == last_tool {
            continue;
        }
        if messages[i].content.len() <= MIN_KEPT_CONTENT_BYTES {
            continue;
        }
        let marker = elided_marker(messages[i].content.len());
        if marker.len() < messages[i].content.len() {
            messages[i].content = marker;
        }
    }

    for i in 0..messages.len() {
        if window_tokens(messages) <= token_limit {
            return;
        }
        if messages[i].role == Role::System {
            continue;
        }
        // Below the floor there is nothing worth reclaiming, and cutting a short
        // turn to nothing would emit an empty content block — which Anthropic
        // rejects, trading one 400 for another. Leave small turns whole.
        if messages[i].content.len() <= MIN_KEPT_CONTENT_BYTES {
            continue;
        }
        // Everything before this point is already minimal, so hand this message
        // whatever slack is left. Walking oldest-first means the newest turns
        // are the last to be cut.
        let others = window_tokens(messages).saturating_sub(message_tokens(&messages[i]));
        let allowance = token_limit.saturating_sub(others);
        let shrunk = truncate_to_tokens(&messages[i].content, allowance);
        // Only take the rewrite if it actually reclaims bytes and leaves the
        // message non-empty.
        if !shrunk.is_empty() && shrunk.len() < messages[i].content.len() {
            messages[i].content = shrunk;
        }
    }
}

/// Sliding-window context strategy.
///
/// Keeps the system message (first message if it has role System)
/// plus as many recent messages as fit within the token limit.
/// Uses a stub token counter: ~4 chars per token.
pub struct SlidingWindowStrategy;

impl SlidingWindowStrategy {
    pub fn new() -> Self {
        Self
    }

    /// Estimate tokens for a message using the stub heuristic (4 chars = 1 token).
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

#[cfg(test)]
mod tests {
    use super::*;
    use simulacra_types::{Message, ToolCallMessage};

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_content: vec![],
        }
    }

    #[test]
    fn keeps_system_and_recent() {
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::System, "You are helpful."),
            msg(
                Role::User,
                "old message that is long enough to be dropped eventually",
            ),
            msg(Role::Assistant, "old reply"),
            msg(Role::User, "recent"),
        ];
        // Give enough budget for system + last message only
        let result = strategy.compact(&messages, 8);
        assert!(result[0].role == Role::System);
        assert!(result.last().unwrap().content == "recent");
    }

    #[test]
    fn empty_input() {
        let strategy = SlidingWindowStrategy::new();
        assert!(strategy.compact(&[], 100).is_empty());
    }

    fn tool_msg(content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: content.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: Some("call_1".into()),
            provider_content: vec![],
        }
    }

    #[test]
    fn observation_masking_elides_old_tool_results() {
        let strategy = ObservationMaskingStrategy::new(1);
        let messages = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "read file A"),
            msg(Role::Assistant, "calling tool"),
            tool_msg("file A contents: lots of text here that is very long"),
            msg(Role::User, "read file B"),
            msg(Role::Assistant, "calling tool"),
            tool_msg("file B contents: recent"),
        ];

        let result = strategy.compact(&messages, 10000);
        assert_eq!(result.len(), 7);
        // Old tool result (index 3) should be masked
        assert!(result[3].content.starts_with("[output elided"));
        assert!(result[3].tool_call_id == Some("call_1".into()));
        // Recent tool result (index 6) should be preserved
        assert_eq!(result[6].content, "file B contents: recent");
    }

    #[test]
    fn observation_masking_preserves_all_non_tool_messages() {
        let strategy = ObservationMaskingStrategy::new(0); // mask ALL tool results
        let messages = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "query"),
            msg(Role::Assistant, "thinking"),
            tool_msg("big output"),
            msg(Role::Assistant, "done"),
        ];

        let result = strategy.compact(&messages, 10000);
        assert_eq!(result[0].content, "sys");
        assert_eq!(result[1].content, "query");
        assert_eq!(result[2].content, "thinking");
        assert!(result[3].content.starts_with("[output elided"));
        assert_eq!(result[4].content, "done");
    }

    #[test]
    fn observation_masking_keeps_recent_n_tool_results() {
        let strategy = ObservationMaskingStrategy::new(2);
        let messages = vec![
            tool_msg("old1"),
            tool_msg("old2"),
            tool_msg("recent1"),
            tool_msg("recent2"),
        ];

        let result = strategy.compact(&messages, 10000);
        assert!(result[0].content.starts_with("[output elided"));
        assert!(result[1].content.starts_with("[output elided"));
        assert_eq!(result[2].content, "recent1");
        assert_eq!(result[3].content, "recent2");
    }

    #[test]
    fn observation_masking_falls_back_to_sliding_window_when_still_over_limit() {
        let strategy = ObservationMaskingStrategy::new(1);
        let messages = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "old query with many words"),
            msg(Role::Assistant, "old response with many words"),
            tool_msg("old tool output"),
            msg(Role::User, "new"),
        ];
        // Budget: sys=1 token, "new"=1 token, total=2; old messages won't fit
        let result = strategy.compact(&messages, 2);
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result.last().unwrap().content, "new");
    }

    #[test]
    fn observation_masking_empty_input() {
        let strategy = ObservationMaskingStrategy::new(3);
        assert!(strategy.compact(&[], 100).is_empty());
    }

    #[test]
    fn observation_masking_all_fit_no_masking_needed() {
        let strategy = ObservationMaskingStrategy::new(10);
        let messages = vec![msg(Role::System, "sys"), tool_msg("a"), tool_msg("b")];
        let result = strategy.compact(&messages, 10000);
        // All 3 tool results fit in recency window of 10 — no masking
        assert_eq!(result[1].content, "a");
        assert_eq!(result[2].content, "b");
    }

    // --- X1: Fallback test with real masking text ---

    #[test]
    fn observation_masking_fallback_includes_masked_tool_output() {
        // Set up: old tool output is large enough that even after masking,
        // the conversation still exceeds the budget, triggering the sliding
        // window fallback. Verify the masking placeholder text appears in
        // the final output for old tool results that survive the window.
        let strategy = ObservationMaskingStrategy::new(1);
        let old_tool_content = "x".repeat(200); // 200 chars = 50 tokens
        let messages = vec![
            msg(Role::System, "sys"),       // 1 token
            msg(Role::User, "q1"),          // 1 token
            msg(Role::Assistant, "a1"),     // 1 token
            tool_msg(&old_tool_content),    // masked → small
            msg(Role::User, "q2"),          // 1 token
            msg(Role::Assistant, "a2"),     // 1 token
            tool_msg("recent tool output"), // 5 tokens (kept)
        ];
        // Budget: enough for system + masked old tool + a few messages but not all.
        // sys(1) + masked_tool(~9 tokens for "[output elided — 200 chars]") + recent_tool(5)
        // + q2(1) + a2(1) = 17; set budget to 10 so fallback drops old messages.
        let result = strategy.compact(&messages, 10);
        // System message must be first
        assert_eq!(result[0].role, Role::System);
        // The recent tool output must be present
        assert!(result.iter().any(|m| m.content == "recent tool output"));
        // If any old tool result survived the window, it must be masked
        for m in &result {
            if m.role == Role::Tool && m.content != "recent tool output" {
                assert_eq!(
                    m.content,
                    format!("[output elided — {} chars]", old_tool_content.len())
                );
            }
        }
    }

    // --- X4: No system message branch ---

    #[test]
    fn sliding_window_no_system_message() {
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "hello"),   // 2 tokens
            msg(Role::Assistant, "hi"), // 1 token
            msg(Role::User, "bye"),     // 1 token
        ];
        // Budget fits all (4 tokens)
        let result = strategy.compact(&messages, 4);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].content, "hello");
        assert_eq!(result[1].content, "hi");
        assert_eq!(result[2].content, "bye");
    }

    #[test]
    fn sliding_window_no_system_message_drops_oldest() {
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "hello"),   // 2 tokens
            msg(Role::Assistant, "hi"), // 1 token
            msg(Role::User, "bye"),     // 1 token
        ];
        // Budget = 2 tokens: only "bye" fits (1 token) then "hi" (1 token)
        let result = strategy.compact(&messages, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hi");
        assert_eq!(result[1].content, "bye");
    }

    // --- X5: System prompt exceeding budget is preserved ---

    #[test]
    fn sliding_window_system_exceeds_budget_still_preserved() {
        let strategy = SlidingWindowStrategy::new();
        let long_system = "a]".repeat(100); // 200 chars = 50 tokens
        let messages = vec![msg(Role::System, &long_system), msg(Role::User, "hello")];
        // Budget is 5 tokens — system alone is 50 tokens
        let result = strategy.compact(&messages, 5);
        // System is still preserved even when it alone exceeds budget — AND the
        // most-recent user turn is kept too, so we never emit a system-only
        // (empty `messages`) transcript.
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[0].content, long_system);
        assert!(
            result
                .iter()
                .any(|m| m.role == Role::User && m.content == "hello"),
            "the user turn must be kept alongside the over-budget system prompt"
        );
    }

    // --- X6: Token boundary math (div_ceil(4)) at exact boundaries ---

    #[test]
    fn estimate_tokens_exact_multiple_of_4() {
        // 4 chars => div_ceil(4) = 1 token
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![msg(Role::User, "abcd")]; // exactly 4 chars = 1 token
        let result = strategy.compact(&messages, 1);
        assert_eq!(result.len(), 1);

        // Budget 0 can't fit even 1 token, but we never drop below the most-recent message
        let result = strategy.compact(&messages, 0);
        assert_eq!(
            result.len(),
            1,
            "never strip below the most-recent message — over budget is kept, not dropped to empty"
        );
    }

    #[test]
    fn estimate_tokens_one_over_boundary() {
        // 5 chars => div_ceil(4) = 2 tokens
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![msg(Role::User, "abcde")]; // 5 chars = 2 tokens
        let result = strategy.compact(&messages, 1);
        assert_eq!(
            result.len(),
            1,
            "never strip below the most-recent message — over budget is kept, not dropped to empty"
        ); // 2 tokens > 1 budget

        let result = strategy.compact(&messages, 2);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn estimate_tokens_boundary_8_chars() {
        // 8 chars => exactly 2 tokens
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "abcdefgh"), // 8 chars = 2 tokens
            msg(Role::User, "ijkl"),     // 4 chars = 1 token
        ];
        // Budget = 2: only "ijkl" (1 token) fits, then we try "abcdefgh" (2 tokens)
        // which doesn't fit in remaining 1 token
        let result = strategy.compact(&messages, 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "ijkl");

        // Budget = 3: both fit (2 + 1 = 3)
        let result = strategy.compact(&messages, 3);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn estimate_tokens_single_char() {
        // 1 char => div_ceil(4) = 1 token
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![msg(Role::User, "x")];
        let result = strategy.compact(&messages, 1);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn estimate_tokens_empty_content() {
        // 0 chars => div_ceil(4) = 0 tokens
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![msg(Role::User, "")];
        let result = strategy.compact(&messages, 0);
        assert_eq!(result.len(), 1); // 0 tokens fits in 0 budget
    }

    // --- X7: ObservationMasking system-over-budget fallback ---

    #[test]
    fn observation_masking_system_exceeds_budget_fallback() {
        let strategy = ObservationMaskingStrategy::new(1);
        let long_system = "s".repeat(100); // 100 chars = 25 tokens
        let messages = vec![
            msg(Role::System, &long_system),
            msg(Role::User, "hello"),
            tool_msg("tool output"),
        ];
        // After masking (no old tools to mask with keep=1 and only 1 tool),
        // total still exceeds budget of 5. Fallback: system alone > budget,
        // but we must also keep the most-recent user turn.
        let result = strategy.compact(&messages, 5);
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[0].content, long_system);
        assert!(
            result
                .iter()
                .any(|m| m.role == Role::User && m.content == "hello"),
            "the user turn must be kept alongside the over-budget system prompt"
        );
    }

    // --- X8: Sliding window exact length and order assertions ---

    #[test]
    fn sliding_window_exact_order_and_count() {
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::System, "sys"),     // 1 token
            msg(Role::User, "msg1"),      // 1 token
            msg(Role::Assistant, "msg2"), // 1 token
            msg(Role::User, "msg3"),      // 1 token
            msg(Role::Assistant, "msg4"), // 1 token
            msg(Role::User, "msg5"),      // 1 token
        ];
        // Budget = 4 tokens: system(1) + 3 most recent
        let result = strategy.compact(&messages, 4);
        assert_eq!(result.len(), 4, "expected system + 3 recent messages");
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[0].content, "sys");
        assert_eq!(result[1].content, "msg3");
        assert_eq!(result[2].content, "msg4");
        assert_eq!(result[3].content, "msg5");
    }

    #[test]
    fn sliding_window_preserves_chronological_order() {
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "first"),  // 2 tokens
            msg(Role::User, "second"), // 2 tokens
            msg(Role::User, "third"),  // 2 tokens
        ];
        // Budget = 4: "third"(2) + "second"(2) = 4, "first" won't fit
        let result = strategy.compact(&messages, 4);
        assert_eq!(result.len(), 2);
        // Must be in chronological order, not reversed
        assert_eq!(result[0].content, "second");
        assert_eq!(result[1].content, "third");
    }

    // --- X9: Full masking format assertions ---

    #[test]
    fn observation_masking_exact_placeholder_format() {
        let strategy = ObservationMaskingStrategy::new(0);
        let content = "hello world tool output";
        let messages = vec![tool_msg(content)];
        let result = strategy.compact(&messages, 10000);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].content,
            format!("[output elided — {} chars]", content.len()),
            "masking placeholder must be exactly: [output elided — <N> chars]"
        );
    }

    #[test]
    fn observation_masking_exact_format_preserves_char_count() {
        let strategy = ObservationMaskingStrategy::new(1);
        let old_content = "a]".repeat(50); // 100 chars
        let new_content = "recent";
        let messages = vec![tool_msg(&old_content), tool_msg(new_content)];
        let result = strategy.compact(&messages, 10000);
        assert_eq!(result.len(), 2);
        // Old tool result: exact format check
        assert_eq!(result[0].content, "[output elided — 100 chars]");
        // Recent tool result: untouched
        assert_eq!(result[1].content, "recent");
    }

    #[test]
    fn observation_masking_placeholder_format_with_varied_lengths() {
        let strategy = ObservationMaskingStrategy::new(0);
        let messages = vec![
            tool_msg(""),                // 0 chars
            tool_msg("x"),               // 1 char
            tool_msg(&"y".repeat(1000)), // 1000 chars
        ];
        let result = strategy.compact(&messages, 10000);
        assert_eq!(result[0].content, "[output elided — 0 chars]");
        assert_eq!(result[1].content, "[output elided — 1 chars]");
        assert_eq!(result[2].content, "[output elided — 1000 chars]");
    }

    #[test]
    fn sliding_window_keeps_a_coherent_block_when_recent_tools_exceed_budget() {
        // Production repro (S043): the coordinator emits a tool_use, then large
        // tool results. With a tiny remaining-budget token_limit the naive window
        // walks back over the tool results, runs out before the anchoring
        // assistant message, then skips FORWARD past all leading orphaned tool
        // results — leaving only the system message. Anthropic puts `system` in
        // its own field, so the messages array is EMPTY → 400 "messages: at least
        // one message is required". The kept window must always retain >=1
        // coherent (non-system, non-orphan-tool-leading) block.
        let strategy = SlidingWindowStrategy::new();
        let big = "x".repeat(4000); // ~1000 tokens each
        let messages = vec![
            msg(Role::System, "you are devforge"),
            msg(Role::User, "where is the health endpoint?"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "src/health.rs"}),
                }],
                tool_call_id: None,
                provider_content: vec![],
            }, // tool_use anchor
            tool_msg(&big),
            tool_msg(&big),
            tool_msg(&big),
            tool_msg(&big),
        ];
        // token_limit (= remaining cost budget late in a turn) far smaller than
        // the tool results. Naive impl returns just [System].
        let result = strategy.compact(&messages, 10);

        let non_system: Vec<&Message> = result.iter().filter(|m| m.role != Role::System).collect();
        assert!(
            !non_system.is_empty(),
            "compaction must never strip the transcript to system-only (empty provider messages)"
        );
        assert_eq!(
            non_system[0].role,
            Role::User,
            "kept window must begin with a user turn, not {:?}",
            non_system[0].role
        );
    }

    #[test]
    fn observation_masking_keeps_a_coherent_block_when_recent_tools_exceed_budget() {
        // Same floor for the observation-masking strategy's sliding-window fallback.
        let strategy = ObservationMaskingStrategy::new(10); // keep recent tools verbatim
        let big = "x".repeat(4000);
        let messages = vec![
            msg(Role::System, "you are devforge"),
            msg(Role::User, "where is the health endpoint?"),
            msg(Role::Assistant, "let me read the relevant files"),
            tool_msg(&big),
            tool_msg(&big),
            tool_msg(&big),
            tool_msg(&big),
        ];
        let result = strategy.compact(&messages, 10);
        let non_system: Vec<&Message> = result.iter().filter(|m| m.role != Role::System).collect();
        assert!(!non_system.is_empty(), "must never strip to system-only");
        assert_eq!(
            non_system[0].role,
            Role::User,
            "kept window must begin with a user turn, not {:?}",
            non_system[0].role
        );
    }

    #[test]
    fn sliding_window_keeps_user_turn_when_single_message_exceeds_budget() {
        // No tool messages at all: just a system prompt and one large user
        // message bigger than the (shrunken) budget. Compaction must still keep
        // the user turn — never return a system-only transcript (which becomes an
        // empty `messages` array for Anthropic → 400).
        let strategy = SlidingWindowStrategy::new();
        let big = "x".repeat(4000);
        let messages = vec![msg(Role::System, "you are devforge"), msg(Role::User, &big)];
        let result = strategy.compact(&messages, 10);
        let non_system: Vec<&Message> = result.iter().filter(|m| m.role != Role::System).collect();
        assert!(
            !non_system.is_empty(),
            "must keep the user turn even when it exceeds budget"
        );
        assert_eq!(
            non_system[0].role,
            Role::User,
            "kept window must begin with the user turn"
        );
    }

    /// Total estimated tokens of a compacted window, mirroring the strategies'
    /// own accounting so a test can assert the budget was actually respected.
    fn total_tokens(msgs: &[Message]) -> u64 {
        msgs.iter()
            .map(|m| (m.content.len() as u64).div_ceil(4))
            .sum()
    }

    /// Production repro (the "prompt is too long" incident): a long agentic
    /// stretch emits many large tool results with NO intervening user message.
    /// The backward walk breaks on the first oversized result, so `start_idx`
    /// never advances; `kept_window_start` then anchors on the most recent user
    /// message and returns the WHOLE tail after it — unbounded. In production
    /// that tail was 2,870,192 tokens against a 1,000,000 cap, and because the
    /// provider rejects it non-retryably the conversation could never recover:
    /// every later turn rebuilt the same oversized prompt.
    ///
    /// Compaction must respect `token_limit` while still returning a
    /// provider-valid window (the S043 invariant below).
    #[test]
    fn sliding_window_bounds_the_tail_after_the_last_user_message() {
        let strategy = SlidingWindowStrategy::new();
        // Each result is ~262k tokens — a single one exceeds the whole window,
        // exactly like a 1 MiB OUTPUT_CAP_BYTES workspace_exec result.
        let huge = "x".repeat(1_048_576);
        let mut messages = vec![
            msg(Role::System, "you are devforge"),
            msg(Role::User, "review this PR"),
        ];
        for _ in 0..11 {
            messages.push(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: "call_1".into(),
                    name: "workspace_exec".into(),
                    arguments: serde_json::json!({"cmd": "git log"}),
                }],
                tool_call_id: None,
                provider_content: vec![],
            });
            messages.push(tool_msg(&huge));
        }

        let limit = 128_000;
        let result = strategy.compact(&messages, limit);

        // The bug: this was ~2.87M.
        assert!(
            total_tokens(&result) <= limit,
            "compacted window must fit the budget, got {} tokens > {} limit",
            total_tokens(&result),
            limit
        );
        // The S043 invariant must still hold.
        let non_system: Vec<&Message> = result.iter().filter(|m| m.role != Role::System).collect();
        assert!(
            !non_system.is_empty(),
            "must never strip the transcript to system-only"
        );
        assert_eq!(
            non_system[0].role,
            Role::User,
            "kept window must begin with a user turn, not {:?}",
            non_system[0].role
        );
    }

    /// Same unbounded-tail hazard for the masking strategy's fallback: masking
    /// only elides tool results OLDER than the recency window, so a run whose
    /// recent results are each larger than the whole budget still overflows.
    #[test]
    fn observation_masking_bounds_the_tail_after_the_last_user_message() {
        let strategy = ObservationMaskingStrategy::new(10);
        let huge = "x".repeat(1_048_576);
        let mut messages = vec![
            msg(Role::System, "you are devforge"),
            msg(Role::User, "review this PR"),
        ];
        for _ in 0..11 {
            messages.push(msg(Role::Assistant, "reading"));
            messages.push(tool_msg(&huge));
        }

        let limit = 128_000;
        let result = strategy.compact(&messages, limit);

        assert!(
            total_tokens(&result) <= limit,
            "compacted window must fit the budget, got {} tokens > {} limit",
            total_tokens(&result),
            limit
        );
        let non_system: Vec<&Message> = result.iter().filter(|m| m.role != Role::System).collect();
        assert!(!non_system.is_empty(), "must never strip to system-only");
        assert_eq!(non_system[0].role, Role::User);
    }

    /// A single user message larger than the entire budget must be TRUNCATED to
    /// fit rather than passed through whole. The S043 escape hatch guarantees a
    /// user turn survives; it must not guarantee an oversized prompt.
    #[test]
    fn sliding_window_truncates_a_lone_oversized_user_message() {
        let strategy = SlidingWindowStrategy::new();
        let huge = "x".repeat(1_048_576);
        let messages = vec![
            msg(Role::System, "you are devforge"),
            msg(Role::User, &huge),
        ];

        let limit = 1_000;
        let result = strategy.compact(&messages, limit);

        assert!(
            total_tokens(&result) <= limit,
            "a lone oversized user turn must be truncated to fit, got {} > {}",
            total_tokens(&result),
            limit
        );
        let non_system: Vec<&Message> = result.iter().filter(|m| m.role != Role::System).collect();
        assert_eq!(non_system.len(), 1);
        assert_eq!(non_system[0].role, Role::User);
        assert!(
            !non_system[0].content.is_empty(),
            "the surviving user turn must still carry content"
        );
    }
}
