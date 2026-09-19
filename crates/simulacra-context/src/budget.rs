//! Token-budget passes: window selection bounds, truncation, and the
//! leading-message normalisation every strategy ends with.

use crate::{Message, Role, bpe, count_tokens, immutable_tokens, message_tokens};

/// After trimming, skip forward past any leading `Role::Tool` messages so we
/// never start the kept window with orphaned tool results (which would produce
/// invalid transcripts for provider APIs).
pub(crate) fn adjust_tool_boundary(msgs: &[Message], start: usize) -> usize {
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
pub(crate) fn kept_window_start(msgs: &[Message], start: usize) -> usize {
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

/// Extend an exclusive head boundary to the end of the tool exchange it lands
/// in. An exchange is an assistant message carrying `tool_calls` plus the
/// contiguous run of `Role::Tool` messages answering it; a boundary inside one
/// leaves either a dangling call or an orphaned result, both provider-invalid.
/// Walks back from the boundary over any results to the assistant that owns
/// them, then forward to the end of that owner's run — by role and position,
/// never by call id, so results answering out of order still travel together.
pub(crate) fn exchange_edge(msgs: &[Message], head_end: usize) -> usize {
    if head_end == 0 || head_end > msgs.len() {
        return head_end;
    }
    let mut owner = head_end - 1;
    while owner > 0 && msgs[owner].role == Role::Tool {
        owner -= 1;
    }
    if msgs[owner].tool_calls.is_empty() {
        return head_end;
    }
    let mut end = head_end;
    while end < msgs.len() && msgs[end].role == Role::Tool {
        end += 1;
    }
    end
}

/// Sum of [`message_tokens`] over a window. Only used by the test-module
/// `total_tokens` helper, so it is compiled only under `cfg(test)` — otherwise
/// `-D warnings` fails the plain build on dead code.
#[cfg(test)]
pub(crate) fn window_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

/// Content at or below this token cost is left alone by the budget pass:
/// shrinking it reclaims nothing and risks emitting an empty (provider-invalid)
/// block. 64 tokens ≈ the old 256-byte floor under the stub estimator, so the
/// "don't bother gutting short turns" threshold is unchanged in spirit.
pub(crate) const MIN_KEPT_CONTENT_TOKENS: u64 = 64;

/// Marker left in place of a tool result whose body was dropped. Mirrors
/// `ObservationMaskingStrategy`'s wording so a transcript reads the same
/// whichever path elided it.
pub(crate) fn elided_marker(original_len: usize) -> String {
    format!("[output elided — {original_len} chars]")
}

/// Shrink `content` to at most `target_tokens`, keeping a leading run of whole
/// tokens and recording how much was cut. Token-native: it encodes `content`,
/// keeps the leading token ids that fit the budget (minus the marker's cost),
/// and decodes them back to a `String`, so the result ends on a token boundary.
/// On the normal path the result costs at most `target_tokens`. The one
/// exception is deliberate: when the truncation marker alone would cost more
/// than `target_tokens`, the marker is emitted whole (bounded, ~a dozen tokens)
/// rather than an empty string, because a content block must stay non-empty for
/// provider validity. That bounded overage is absorbed by the context headroom.
pub(crate) fn truncate_to_tokens(content: &str, target_tokens: u64) -> String {
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
/// `fixed_head` is the count of leading messages the caller has already
/// committed to: the system message plus any pinned prefix. No pass removes
/// them, though content shrinking still applies.
///
/// Four passes, cheapest reclamation first:
///   0. leading normalization — drop non-User messages from the front (after
///      the fixed head) until the window begins with a user turn, the shape
///      providers require. Runs even when the window is within budget: the
///      backward walk can select an assistant-first window on its own.
///   1. tool results are elided oldest-first (they dominate context), sparing
///      the most recent one — the model usually needs it verbatim to act;
///   2. remaining oversized content is truncated oldest-first to a prefix plus
///      a marker, so the newest turns keep their detail longest;
///   3. if the window STILL exceeds the budget (many small messages, each
///      under the floor; irreducible provider blocks), whole messages are
///      dropped oldest-first, keeping the fixed head and at least the final
///      message, then the front is re-normalized.
///
/// `provider_content` is never rewritten, so thinking blocks round-trip
/// unchanged. Passes 1–2 never remove a message; pass 3 removes whole
/// messages only, so a `tool_use` and its `tool_result` are either both kept
/// or the orphaned result is dropped by re-normalization — never a dangling
/// half.
///
/// Guarantee: the result is bounded by `token_limit` plus an irreducible
/// residual — the fixed head and the final message's floor/provider blocks. It
/// is never proportional to transcript length or tool-output volume, the terms
/// that actually run away.
pub(crate) fn enforce_token_budget(
    messages: &mut Vec<Message>,
    token_limit: u64,
    fixed_head: usize,
) {
    normalize_leading(messages, fixed_head);

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
    // tool_result, both provider-invalid. The fixed head, the block holding
    // the last user turn (the transcript's anchor), and the final block are
    // never dropped; if only those remain, the residual is accepted — after
    // passes 1–2 it is a handful of floor-sized messages, not the
    // transcript-proportional overflow this pass exists to stop.
    if total > token_limit {
        let last_user = messages.iter().rposition(|m| m.role == Role::User);

        // Block start indices, oldest-first. Blocks begin after the fixed
        // head, so no committed index can land inside one.
        let mut blocks: Vec<(usize, usize)> = Vec::new(); // (start, end_exclusive)
        let mut i = fixed_head.min(messages.len());
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
            if let Some(u) = last_user
                && (start..end).contains(&u)
            {
                continue; // never drop the last user turn
            }
            for j in start..end {
                dropped[j] = true;
                total -= costs[j];
            }
        }
        if dropped.iter().any(|&d| d) {
            let mut keep = dropped.iter().map(|&d| !d);
            messages.retain(|_| keep.next().unwrap());
            normalize_leading(messages, fixed_head);
        }
    }
}

/// Drop non-User messages from the front of the window (after the `fixed_head`
/// messages the caller committed to) until the first conversational message is
/// a user turn — the shape providers require — as long as a later user turn
/// exists to anchor on. A window with no user message after the head is left
/// as-is rather than emptied.
pub(crate) fn normalize_leading(messages: &mut Vec<Message>, fixed_head: usize) {
    let fixed_head = fixed_head.min(messages.len());
    let Some(rel_user) = messages[fixed_head..]
        .iter()
        .position(|m| m.role == Role::User)
    else {
        return;
    };
    if rel_user > 0 {
        messages.drain(fixed_head..fixed_head + rel_user);
    }
}
