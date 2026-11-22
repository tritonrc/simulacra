use super::*;
use crate::ProviderContentBlock;
use serde_json::json;

fn image_block(label: &str) -> ProviderContentBlock {
    ProviderContentBlock {
        provider: "anthropic".into(),
        value: json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": label
            }
        }),
    }
}

#[test]
fn tool_output_value_roundtrip_preserves_provider_blocks_in_order() {
    let output = ToolOutput {
        content: "screenshot ready".into(),
        is_error: false,
        log_preview: "screenshot ready".into(),
        structured: Some(json!({"width": 800})),
        hook_input: Some(json!({"requested": true})),
        hook_output: Some(json!({"allowed": true})),
        provider_content: vec![image_block("first"), image_block("second")],
    };

    let value = output.to_value();

    assert_eq!(
        value["provider_content"],
        json!([
            {
                "provider": "anthropic",
                "value": {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "first"
                    }
                }
            },
            {
                "provider": "anthropic",
                "value": {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "second"
                    }
                }
            }
        ])
    );

    let decoded = ToolOutput::from_value(value);

    assert_eq!(decoded.provider_content, output.provider_content);
    assert_eq!(decoded.content, "screenshot ready");
    assert_eq!(decoded.structured, Some(json!({"width": 800})));
}

#[test]
fn tool_output_constructors_start_with_no_provider_blocks() {
    let none = Vec::<ProviderContentBlock>::new();

    assert_eq!(ToolOutput::success("plain text").provider_content, none);
    assert_eq!(ToolOutput::error("boom").provider_content, none);
    assert_eq!(
        ToolOutput::success("plain text")
            .with_structured(json!({"ok": true}))
            .with_hook_input(json!({"in": 1}))
            .with_hook_output(json!({"out": 2}))
            .with_log_preview("preview")
            .provider_content,
        none
    );
}

#[test]
fn tool_output_empty_provider_blocks_are_omitted_and_default_when_absent() {
    let output = ToolOutput::success("plain text");
    let value = output.to_value();

    assert!(
        value.get("provider_content").is_none(),
        "empty provider blocks must not be serialized"
    );

    let decoded = ToolOutput::from_value(json!({
        "content": "legacy plain text",
        "is_error": false,
        "log_preview": "legacy plain text"
    }));

    assert_eq!(decoded.content, "legacy plain text");
    assert_eq!(decoded.provider_content, Vec::<ProviderContentBlock>::new());
}

/// `from_value` is lenient by contract: a `provider_content` that does not
/// parse as a block array degrades to an empty vector and nothing else
/// moves. The failure this guards against is the whole object falling
/// through to the `Self::success(other.to_string())` arm, which would turn
/// `content` into the serialized object and lose `is_error`.
#[test]
fn from_value_with_unparseable_provider_content_keeps_every_other_field() {
    let baseline = ToolOutput::from_value(json!({
        "content": "look at this",
        "is_error": true,
        "log_preview": "look",
        "structured": {"width": 800}
    }));

    for malformed in [
        json!("not a block array"),
        json!(17),
        json!(["not an object", 3, null]),
    ] {
        let decoded = ToolOutput::from_value(json!({
            "content": "look at this",
            "is_error": true,
            "log_preview": "look",
            "structured": {"width": 800},
            "provider_content": malformed
        }));

        assert_eq!(
            decoded.provider_content,
            Vec::<ProviderContentBlock>::new(),
            "malformed provider_content {malformed} should degrade to no blocks"
        );
        assert_eq!(decoded.content, "look at this");
        assert!(decoded.is_error);
        assert_eq!(decoded.log_preview, "look");
        assert_eq!(decoded.structured, Some(json!({"width": 800})));
        assert_eq!(
            decoded, baseline,
            "malformed provider_content {malformed} should change nothing but the blocks"
        );
    }
}

/// `to_value`/`from_value` are hand-written, but `ToolOutput` also derives
/// `Serialize`/`Deserialize`, and hosts round-trip it that way. The derived
/// path has to agree: empty blocks leave no key, and a payload written
/// before the field existed still deserializes.
#[test]
fn derived_serde_omits_empty_provider_blocks_and_defaults_them_when_absent() {
    let serialized = serde_json::to_value(ToolOutput::success("plain text"))
        .expect("tool output should serialize");

    assert!(
        serialized.get("provider_content").is_none(),
        "empty provider blocks must not be serialized"
    );

    let decoded: ToolOutput = serde_json::from_value(json!({
        "content": "legacy plain text",
        "is_error": false,
        "log_preview": "legacy plain text"
    }))
    .expect("a payload without provider_content must still deserialize");

    assert_eq!(decoded.provider_content, Vec::<ProviderContentBlock>::new());

    let carried = ToolOutput {
        content: "screenshot ready".into(),
        is_error: false,
        log_preview: "screenshot ready".into(),
        structured: None,
        hook_input: None,
        hook_output: None,
        provider_content: vec![image_block("first"), image_block("second")],
    };
    let round_tripped: ToolOutput =
        serde_json::from_value(serde_json::to_value(&carried).expect("should serialize"))
            .expect("should deserialize");

    assert_eq!(round_tripped.provider_content, carried.provider_content);
}
