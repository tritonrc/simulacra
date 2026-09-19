use super::msg;
use crate::{ContextStrategy, Message, Role, SlidingWindowStrategy};
use simulacra_types::ToolCallMessage;

/// The shape every hydrated turn arrives in: the system prompt, then the two
/// synthetic frames the engine rebuilds each turn and pins behind it.
fn head() -> Vec<Message> {
    vec![
        msg(Role::System, "system"),
        msg(Role::User, "<conversation-state/>"),
        msg(Role::User, "<history-window total=\"9\" hidden=\"4\">"),
    ]
}

#[test]
fn pinned_messages_survive_when_nothing_of_the_rest_fits_and_the_last_user_is_restored() {
    let mut messages = head();
    messages.push(msg(Role::Assistant, &"a".repeat(4_000)));
    messages.push(msg(
        Role::User,
        &format!("latest question {}", "q".repeat(2_000)),
    ));

    // 100 tokens buys the head (16) plus a usable prefix of the final user
    // turn; the 500-token assistant still cannot fit, so the tail scan selects
    // nothing and the pinned pair is the only reason the frames are here.
    let out = SlidingWindowStrategy::with_pinned_prefix(2).compact(&messages, 100);

    let contents: Vec<&str> = out.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents.len(), 4, "got {contents:?}");
    assert_eq!(contents[0], "system");
    assert_eq!(contents[1], "<conversation-state/>");
    assert_eq!(contents[2], "<history-window total=\"9\" hidden=\"4\">");
    assert!(
        contents[3].starts_with("latest question"),
        "the last user turn must be restored, got {:?}",
        contents[3]
    );
}

#[test]
fn a_pinned_assistant_directly_after_system_survives_normalisation() {
    let messages = vec![
        msg(Role::System, "system"),
        msg(Role::Assistant, "pinned note"),
        msg(Role::User, "q"),
        msg(Role::Assistant, "a"),
    ];

    let out = SlidingWindowStrategy::with_pinned_prefix(1).compact(&messages, 1_000_000);

    assert_eq!(
        out[1].content, "pinned note",
        "leading normalisation must not evict a pinned non-user message"
    );
    assert_eq!(out.len(), 4);
}

#[test]
fn the_block_drop_pass_removes_middle_blocks_and_never_a_pinned_one() {
    let mut messages = head();
    for i in 0..5 {
        messages.push(msg(Role::User, &format!("u{i} {}", "x".repeat(400))));
        messages.push(msg(Role::Assistant, &format!("a{i} {}", "x".repeat(400))));
    }

    // Every conversational message here costs 54 tokens — under
    // MIN_KEPT_CONTENT_TOKENS, so tool elision and truncation can reclaim
    // nothing and the block-drop pass is the only pass that can act. The
    // budget pays for the system message and the final exchange exactly, so
    // the pinned frames are pure overflow: the pass is entered, reaches them,
    // and must refuse to drop them.
    let system_cost = crate::message_tokens(&messages[0]);
    let last_block: u64 = messages[messages.len() - 2..]
        .iter()
        .map(crate::message_tokens)
        .sum();
    let out =
        SlidingWindowStrategy::with_pinned_prefix(2).compact(&messages, system_cost + last_block);

    let contents: Vec<&str> = out.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents[0], "system");
    assert_eq!(contents[1], "<conversation-state/>");
    assert_eq!(contents[2], "<history-window total=\"9\" hidden=\"4\">");
    assert!(
        contents.last().unwrap().starts_with("a4"),
        "the final block must survive, got {:?}",
        contents.last()
    );
    assert!(
        !contents.iter().any(|c| c.starts_with("u1")),
        "a middle block must still be droppable, got {contents:?}"
    );
}

#[test]
fn a_pinned_synthetic_user_is_not_the_anchor_when_the_rest_has_no_user() {
    let mut messages = head();
    messages.push(msg(Role::Assistant, &"a".repeat(4_000)));

    let out = SlidingWindowStrategy::with_pinned_prefix(2).compact(&messages, 40);

    let contents: Vec<&str> = out.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        contents,
        vec![
            "system",
            "<conversation-state/>",
            "<history-window total=\"9\" hidden=\"4\">",
        ],
        "the pinned frames must not anchor the window and drag the tail in"
    );
}

#[test]
fn a_prefix_longer_than_the_rest_pins_everything_after_system() {
    let messages = vec![
        msg(Role::System, "s"),
        msg(Role::User, "p1"),
        msg(Role::User, "p2"),
    ];

    let out = SlidingWindowStrategy::with_pinned_prefix(10).compact(&messages, 0);
    assert_eq!(out.len(), 3, "got {out:?}");

    assert!(
        SlidingWindowStrategy::with_pinned_prefix(3)
            .compact(&[], 100)
            .is_empty()
    );
}

#[test]
fn the_reproduction_keeps_both_prefixes_and_the_latest_user() {
    let mut messages = head();
    messages.push(msg(Role::User, "DISTINCTIVE_FACT: crimson walnut 7391"));
    for n in 0..146 {
        messages.push(Message {
            role: Role::Assistant,
            content: "checking".into(),
            tool_calls: vec![ToolCallMessage {
                id: format!("call-{n}"),
                name: "example".into(),
                arguments: Default::default(),
            }],
            tool_call_id: None,
            provider_content: vec![],
        });
        messages.push(Message {
            role: Role::Tool,
            content: "token ".repeat(7_000),
            tool_calls: Vec::new(),
            tool_call_id: Some(format!("call-{n}")),
            provider_content: vec![],
        });
    }
    messages.push(msg(Role::Assistant, "All done."));
    messages.push(msg(Role::User, "What distinctive fact did I tell you?"));

    let out = SlidingWindowStrategy::with_pinned_prefix(2).compact(&messages, 800_000);

    assert_eq!(out[1].content, "<conversation-state/>");
    assert_eq!(out[2].content, "<history-window total=\"9\" hidden=\"4\">");
    assert_eq!(
        out.last().unwrap().content,
        "What distinctive fact did I tell you?"
    );
}
