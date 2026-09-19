//! What the token-budget passes are allowed to do to a pinned prefix: shrink
//! its content, never remove it, and still bound the window it sits in.

use super::{head, msg, total_tokens};
use crate::message_tokens;
use crate::{ContextStrategy, Role, SlidingWindowStrategy};

#[test]
fn an_over_budget_pinned_message_is_truncated_in_place() {
    let body = format!("PINNED_BODY {}", "x".repeat(4_000));
    let messages = vec![
        msg(Role::System, "sys"),
        msg(Role::User, &body),
        msg(Role::User, "latest"),
    ];

    let out = SlidingWindowStrategy::with_pinned_prefix(1).compact(&messages, 200);

    assert_eq!(
        out.len(),
        3,
        "no pass may remove a pinned message, got {:?}",
        out.iter().map(|m| m.content.len()).collect::<Vec<_>>()
    );
    assert_eq!(out[1].role, Role::User);
    assert!(
        out[1].content.starts_with("PINNED_BODY"),
        "the pinned message must still be in position, got {:?}",
        out[1].content
    );
    assert!(
        out[1].content.len() < body.len(),
        "content far above the floor must be truncated even when pinned, got {} of {} bytes",
        out[1].content.len(),
        body.len()
    );
}

#[test]
fn a_pinned_window_is_bounded_by_the_budget_plus_the_protected_set() {
    let mut messages = head();
    messages.push(msg(Role::User, "go"));
    for _ in 0..2_000 {
        messages.push(msg(Role::Assistant, &"x".repeat(256)));
    }

    // The residual is the set no pass may shrink or remove: the three pinned
    // head messages, the block holding the last user turn ("go"), and the final
    // block. Each is bounded by its input cost because no pass grows a message
    // and every one of these sits under MIN_KEPT_CONTENT_TOKENS, so elision and
    // truncation reclaim nothing from them. It cannot be zero — the pinned head
    // is kept unconditionally, so a limit below the protected set's cost (as
    // here, 40) is honoured only up to that set.
    let residual: u64 = messages[..4]
        .iter()
        .chain(std::iter::once(messages.last().unwrap()))
        .map(message_tokens)
        .sum();
    let limit = 40;

    let out = SlidingWindowStrategy::with_pinned_prefix(2).compact(&messages, limit);

    assert!(
        total_tokens(&out) <= limit + residual,
        "a pinned window must stay bounded, got {} > {} (limit {limit} + residual {residual})",
        total_tokens(&out),
        limit + residual
    );
    assert_eq!(out[1].content, "<conversation-state/>");
    assert_eq!(out[2].content, "<history-window total=\"9\" hidden=\"4\">");
}
