use super::{msg, total_tokens};
use crate::message_tokens;
use crate::{ContextStrategy, Message, Role, SlidingWindowStrategy};
use simulacra_types::ToolCallMessage;

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
        assistant.tool_calls[0].arguments["cmd"].as_str().unwrap(),
        "ls"
    );
    // And the content really was truncated (the whole point of the pass).
    assert!(
        assistant.content.len() < 4_000,
        "over-budget content must be truncated, got {} bytes",
        assistant.content.len()
    );
}

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
    let result = strategy.compact(&messages, 1); // budget 1 < 2
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

#[test]
fn message_tokens_is_cl100k_not_bytes_over_four() {
    let sentence = "The quick brown fox jumps over the lazy dog and runs through the forest.";
    assert_eq!(sentence.len(), 72, "test premise: byte length changed");
    // bytes/4 would be 18; cl100k is 15. Pin the real tokenizer's count.
    assert_eq!(
        message_tokens(&msg(Role::User, sentence)),
        15,
        "message_tokens must be the cl100k count (15), not bytes/4 (18)"
    );
}
