//! Provider-validity of the window a pinned prefix produces: a pinned boundary
//! must not split a tool exchange, a prefix longer than any transcript must not
//! overflow, and pinning must work without a system prompt to offset from.

use super::msg;
use crate::{ContextStrategy, Message, Role, SlidingWindowStrategy};
use simulacra_types::ToolCallMessage;

/// A complete tool exchange with a turn on either side. Pinned counts 0..=4
/// place the boundary at every position in it, including between the assistant
/// that owns the call and the result that answers it.
fn exchange() -> Vec<Message> {
    vec![
        msg(Role::System, "system"),
        msg(Role::User, "q"),
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
            }],
            tool_call_id: None,
            provider_content: vec![],
        },
        Message {
            role: Role::Tool,
            content: "fn main() {}".into(),
            tool_calls: Vec::new(),
            tool_call_id: Some("c1".into()),
            provider_content: vec![],
        },
        msg(Role::User, "next"),
    ]
}

#[test]
fn no_pinned_boundary_splits_a_tool_exchange() {
    let messages = exchange();
    let whole = contents(&messages);
    // Pinned 1 ends the head on the user turn, so normalisation drains the
    // exchange sitting above the boundary. Every other boundary keeps it.
    let drained = vec![
        (&Role::System, "system"),
        (&Role::User, "q"),
        (&Role::User, "next"),
    ];

    for pinned in 0..=4 {
        // A budget far above the fixture's cost, so the pinned prefix is the
        // only thing that can move the window boundary.
        let out = SlidingWindowStrategy::with_pinned_prefix(pinned).compact(&messages, 1_000_000);

        // The pairing loops below iterate zero times over a window stripped of
        // calls and results, so pin what the window actually holds first.
        assert_eq!(
            &contents(&out),
            if pinned == 1 { &drained } else { &whole },
            "pinned {pinned}: unexpected window"
        );

        for result in out.iter().filter(|m| m.role == Role::Tool) {
            let id = result
                .tool_call_id
                .as_deref()
                .expect("fixture tool results carry an id");
            assert!(
                out.iter().any(|m| m.tool_calls.iter().any(|c| c.id == id)),
                "pinned {pinned}: tool result {id} kept without its tool_use, got {:?}",
                contents(&out)
            );
        }
        for owner in out.iter().filter(|m| !m.tool_calls.is_empty()) {
            for call in &owner.tool_calls {
                assert!(
                    out.iter()
                        .any(|m| m.tool_call_id.as_deref() == Some(call.id.as_str())),
                    "pinned {pinned}: tool_use {} kept without its result, got {:?}",
                    call.id,
                    contents(&out)
                );
            }
        }
    }
}

fn contents(msgs: &[Message]) -> Vec<(&Role, &str)> {
    msgs.iter().map(|m| (&m.role, m.content.as_str())).collect()
}

#[test]
fn a_prefix_at_the_numeric_ceiling_pins_everything_after_system() {
    let messages = vec![msg(Role::System, "system"), msg(Role::User, "q")];

    let out = SlidingWindowStrategy::with_pinned_prefix(usize::MAX).compact(&messages, 1_000);

    let kept: Vec<&str> = out.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(kept, vec!["system", "q"]);
}

#[test]
fn a_pinned_leading_message_survives_without_a_system_prompt() {
    // No System at index 0, so the head offset is 0 and the pinned prefix is
    // the only thing holding this turn in place. A pinned prefix must be
    // provider-valid, so it is a user turn and the assistant follows it.
    let messages = vec![
        msg(Role::User, "pinned note"),
        msg(Role::Assistant, "stray"),
        msg(Role::User, "q"),
    ];

    // At 0 the tail scan selects nothing; at a budget that fits everything
    // normalisation drains the assistant above the boundary. Either way the
    // pinned turn leads the window.
    for limit in [0, 1_000_000] {
        let out = SlidingWindowStrategy::with_pinned_prefix(1).compact(&messages, limit);

        assert_eq!(
            contents(&out),
            vec![(&Role::User, "pinned note"), (&Role::User, "q")],
            "limit {limit}"
        );
    }
}

#[test]
fn a_fitting_tail_with_no_user_message_is_not_normalised_away() {
    let messages = vec![
        msg(Role::System, "system"),
        msg(Role::User, "<conversation-state/>"),
        msg(Role::Assistant, "short note"),
    ];

    let out = SlidingWindowStrategy::with_pinned_prefix(1).compact(&messages, 1_000_000);

    assert_eq!(
        contents(&out),
        vec![
            (&Role::System, "system"),
            (&Role::User, "<conversation-state/>"),
            (&Role::Assistant, "short note")
        ]
    );
}
