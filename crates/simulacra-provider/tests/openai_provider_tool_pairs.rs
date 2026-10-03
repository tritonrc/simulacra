//! A persisted history with a late, repeated tool result reaches an
//! OpenAI-compatible gateway as a valid tool sequence.

mod support;

use simulacra_provider::{OpenAiConfig, OpenAiProvider, Provider};
use simulacra_types::{Message, Role, ToolCallMessage};
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

fn tool_result(id: &str, content: &str) -> Message {
    Message {
        tool_call_id: Some(id.into()),
        ..message(Role::Tool, content)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_paused_call_answered_again_on_approval_is_sent_once_and_in_place() {
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json("stop")));
    let provider = OpenAiProvider::with_config(OpenAiConfig::new(fake.base_url(), "k", "m"));
    let held_call = Message {
        tool_calls: vec![ToolCallMessage {
            id: "call_x".into(),
            name: "publish".into(),
            arguments: serde_json::json!({}),
        }],
        ..message(Role::Assistant, "")
    };
    let history = vec![
        message(Role::User, "ship it"),
        held_call,
        tool_result("call_x", "paused for approval"),
        message(Role::Assistant, "waiting on approval"),
        message(Role::User, "approved"),
        tool_result("call_x", "published"),
    ];

    provider
        .chat(&history, &[], &mut fresh_budget())
        .await
        .expect("fake upstream answers 200");

    let body: serde_json::Value = serde_json::from_slice(&fake.first_request().body).unwrap();
    let sent: Vec<(String, String)> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_owned(),
                m["content"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let expected = [
        ("user", "ship it"),
        ("assistant", ""),
        ("tool", "paused for approval"),
        ("assistant", "waiting on approval"),
        ("user", "approved"),
    ];
    assert_eq!(
        sent,
        expected.map(|(r, c)| (r.to_owned(), c.to_owned())),
        "the second result for call_x must not be sent as an orphan tool message"
    );
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_x");
    assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "publish");
    assert_eq!(messages[2]["tool_call_id"], "call_x");
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "tool").count(),
        1,
        "exactly one result answers call_x"
    );
}
