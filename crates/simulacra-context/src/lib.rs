//! Simulacra context crate.
//!
//! Strategies for compacting conversation history to fit within
//! a provider's token limit.

pub use simulacra_types::{ContextStrategy, Message, Role};
use tiktoken_rs::CoreBPE;

/// Shared `cl100k_base` BPE encoder. `cl100k_base_singleton` initializes it once
/// and hands out a `&'static` reference, so estimation has no per-call setup.
///
/// Why cl100k: Anthropic does not publish Claude's tokenizer, and DevForge's
/// agent loop targets Claude models. cl100k_base is the standard deterministic
/// offline approximation — it runs materially denser than the old 4-chars-per-
/// token stub on prose and sparser on code/JSON, which is the mis-sizing this
/// replaces. It is a heuristic, not Claude's exact count.
fn bpe() -> &'static CoreBPE {
    tiktoken_rs::cl100k_base_singleton()
}

/// Real token count of a text blob under the shared encoder.
fn count_tokens(text: &str) -> u64 {
    bpe().encode_with_special_tokens(text).len() as u64
}

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
    // Providers require the first non-system message to be a user turn, so the
    // window must always reach back to the most recent user message — whether
    // the budget walk stopped after it (an assistant-/tool-first window) or
    // found nothing at all. Size is NOT this function's concern: the window it
    // returns is bounded afterwards by `enforce_token_budget`. If there is no
    // user message at all (a malformed transcript), fall back to `adjusted`
    // rather than starting the window on an orphaned tool result.
    match msgs.iter().rposition(|m| m.role == Role::User) {
        Some(last_user) => adjusted.min(last_user),
        None => adjusted,
    }
}

/// Estimated cost of a single message in real BPE tokens, covering `content`,
/// tool-call arguments, and provider-native content blocks. `provider_content`
/// must round-trip unchanged, so `enforce_token_budget` counts it but can only
/// reclaim space from `content`; the `MIN_KEPT_CONTENT_TOKENS` floor keeps that
/// pressure from gutting short turns.
fn message_tokens(message: &Message) -> u64 {
    let mut tokens = count_tokens(&message.content);
    // Tool-call arguments and provider-native blocks (`thinking` etc.) are
    // sent to the provider too; leaving them uncounted is how an "in budget"
    // window overshoots the real limit. Ids/names are short, but counted.
    for call in &message.tool_calls {
        tokens += count_tokens(&call.id);
        tokens += count_tokens(&call.name);
        tokens += count_tokens(&call.arguments.to_string());
    }
    if let Some(id) = &message.tool_call_id {
        tokens += count_tokens(id);
    }
    for block in &message.provider_content {
        tokens += count_tokens(&block.provider);
        tokens += count_tokens(&block.value.to_string());
    }
    tokens
}

/// The share of a message's cost that compaction cannot reclaim: tool-call
/// ids/names/arguments, `tool_call_id`, and provider-native blocks all must
/// reach the provider verbatim. Only `content` is shrinkable.
fn immutable_tokens(message: &Message) -> u64 {
    message_tokens(message).saturating_sub(count_tokens(&message.content))
}

fn window_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

/// Content at or below this token cost is left alone by the budget pass:
/// shrinking it reclaims nothing and risks emitting an empty (provider-invalid)
/// block. 64 tokens ≈ the old 256-byte floor under the stub estimator, so the
/// "don't bother gutting short turns" threshold is unchanged in spirit.
const MIN_KEPT_CONTENT_TOKENS: u64 = 64;

/// Marker left in place of a tool result whose body was dropped. Mirrors
/// `ObservationMaskingStrategy`'s wording so a transcript reads the same
/// whichever path elided it.
fn elided_marker(original_len: usize) -> String {
    format!("[output elided — {original_len} chars]")
}

