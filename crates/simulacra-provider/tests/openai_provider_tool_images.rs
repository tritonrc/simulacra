//! Images returned by tools reach an OpenAI-compatible gateway as one
//! `user` message after the tool messages of that turn.

mod support;

use simulacra_provider::{OpenAiConfig, OpenAiProvider, Provider};
use simulacra_types::{Message, ProviderContentBlock, Role, ToolCallMessage};
use support::*;

fn message(role: Role, content: &str) -> Message {
    Message {
        role,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_content: vec![],
    }
}

fn tool_result(id: &str, content: &str, source: Option<serde_json::Value>) -> Message {
    Message {
        tool_call_id: Some(id.into()),
        provider_content: source
            .into_iter()
            .map(|source| ProviderContentBlock {
                provider: "anthropic".into(),
                value: serde_json::json!({"type": "image", "source": source}),
            })
            .collect(),
        ..message(Role::Tool, content)
    }
}

fn call(id: &str) -> ToolCallMessage {
    ToolCallMessage {
        id: id.into(),
        name: "shot".into(),
        arguments: serde_json::json!({}),
    }
}

fn history(first_source: Option<serde_json::Value>) -> Vec<Message> {
    vec![
        message(Role::User, "look"),
        Message {
            tool_calls: vec![call("call_a"), call("call_b")],
            ..message(Role::Assistant, "")
        },
        tool_result("call_a", "first", first_source),
        tool_result("call_b", "second", None),
    ]
}

async fn sent_messages(history: &[Message]) -> (Vec<serde_json::Value>, String) {
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "k", "m"));
    provider
        .chat(history, &[], &mut fresh_budget())
        .await
        .expect("fake upstream answers 200");
    let raw = String::from_utf8(fake.first_request().body).unwrap();
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    (body["messages"].as_array().unwrap().clone(), raw)
}

fn roles(messages: &[serde_json::Value]) -> Vec<&str> {
    messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn a_base64_image_follows_all_tool_messages_as_a_data_uri() {
    let source = serde_json::json!({"type":"base64","media_type":"image/png","data":"QUJD"});
    let (messages, _) = sent_messages(&history(Some(source))).await;
    assert_eq!(
        roles(&messages),
        ["user", "assistant", "tool", "tool", "user"]
    );
    assert_eq!(messages[2]["tool_call_id"], "call_a");
    assert_eq!(messages[2]["content"], "first");
    assert_eq!(messages[3]["tool_call_id"], "call_b");
    assert_eq!(
        messages[4]["content"],
        serde_json::json!([
            {"type":"text","text":"Image(s) returned by tool call call_a:"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}},
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_url_image_passes_through() {
    let source = serde_json::json!({"type":"url","url":"https://example.test/a.png"});
    let (messages, _) = sent_messages(&history(Some(source))).await;
    assert_eq!(
        messages[4]["content"][1]["image_url"]["url"],
        "https://example.test/a.png"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_file_id_image_becomes_unavailable_text_and_the_id_is_not_sent() {
    let source = serde_json::json!({"type":"file","file_id":"file_SECRET123"});
    let (messages, raw) = sent_messages(&history(Some(source))).await;
    assert!(!raw.contains("file_SECRET123"));
    assert_eq!(messages[4]["content"][1]["type"], "text");
    assert!(
        messages[4]["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    assert!(!raw.contains("image_url"));
}

#[tokio::test(flavor = "current_thread")]
async fn no_images_adds_no_message() {
    let (messages, _) = sent_messages(&history(None)).await;
    assert_eq!(roles(&messages), ["user", "assistant", "tool", "tool"]);
}

#[tokio::test]
async fn images_join_a_user_message_that_follows_instead_of_doubling_it() {
    let mut turns = history(Some(serde_json::json!({
        "type": "base64", "media_type": "image/png", "data": "QUJD"
    })));
    turns.push(message(Role::User, "what do you see?"));

    let (messages, _) = sent_messages(&turns).await;

    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "tool", "user"]);
    assert_eq!(
        messages[4]["content"],
        serde_json::json!([
            {"type": "text", "text": "Image(s) returned by tool call call_a:"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
            {"type": "text", "text": "what do you see?"},
        ])
    );
}

#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The unsendable-image warning fires and names neither the file id nor
/// any image data.
#[tokio::test(flavor = "current_thread")]
async fn the_unsendable_image_warning_carries_no_id_or_data() {
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let file = serde_json::json!({"type":"file","file_id":"file_SECRET123"});
    sent_messages(&history(Some(file))).await;
    // Base64 with no media type is unsendable while still carrying data.
    let headless = serde_json::json!({"type":"base64","data":"SECRETIMAGEDATA"});
    sent_messages(&history(Some(headless))).await;

    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        text.matches("cannot send").count(),
        2,
        "one warning per unsendable image: {text}"
    );
    assert!(!text.contains("file_SECRET123"), "{text}");
    assert!(!text.contains("SECRETIMAGEDATA"), "{text}");
}
