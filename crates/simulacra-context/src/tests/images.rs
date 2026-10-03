//! An image block costs what a provider bills for an image, not the length
//! of its encoded source.

use serde_json::json;
use simulacra_types::{ProviderContentBlock, ToolCallMessage};

use super::{msg, total_tokens};
use crate::budget::window_tokens;
use crate::{
    ContextStrategy, IMAGE_BLOCK_TOKENS, Message, ObservationMaskingStrategy, Role,
    SlidingWindowStrategy, message_tokens,
};

fn image_result(id: &str, source: serde_json::Value) -> Message {
    Message {
        tool_call_id: Some(id.into()),
        provider_content: vec![ProviderContentBlock {
            provider: "anthropic".into(),
            value: json!({"type": "image", "source": source}),
        }],
        ..msg(Role::Tool, "shown")
    }
}

fn call(id: &str) -> Message {
    Message {
        tool_calls: vec![ToolCallMessage {
            id: id.into(),
            name: "artifact_get".into(),
            arguments: json!({}),
        }],
        ..msg(Role::Assistant, "")
    }
}

/// A 1 MiB screenshot, base64-encoded the way an inline source carries it.
fn big_base64() -> serde_json::Value {
    json!({"type": "base64", "media_type": "image/png", "data": "iVBORw0K".repeat(180_000)})
}

#[test]
fn an_image_costs_the_same_whatever_its_source_carries() {
    let inline = message_tokens(&image_result("c1", big_base64()), IMAGE_BLOCK_TOKENS);
    let by_id = message_tokens(
        &image_result("c1", json!({"type": "file", "file_id": "file_011abc"})),
        IMAGE_BLOCK_TOKENS,
    );
    assert_eq!(inline, by_id);
    assert!(
        (IMAGE_BLOCK_TOKENS..IMAGE_BLOCK_TOKENS + 20).contains(&inline),
        "{inline}"
    );
}

/// A later tool step must not compact away the exchange that showed the
/// image: counted as text, its base64 would dwarf a 200k window.
#[test]
fn a_large_inline_image_survives_compaction_after_another_tool_step() {
    let messages = vec![
        msg(Role::System, "sys"),
        msg(Role::User, "look at the screenshot, then check the build"),
        call("c1"),
        image_result("c1", big_base64()),
        call("c2"),
        Message {
            tool_call_id: Some("c2".into()),
            ..msg(Role::Tool, "build ok")
        },
    ];

    let kept = SlidingWindowStrategy::new().compact(&messages, 200_000);

    assert_eq!(kept.len(), messages.len(), "nothing needed dropping");
    assert!(total_tokens(&kept) < 10_000);
}

/// Six screenshots in a turn reserve at least what a high-resolution Claude
/// model bills for six full-size images (4,784 each), so the window does not
/// overflow.
#[test]
fn several_images_reserve_at_least_a_claude_full_size_image_each() {
    let mut messages = vec![msg(Role::User, "compare these")];
    for i in 0..6 {
        let id = format!("c{i}");
        messages.push(call(&id));
        messages.push(image_result(&id, big_base64()));
    }
    assert!(total_tokens(&messages) >= 6 * 4_784);
}

/// A user turn followed by three image exchanges.
fn image_history() -> Vec<Message> {
    let mut messages = vec![msg(Role::User, "compare these")];
    for i in 0..3 {
        let id = format!("c{i}");
        messages.push(call(&id));
        messages.push(image_result(&id, big_base64()));
    }
    messages
}

/// Three images fit 20k at the default 4,800 each but not at 7,000 each, so a
/// host declaring a costlier model gets the older exchanges compacted away.
#[test]
fn a_host_declared_image_cost_compacts_earlier_than_the_default() {
    let messages = image_history();
    let limit = 20_000;

    let default = SlidingWindowStrategy::new().compact(&messages, limit);
    assert_eq!(
        default.len(),
        messages.len(),
        "default keeps every exchange"
    );

    let costly = SlidingWindowStrategy::new()
        .with_image_tokens(7_000)
        .compact(&messages, limit);
    assert!(costly.len() < messages.len(), "{} kept", costly.len());
    assert!(window_tokens(&costly, 7_000) <= limit);
}

#[test]
fn masking_honours_a_host_declared_image_cost() {
    let messages = image_history();
    let limit = 20_000;

    let default = ObservationMaskingStrategy::new(10).compact(&messages, limit);
    assert_eq!(
        default.len(),
        messages.len(),
        "default keeps every exchange"
    );

    let costly = ObservationMaskingStrategy::new(10)
        .with_image_tokens(7_000)
        .compact(&messages, limit);
    assert!(costly.len() < messages.len(), "{} kept", costly.len());
    assert!(window_tokens(&costly, 7_000) <= limit);
}
