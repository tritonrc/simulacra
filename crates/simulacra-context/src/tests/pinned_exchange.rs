//! The pinned boundary against tool exchanges. Wherever the boundary lands in
//! a call/result run, the window it produces must still pair every call with
//! its result — and a prefix of zero must not move the boundary at all.

use super::msg;
use crate::{ContextStrategy, Message, Role, SlidingWindowStrategy};
use simulacra_types::ToolCallMessage;

/// One budget that fits every fixture whole, so the boundary is the only thing
/// shaping the head, and one that fits nothing, so the last-user fallback and
/// the block-drop pass are both in play.
const LIMITS: [u64; 2] = [1_000_000, 0];

fn caller(ids: &[&str]) -> Message {
    Message {
        role: Role::Assistant,
        content: String::new(),
        tool_calls: ids
            .iter()
            .map(|id| ToolCallMessage {
                id: (*id).into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
            })
            .collect(),
        tool_call_id: None,
        provider_content: vec![],
    }
}

fn result(id: &str) -> Message {
    Message {
        role: Role::Tool,
        content: format!("fn main() {{}} // {id}"),
        tool_calls: Vec::new(),
        tool_call_id: Some(id.into()),
        provider_content: vec![],
    }
}

/// One assistant turn carrying two calls, both answered, a turn either side.
fn two_calls() -> Vec<Message> {
    vec![
        msg(Role::System, "system"),
        msg(Role::User, "q"),
        caller(&["c1", "c2"]),
        result("c1"),
        result("c2"),
        msg(Role::User, "next"),
    ]
}

/// The same exchange with the results in the reverse order to the calls, so a
/// boundary rule that pairs by position rather than by role cannot pass.
fn reversed_results() -> Vec<Message> {
    vec![
        msg(Role::System, "system"),
        msg(Role::User, "q"),
        caller(&["c1", "c2"]),
        result("c2"),
        result("c1"),
        msg(Role::User, "next"),
    ]
}

/// Two exchanges back to back, so the boundary can land inside either run.
fn two_exchanges() -> Vec<Message> {
    vec![
        msg(Role::System, "system"),
        msg(Role::User, "q"),
        caller(&["c1", "c2"]),
        result("c1"),
        result("c2"),
        caller(&["c3", "c4"]),
        result("c4"),
        result("c3"),
        msg(Role::User, "next"),
    ]
}

fn fixtures() -> Vec<(&'static str, Vec<Message>)> {
    vec![
        ("two_calls", two_calls()),
        ("reversed_results", reversed_results()),
        ("two_exchanges", two_exchanges()),
    ]
}

type Shape<'a> = (&'a Role, &'a str, Vec<&'a str>, Option<&'a str>);

fn shape(msgs: &[Message]) -> Vec<Shape<'_>> {
    msgs.iter()
        .map(|m| {
            (
                &m.role,
                m.content.as_str(),
                m.tool_calls.iter().map(|c| c.id.as_str()).collect(),
                m.tool_call_id.as_deref(),
            )
        })
        .collect()
}

/// The pairing invariant in both directions: no result without the call that
/// owns it, no call without the result that answers it.
fn assert_paired(out: &[Message], label: &str) {
    for kept in out.iter().filter(|m| m.role == Role::Tool) {
        let id = kept
            .tool_call_id
            .as_deref()
            .expect("fixture tool results carry an id");
        assert!(
            out.iter().any(|m| m.tool_calls.iter().any(|c| c.id == id)),
            "{label}: result {id} kept without its call, got {:?}",
            shape(out)
        );
    }
    for owner in out.iter().filter(|m| !m.tool_calls.is_empty()) {
        for call in &owner.tool_calls {
            assert!(
                out.iter()
                    .any(|m| m.tool_call_id.as_deref() == Some(call.id.as_str())),
                "{label}: call {} kept without its result, got {:?}",
                call.id,
                shape(out)
            );
        }
    }
}

/// Every kept message is an input message, in input order and un-rewritten —
/// so a window cannot satisfy the pairing loops by reordering an owner and its
/// result, or by inventing a message.
fn assert_ordered_subsequence(out: &[Message], input: &[Message], label: &str) {
    let input_shapes = shape(input);
    let mut cursor = 0;
    for kept in shape(out) {
        let Some(at) = input_shapes[cursor..].iter().position(|s| *s == kept) else {
            panic!(
                "{label}: {kept:?} is not the next surviving input message, got {:?}",
                shape(out)
            );
        };
        cursor += at + 1;
    }
}

