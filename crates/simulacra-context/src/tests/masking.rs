use super::{msg, tool_msg, total_tokens};
use crate::{ContextStrategy, Message, ObservationMaskingStrategy, Role};

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
