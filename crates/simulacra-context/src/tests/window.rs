use super::{msg, tool_msg, total_tokens};
use crate::{ContextStrategy, Message, Role, SlidingWindowStrategy};
use simulacra_types::ToolCallMessage;

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