/// Shrink `content` to at most `target_tokens`, keeping a leading run of whole
/// tokens and recording how much was cut. Token-native: it encodes `content`,
/// keeps the leading token ids that fit the budget (minus the marker's cost),
/// and decodes them back to a `String` — so the result is never larger than
/// `target_tokens` and always ends on a token boundary. Returns an empty string
/// when not even the marker fits — the message itself still stays in the
/// transcript, so provider validity is unaffected.
fn truncate_to_tokens(content: &str, target_tokens: u64) -> String {
    let tokens = bpe().encode_with_special_tokens(content);
    if tokens.len() as u64 <= target_tokens {
        return content.to_string();
    }
    let marker = format!("\n[…truncated — {} chars total]", content.len());
    let marker_tokens = bpe().encode_with_special_tokens(&marker).len() as u64;
    if marker_tokens >= target_tokens {
        // The allowance cannot even hold the marker. Emit the marker alone: it
        // records what was dropped and keeps the block non-empty, which the
        // provider requires.
        return marker;
    }
    let keep = (target_tokens - marker_tokens) as usize;
    // Decoding whole token ids yields valid UTF-8; on the (not expected)
    // failure path, emit the marker alone rather than a partial invalid block.
    let mut out = bpe().decode(&tokens[..keep]).unwrap_or_default();
    out.push_str(&marker);
    out
}

/// Hard-bound an already-selected window so it cannot exceed `token_limit`.
///
/// `kept_window_start` guarantees the window is never system-only and never
/// LEADS with an orphaned tool result, but says nothing about SIZE: when the
/// tail after the last user message is itself larger than the budget, it
/// returns that tail whole. In production that produced a 2,870,192-token
/// prompt against a 1,000,000 cap. The provider rejects that non-retryably, so
/// every later turn rebuilt the same oversized prompt and the conversation
/// wedged permanently.
///
/// Four passes, cheapest reclamation first:
///   0. leading normalization — drop non-User messages from the front (after
///      system) until the window begins with a user turn, the shape providers
///      require. Runs even when the window is within budget: the backward walk
///      can select an assistant-first window on its own.
///   1. tool results are elided oldest-first (they dominate context), sparing
///      the most recent one — the model usually needs it verbatim to act;
///   2. remaining oversized content is truncated oldest-first to a prefix plus
///      a marker, so the newest turns keep their detail longest;
///   3. if the window STILL exceeds the budget (many small messages, each
///      under the floor; irreducible provider blocks), whole messages are
///      dropped oldest-first, keeping the system message and at least the
///      final message, then the front is re-normalized.
///
/// `provider_content` is never rewritten, so thinking blocks round-trip
/// unchanged. Passes 1–2 never remove a message; pass 3 removes whole
/// messages only, so a `tool_use` and its `tool_result` are either both kept
/// or the orphaned result is dropped by re-normalization — never a dangling
/// half.
///
/// Guarantee: the result is bounded by `token_limit` plus an irreducible
/// residual — the system message and the final message's floor/provider
/// blocks. It is never proportional to transcript length or tool-output
/// volume, the terms that actually run away.
fn enforce_token_budget(messages: &mut Vec<Message>, token_limit: u64) {
    normalize_leading(messages);

    let mut costs: Vec<u64> = messages.iter().map(message_tokens).collect();
    let mut total: u64 = costs.iter().sum();
    if total <= token_limit {
        return;
    }

    // Pass 1: elide tool results oldest-first, but never the most recent one.
    // If that last result is itself oversized, pass 2 still bounds it —
    // truncation keeps a prefix instead of destroying the message.
    let last_tool = messages.iter().rposition(|m| m.role == Role::Tool);
    for i in 0..messages.len() {
        if total <= token_limit {
            return;
        }
        if messages[i].role != Role::Tool
            || Some(i) == last_tool
            || count_tokens(&messages[i].content) <= MIN_KEPT_CONTENT_TOKENS
        {
            continue;
        }
        let marker = elided_marker(messages[i].content.len());
        if marker.len() < messages[i].content.len() {
            messages[i].content = marker;
            let new_cost = message_tokens(&messages[i]);
            total = total - costs[i] + new_cost;
            costs[i] = new_cost;
        }
    }

    // Pass 2: truncate oversized content oldest-first.
    for i in 0..messages.len() {
        if total <= token_limit {
            return;
        }
        if messages[i].role == Role::System {
            continue;
        }
        // Below the floor there is nothing worth reclaiming, and cutting a
        // short turn to nothing would emit an empty content block — which the
        // provider rejects, trading one 400 for another.
        if count_tokens(&messages[i].content) <= MIN_KEPT_CONTENT_TOKENS {
            continue;
        }
        // Give this message whatever slack the rest of the window leaves for
        // its CONTENT specifically: its own immutable share (tool arguments,
        // provider blocks) cannot be reclaimed and must be budgeted around,
        // not granted to the content and then re-added on top.
        let others = total.saturating_sub(costs[i]);
        let allowance =
            token_limit.saturating_sub(others.saturating_add(immutable_tokens(&messages[i])));
        let shrunk = truncate_to_tokens(&messages[i].content, allowance);
        if !shrunk.is_empty() && shrunk.len() < messages[i].content.len() {
            messages[i].content = shrunk;
            let new_cost = message_tokens(&messages[i]);
            total = total - costs[i] + new_cost;
            costs[i] = new_cost;
        }
    }

    // Pass 3: content shrinking was not enough — the overflow is message
    // COUNT (each under the floor) or irreducible provider blocks. Drop whole
    // BLOCKS oldest-first. A block is one message, except an assistant carrying
    // tool_calls, which takes its contiguous tool results with it — dropping
    // half of that pair would leave a dangling tool_use or an orphaned
    // tool_result, both provider-invalid. The system message, the block holding
    // the last user turn (the transcript's anchor), and the final block are
    // never dropped; if only those remain, the residual is accepted — after
    // passes 1–2 it is a handful of floor-sized messages, not the
    // transcript-proportional overflow this pass exists to stop.
    if total > token_limit {
        let offset = usize::from(messages[0].role == Role::System);
        let last_user = messages.iter().rposition(|m| m.role == Role::User);

        // Block start indices, oldest-first.
        let mut blocks: Vec<(usize, usize)> = Vec::new(); // (start, end_exclusive)
        let mut i = offset;
        while i < messages.len() {
            let mut end = i + 1;
            if !messages[i].tool_calls.is_empty() {
                while end < messages.len() && messages[end].role == Role::Tool {
                    end += 1;
                }
            }
            blocks.push((i, end));
            i = end;
        }

        let mut dropped = vec![false; messages.len()];
        let final_block_start = blocks.last().map(|b| b.0);
        for &(start, end) in &blocks {
            if total <= token_limit {
                break;
            }
            if Some(start) == final_block_start {
                break; // never drop the final block
            }
            if let Some(u) = last_user {
                if (start..end).contains(&u) {
                    continue; // never drop the last user turn
                }
            }
            for j in start..end {
                dropped[j] = true;
                total -= costs[j];
            }
        }
        if dropped.iter().any(|&d| d) {
            let mut keep = dropped.iter().map(|&d| !d);
            messages.retain(|_| keep.next().unwrap());
            normalize_leading(messages);
        }
    }
}

