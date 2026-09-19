//! Sliding-window selection with an explicitly empty pinned prefix, so a prefix
//! of zero is pinned to today's behaviour rather than to whatever `new()`
//! happens to do. Most tests here restate their counterpart's expected output
//! as a literal. Four do not: `keeps_system_and_recent_*`,
//! `sliding_window_system_exceeds_budget_still_preserved_*`,
//! `sliding_window_keeps_a_coherent_block_when_recent_tools_exceed_budget_*`
//! and `sliding_window_keeps_user_turn_when_single_message_exceeds_budget_*`
//! are deliberate mirrors of the originals in `window.rs` and carry the same
//! weaker shape assertions, unchanged on purpose.

use super::{msg, tool_msg};
use crate::{ContextStrategy, Message, Role, SlidingWindowStrategy};
use simulacra_types::ToolCallMessage;

#[test]
fn keeps_system_and_recent_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let messages = vec![
        msg(Role::System, "You are helpful."),
        msg(
            Role::User,
            "old message that is long enough to be dropped eventually",
        ),
        msg(Role::Assistant, "old reply"),
        msg(Role::User, "recent"),
    ];
    let result = strategy.compact(&messages, 8);
    assert!(result[0].role == Role::System);
    assert!(result.last().unwrap().content == "recent");
}

#[test]
fn empty_input_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    assert!(strategy.compact(&[], 100).is_empty());
}

#[test]
fn sliding_window_no_system_message_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let messages = vec![
        msg(Role::User, "hello"),
        msg(Role::Assistant, "hi"),
        msg(Role::User, "bye"),
    ];
    let result = strategy.compact(&messages, 4);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].content, "hello");
    assert_eq!(result[1].content, "hi");
    assert_eq!(result[2].content, "bye");
}

#[test]
fn sliding_window_no_system_message_drops_oldest_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let messages = vec![
        msg(Role::User, "hello"),
        msg(Role::Assistant, "hi"),
        msg(Role::User, "bye"),
    ];
    let result = strategy.compact(&messages, 2);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "bye");
    assert_eq!(result[0].role, Role::User);
}

#[test]
fn sliding_window_system_exceeds_budget_still_preserved_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let long_system = "a]".repeat(100);
    let messages = vec![msg(Role::System, &long_system), msg(Role::User, "hello")];
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

#[test]
fn sliding_window_exact_order_and_count_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let messages = vec![
        msg(Role::System, "sys"),
        msg(Role::User, "msg1"),
        msg(Role::Assistant, "msg2"),
        msg(Role::User, "msg3"),
        msg(Role::Assistant, "msg4"),
        msg(Role::User, "msg5"),
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
fn sliding_window_preserves_chronological_order_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let messages = vec![
        msg(Role::User, "first"),
        msg(Role::User, "second"),
        msg(Role::User, "third"),
    ];
    let result = strategy.compact(&messages, 2);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].content, "second");
    assert_eq!(result[1].content, "third");
}

#[test]
fn sliding_window_keeps_a_coherent_block_when_recent_tools_exceed_budget_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let big = "x".repeat(4000);
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
        },
        tool_msg(&big),
        tool_msg(&big),
        tool_msg(&big),
        tool_msg(&big),
    ];
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
fn sliding_window_keeps_user_turn_when_single_message_exceeds_budget_with_zero_pinned_prefix() {
    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
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
