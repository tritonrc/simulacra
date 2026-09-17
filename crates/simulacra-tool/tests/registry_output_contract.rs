//! `ToolRegistry::call_output` converts a tool's raw value into `ToolOutput`.
//! These tests are deliberately outside `builtin_tools.rs`, which is gated on
//! the `sandbox` feature: the conversion is core registry behaviour and has to
//! run under a plain `cargo test -p simulacra-tool`.

use serde_json::{Value, json};
use simulacra_tool::{CapabilityToken, ToolRegistry};
use simulacra_types::{Tool, ToolDefinition, ToolError};
use std::future::Future;

/// Returns its arguments verbatim, so the value the registry converts is the
/// value the test wrote.
struct RawEchoTool;

impl Tool for RawEchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "raw_echo".into(),
            description: "Echo arguments".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        arguments: Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move { Ok(arguments) })
    }
}

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(future)
}

fn registry_with_raw_echo() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(RawEchoTool))
        .expect("test tool registration should succeed");
    registry
}

#[test]
fn call_output_preserves_provider_blocks_from_raw_tool_value() {
    let registry = registry_with_raw_echo();

    let output = run_async(registry.call_output(
        "raw_echo",
        json!({
            "content": "look at this",
            "is_error": false,
            "log_preview": "look at this",
            "provider_content": [
                {
                    "provider": "anthropic",
                    "value": {
                        "type": "image",
                        "source": { "type": "url", "url": "https://example.test/one.png" }
                    }
                },
                {
                    "provider": "other-provider",
                    "value": {
                        "kind": "opaque",
                        "nested": { "keep": [1, 2, 3] }
                    }
                }
            ]
        }),
        &CapabilityToken::default(),
    ))
    .expect("echoed typed output should convert through call_output");

    assert_eq!(output.content, "look at this");
    assert!(!output.is_error);
    assert_eq!(
        output.provider_content,
        vec![
            simulacra_types::ProviderContentBlock {
                provider: "anthropic".into(),
                value: json!({
                    "type": "image",
                    "source": { "type": "url", "url": "https://example.test/one.png" }
                }),
            },
            simulacra_types::ProviderContentBlock {
                provider: "other-provider".into(),
                value: json!({
                    "kind": "opaque",
                    "nested": { "keep": [1, 2, 3] }
                }),
            },
        ]
    );
}

#[test]
fn call_output_leaves_provider_blocks_empty_when_the_raw_value_has_none() {
    let registry = registry_with_raw_echo();

    let output = run_async(registry.call_output(
        "raw_echo",
        json!({
            "content": "plain text",
            "is_error": false,
            "log_preview": "plain text"
        }),
        &CapabilityToken::default(),
    ))
    .expect("echoed typed output should convert through call_output");

    assert_eq!(output.content, "plain text");
    assert_eq!(
        output.provider_content,
        Vec::<simulacra_types::ProviderContentBlock>::new()
    );
}