/// Drop non-User messages from the front of the window (after any system
/// message) until the first conversational message is a user turn — the shape
/// providers require — as long as a later user turn exists to anchor on. A
/// window with no user message at all is left as-is rather than emptied.
fn normalize_leading(messages: &mut Vec<Message>) {
    let offset = usize::from(!messages.is_empty() && messages[0].role == Role::System);
    let Some(rel_user) = messages[offset..].iter().position(|m| m.role == Role::User) else {
        return;
    };
    if rel_user > 0 {
        messages.drain(offset..offset + rel_user);
    }
}

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
        // Budget = 2 tokens: "bye" fits, then "hi" — but an assistant-first
        // window is provider-invalid (the first conversational message must be
        // a user turn), so normalization drops "hi" too. This test previously
        // enshrined the assistant-first shape.
        let result = strategy.compact(&messages, 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "bye");
        assert_eq!(result[0].role, Role::User);
    }

    /// Review finding: enough small messages — each under the truncation floor
    /// — still overflowed the budget in proportion to transcript length,
    /// recreating the permanent-wedge failure mode without any single
    /// oversized message. The drop pass must bound this.
    #[test]
    fn budget_holds_against_many_small_messages() {
        let strategy = SlidingWindowStrategy::new();
        let mut messages = vec![msg(Role::System, "sys"), msg(Role::User, "go")];
        for _ in 0..10_000 {
            messages.push(msg(Role::Assistant, &"x".repeat(256)));
        }
        let limit = 1_000;
        let result = strategy.compact(&messages, limit);
        assert!(
            total_tokens(&result) <= limit,
            "count-proportional overflow must be bounded, got {} > {}",
            total_tokens(&result),
            limit
        );
        let non_system: Vec<&Message> = result.iter().filter(|m| m.role != Role::System).collect();
        assert!(!non_system.is_empty());
        assert_eq!(non_system[0].role, Role::User);
    }

    /// Review finding: the truncation allowance granted a message's whole
    /// slack to its content and then re-added its immutable share (tool
    /// arguments) on top, roughly doubling the budget. The allowance must be
    /// computed net of the immutable share.
    #[test]
    fn truncation_allowance_accounts_for_immutable_tool_arguments() {
        // cl100k: content "y"*4000 = 1000 tokens (shrinkable); the small
        // tool-call argument + id + name = ~9 tokens (immutable). Budget 100
        // forces content truncation; the allowance must be computed NET of the
        // immutable share, or the result lands ~immutable over budget.
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: "y".repeat(4_000),
                tool_calls: vec![ToolCallMessage {
                    id: "call_1".into(),
                    name: "exec".into(),
                    arguments: serde_json::json!({"cmd": "ls"}),
                }],
                tool_call_id: None,
                provider_content: vec![],
            },
        ];
        let limit = 100;
        let result = strategy.compact(&messages, limit);
        assert!(
            total_tokens(&result) <= limit,
            "immutable tool arguments must be budgeted, not granted twice: {} > {}",
            total_tokens(&result),
            limit
        );
        // The tool call itself must survive untouched.
        let assistant = result.iter().find(|m| !m.tool_calls.is_empty()).unwrap();
        assert_eq!(
            assistant.tool_calls[0].arguments["cmd"]
                .as_str()
                .unwrap(),
            "ls"
        );
        // And the content really was truncated (the whole point of the pass).
        assert!(
            assistant.content.len() < 4_000,
            "over-budget content must be truncated, got {} bytes",
            assistant.content.len()
        );
    }

    /// Review finding: dropping whole messages must not orphan a tool result
    /// from its tool_use — the pair moves as one block.
    #[test]
    fn drop_pass_never_orphans_tool_results() {
        let strategy = SlidingWindowStrategy::new();
        let mut messages = vec![msg(Role::System, "sys"), msg(Role::User, "go")];
        // Many small tool-use blocks, then a final block.
        for k in 0..2_000 {
            messages.push(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: format!("call_{k}"),
                    name: "exec".into(),
                    arguments: serde_json::json!({"n": k}),
                }],
                tool_call_id: None,
                provider_content: vec![],
            });
            messages.push(Message {
                role: Role::Tool,
                content: "ok".repeat(64),
                tool_calls: vec![],
                tool_call_id: Some(format!("call_{k}")),
                provider_content: vec![],
            });
        }
        let limit = 2_000;
        let result = strategy.compact(&messages, limit);
        assert!(total_tokens(&result) <= limit);
        // Every kept tool result must be preceded (somewhere) by the assistant
        // message carrying its tool_use id.
        for m in result.iter().filter(|m| m.role == Role::Tool) {
            let id = m.tool_call_id.as_deref().unwrap();
            assert!(
                result
                    .iter()
                    .any(|a| a.tool_calls.iter().any(|c| c.id == id)),
                "tool result {id} kept without its tool_use"
            );
        }
        // And no dangling tool_use either: every kept tool_use has its result.
        for a in result.iter().filter(|m| !m.tool_calls.is_empty()) {
            for c in &a.tool_calls {
                assert!(
                    result
                        .iter()
                        .any(|t| t.tool_call_id.as_deref() == Some(c.id.as_str())),
                    "tool_use {} kept without its result",
                    c.id
                );
            }
        }
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

    // --- X6: Estimation math — the budget walk tracks real BPE token counts ---
    //
    // These pin the WINDOW SELECTION against cl100k_base counts, not a char
    // ratio. cl100k: "hello world" = 2 tokens; the pangram below = 9.

    #[test]
    fn estimator_counts_real_bpe_tokens() {
        // Discriminating window: the recent message costs the SAME under both
        // estimators ("ok go now" = 3 cl100k = 3 bytes/4), but the older one
        // DIVERGES ("a b c d e" = 5 cl100k vs 3 bytes/4). Budget 6 keeps the
        // older message under bytes/4 (3+3=6) but excludes it under cl100k
        // (3+5=8 > 6). So len==1 passes ONLY with the real BPE estimator.
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "a b c d e"), // older: cl100k 5, bytes/4 3
            msg(Role::User, "ok go now"), // recent: cl100k 3, bytes/4 3
        ];
        let result = strategy.compact(&messages, 6);
        assert_eq!(
            result.len(),
            1,
            "older message (5 real tokens) must be excluded at budget 6; bytes/4 (3) would wrongly keep it"
        );
        assert_eq!(result[0].content, "ok go now");
    }

    #[test]
    fn estimate_tokens_keeps_last_when_over_budget() {
        // A message costing more than the budget is KEPT (never dropped below
        // the most-recent message), so over-budget is kept, not emptied. Uses a
        // real 2-token message against a 1-token budget.
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![msg(Role::User, "hello world")]; // 2 tokens
        let result = strategy.compact(&messages, 1);         // budget 1 < 2
        assert_eq!(
            result.len(),
            1,
            "never strip below the most-recent message — over budget is kept, not dropped to empty"
        );
        let result = strategy.compact(&messages, 0);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn estimate_tokens_window_selects_by_token_cost() {
        // cl100k: pangram = 9 tokens, "hello world" = 2 tokens. Budget 9 keeps
        // the recent pangram and excludes the older "hello world" (9+2 > 9).
        let pangram = "the quick brown fox jumps over the lazy dog";
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "hello world"), // older, 2 tokens
            msg(Role::User, pangram),       // recent, 9 tokens
        ];
        let result = strategy.compact(&messages, 9);
        assert_eq!(result.len(), 1, "only the 9-token message fits");
        assert_eq!(result[0].content, pangram);

        // Budget 11: both fit (2 + 9 = 11).
        let result = strategy.compact(&messages, 11);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn estimate_tokens_single_char() {
        // "x" is one cl100k token.
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![msg(Role::User, "x")];
        let result = strategy.compact(&messages, 1);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn estimate_tokens_empty_content() {
        // Empty content encodes to 0 tokens, so it fits a 0 budget.
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![msg(Role::User, "")];
        let result = strategy.compact(&messages, 0);
        assert_eq!(result.len(), 1); // 0 tokens fits in 0 budget
    }

    /// The regression-pinning test for the estimator swap: `message_tokens` must
    /// return the real cl100k count, NOT the old bytes/4 stub. This 72-byte
    /// sentence is 15 cl100k tokens but 18 under bytes/4, so asserting 15 fails
    /// if the stub is restored.
    #[test]
    fn message_tokens_is_cl100k_not_bytes_over_four() {
        let sentence =
            "The quick brown fox jumps over the lazy dog and runs through the forest.";
        assert_eq!(sentence.len(), 72, "test premise: byte length changed");
        // bytes/4 would be 18; cl100k is 15. Pin the real tokenizer's count.
        assert_eq!(
            message_tokens(&msg(Role::User, sentence)),
            15,
            "message_tokens must be the cl100k count (15), not bytes/4 (18)"
        );
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
        // cl100k costs: sys=1, msgN=2 each. Budget 7 keeps system + the three
        // most recent (2+2+2=6) but not msg1 (6+2=8 > 7).
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::System, "sys"),     // 1 token
            msg(Role::User, "msg1"),      // 2 tokens
            msg(Role::Assistant, "msg2"), // 2 tokens
            msg(Role::User, "msg3"),      // 2 tokens
            msg(Role::Assistant, "msg4"), // 2 tokens
            msg(Role::User, "msg5"),      // 2 tokens
        ];
        let result = strategy.compact(&messages, 7);
        assert_eq!(result.len(), 4, "expected system + 3 recent messages");
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[0].content, "sys");
        assert_eq!(result[1].content, "msg3");
        assert_eq!(result[2].content, "msg4");
        assert_eq!(result[3].content, "msg5");
    }

    #[test]
    fn sliding_window_preserves_chronological_order() {
        // cl100k: "first"/"second"/"third" are 1 token each. Budget 2 keeps
        // "third"+"second" (1+1) and excludes "first".
        let strategy = SlidingWindowStrategy::new();
        let messages = vec![
            msg(Role::User, "first"),  // 1 token
            msg(Role::User, "second"), // 1 token
            msg(Role::User, "third"),  // 1 token
        ];
        let result = strategy.compact(&messages, 2);
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

    /// Total estimated tokens of a compacted window, using the SAME estimator
    /// as production (`window_tokens`) so the assertion cannot pass while the
    /// real accounting is over budget.
    fn total_tokens(msgs: &[Message]) -> u64 {
        window_tokens(msgs)
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