/// The pairing loops iterate zero times over a window with no calls and no
/// results, so they need something forcing the exchanges to be there. A head
/// of `head_end` messages reaches every exchange whose owner sits below it, and
/// no pass may evict the head — so each of those exchanges must arrive whole
/// whatever the budget.
fn assert_pinned_exchanges_present(out: &[Message], input: &[Message], head_end: usize, l: &str) {
    let kept = shape(out);
    for (owner, _) in input.iter().enumerate().filter(|(i, m)| {
        // Enumerate the owners the head covers, then walk each one's result run.
        *i < head_end && !m.tool_calls.is_empty()
    }) {
        let mut end = owner + 1;
        while end < input.len() && input[end].role == Role::Tool {
            end += 1;
        }
        for expected in shape(&input[owner..end]) {
            assert!(
                kept.contains(&expected),
                "{l}: the head covers index {owner}, so {expected:?} must survive, got {kept:?}"
            );
        }
    }
}

#[test]
fn no_boundary_splits_a_tool_exchange() {
    for (name, messages) in fixtures() {
        // Every fixture leads with System, so the head spans `1 + pinned`
        // messages before any tool-result extension widens it.
        for limit in LIMITS {
            for pinned in 0..=messages.len() {
                let label = format!("{name} pinned {pinned} limit {limit}");
                let out =
                    SlidingWindowStrategy::with_pinned_prefix(pinned).compact(&messages, limit);

                assert_paired(&out, &label);
                assert_ordered_subsequence(&out, &messages, &label);
                assert_pinned_exchanges_present(&out, &messages, 1 + pinned, &label);
                assert_eq!(
                    shape(&out).first(),
                    shape(&messages).first(),
                    "{label}: the system prompt must lead the window"
                );
                assert_eq!(
                    shape(&out).last(),
                    shape(&messages).last(),
                    "{label}: the latest user turn must anchor the window"
                );
            }
        }
    }
}

#[test]
fn a_prefix_of_zero_ignores_tool_calls_on_the_system_message() {
    let mut with_calls = vec![
        msg(Role::System, "system"),
        result("c1"),
        msg(Role::User, "q"),
    ];
    with_calls[0].tool_calls = caller(&["c1"]).tool_calls;
    let plain = vec![
        msg(Role::System, "system"),
        result("c1"),
        msg(Role::User, "q"),
    ];

    let strategy = SlidingWindowStrategy::with_pinned_prefix(0);
    let out = strategy.compact(&with_calls, 0);
    let control = strategy.compact(&plain, 0);

    let roles = |msgs: &[Message]| msgs.iter().map(|m| m.role.clone()).collect::<Vec<_>>();
    assert_eq!(
        roles(&out),
        vec![Role::System, Role::User],
        "with no pinned prefix the head is the system message alone, got {:?}",
        shape(&out)
    );
    assert_eq!(
        roles(&out),
        roles(&control),
        "tool_calls on the system message must not move a boundary that is not pinned"
    );
}

/// The same two exchanges with a user turn between them, so the second one can
/// outlive the boundary on a generous budget and be evicted on a tight one.
fn exchanges_split_by_a_user_turn() -> Vec<Message> {
    vec![
        msg(Role::System, "system"),
        msg(Role::User, "q"),
        caller(&["c1", "c2"]),
        result("c1"),
        result("c2"),
        msg(Role::User, "mid"),
        caller(&["c3", "c4"]),
        result("c4"),
        result("c3"),
        msg(Role::User, "next"),
    ]
}

/// The sweep above accepts any extra complete exchange in the protected
/// region, so an over-reaching boundary satisfies it; only an exact expected
/// window turns pinning the next exchange into a failure. A prefix of 2 puts
/// the boundary on the first exchange's owner and 3 puts it inside that
/// exchange's result run; both must protect indices 0..5 and nothing beyond.
fn assert_exact_window(messages: &[Message], limit: u64, expected: &[usize]) {
    for pinned in [2, 3] {
        let out = SlidingWindowStrategy::with_pinned_prefix(pinned).compact(messages, limit);
        let want: Vec<Message> = expected.iter().map(|i| messages[*i].clone()).collect();
        assert_eq!(
            shape(&out),
            shape(&want),
            "pinned {pinned} limit {limit}: the protected region is the first exchange alone"
        );
    }
}

#[test]
fn a_boundary_in_the_first_exchange_does_not_pin_the_second() {
    let messages = two_exchanges();
    // The second exchange leads with its assistant owner, so once it is outside
    // the head the leading normalisation re-anchors the tail on "next" — on any
    // budget. Pinning it instead would keep indices 5..8 here.
    for limit in LIMITS {
        assert_exact_window(&messages, limit, &[0, 1, 2, 3, 4, 8]);
    }
}

#[test]
fn an_unpinned_exchange_is_governed_by_the_budget() {
    let messages = exchanges_split_by_a_user_turn();
    let whole: Vec<usize> = (0..messages.len()).collect();

    assert_exact_window(&messages, 1_000_000, &whole);
    assert_exact_window(&messages, 0, &[0, 1, 2, 3, 4, 9]);
}
