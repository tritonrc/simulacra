use serde_json::json;

use super::*;

fn ephemeral() -> serde_json::Value {
    json!({ "type": "ephemeral" })
}

#[test]
fn a_string_content_becomes_one_marked_text_block() {
    let mut body = json!({ "messages": [{ "role": "assistant", "content": "done" }] });
    apply(&mut body, &[0]);
    assert_eq!(
        body["messages"][0]["content"],
        json!([{ "type": "text", "text": "done", "cache_control": ephemeral() }])
    );
}

#[test]
fn the_last_cacheable_block_is_marked_and_thinking_is_skipped() {
    let mut body = json!({ "messages": [{ "role": "assistant", "content": [
        { "type": "tool_use", "id": "t", "name": "n", "input": {} },
        { "type": "text", "text": "" },
        { "type": "thinking", "thinking": "x", "signature": "s" }
    ] }] });
    apply(&mut body, &[0]);
    let blocks = &body["messages"][0]["content"];
    assert_eq!(blocks[0]["cache_control"], ephemeral());
    assert!(blocks[1].get("cache_control").is_none());
    assert!(blocks[2].get("cache_control").is_none());
}

#[test]
fn an_empty_string_content_is_left_alone() {
    let mut body = json!({ "messages": [{ "role": "user", "content": "" }] });
    apply(&mut body, &[0]);
    assert_eq!(body["messages"][0]["content"], json!(""));
}

#[test]
fn only_the_last_three_breakpoints_are_kept() {
    let mut body = json!({ "messages": [
        { "role": "user", "content": "a" }, { "role": "user", "content": "b" },
        { "role": "user", "content": "c" }, { "role": "user", "content": "d" }
    ] });
    apply(&mut body, &[0, 1, 2, 3]);
    assert_eq!(body["messages"][0]["content"], json!("a"));
    for index in 1..4 {
        assert_eq!(
            body["messages"][index]["content"][0]["cache_control"],
            ephemeral()
        );
    }
}
