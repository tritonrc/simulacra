#[test]
fn file_read_with_a_path_that_exists_returns_the_file_content() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/hello.txt", b"hello from simulacra")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_read",
        json!({ "path": "/workspace/hello.txt" }),
        &capability,
    )
    .expect("file_read should succeed");

    assert_eq!(result, json!("hello from simulacra"));
}

#[test]
fn file_read_with_a_path_that_does_not_exist_returns_error_result_with_not_found_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_read",
        json!({ "path": "/workspace/missing.txt" }),
        &capability,
    )
    .expect("file_read should return a user-facing error result");

    assert_error_result_contains(&result, "not found");
}

#[test]
fn file_read_without_path_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "file_read", json!({}), &capability));
}

#[test]
fn file_write_writes_content_and_returns_confirmation_with_byte_count() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/note.txt",
            "content": "abc123"
        }),
        &capability,
    )
    .expect("file_write should succeed");

    assert_eq!(harness.vfs.read("/workspace/note.txt").unwrap(), b"abc123");
    let message = string_result(&result);
    assert!(message.contains("/workspace/note.txt"));
    assert!(message.contains('6'));
}

#[test]
fn file_write_to_a_nested_path_creates_parent_directories() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/deep/tree/file.txt",
            "content": "nested"
        }),
        &capability,
    )
    .expect("file_write should succeed");

    assert!(harness.vfs.exists("/workspace/deep"));
    assert!(harness.vfs.exists("/workspace/deep/tree"));
    assert_eq!(
        harness.vfs.read("/workspace/deep/tree/file.txt").unwrap(),
        b"nested"
    );
}

#[test]
fn file_write_without_content_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(
        &harness,
        "file_write",
        json!({ "path": "/workspace/out.txt" }),
        &capability,
    ));
}

#[test]
fn file_edit_replaces_old_string_with_new_string_in_the_file() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"alpha beta gamma")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "beta",
            "new_string": "delta"
        }),
        &capability,
    )
    .expect("file_edit should succeed");

    assert_eq!(
        harness.vfs.read("/workspace/edit.txt").unwrap(),
        b"alpha delta gamma"
    );
    assert!(!string_result(&result).is_empty());
}

#[test]
fn file_edit_where_old_string_is_not_found_returns_error_result_with_not_found_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"alpha beta gamma")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "omega",
            "new_string": "delta"
        }),
        &capability,
    )
    .expect("file_edit should return a user-facing error result");

    assert_error_result_contains(&result, "not found");
}

#[test]
fn file_edit_where_old_string_appears_more_than_once_returns_error_result_with_ambiguous_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"repeat and repeat again")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "repeat",
            "new_string": "done"
        }),
        &capability,
    )
    .expect("file_edit should return a user-facing error result");

    assert_error_result_contains(&result, "ambiguous");
}

#[test]
fn file_edit_on_a_non_existent_file_returns_tool_result_is_error_true() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/missing.txt",
            "old_string": "old",
            "new_string": "new"
        }),
        &capability,
    )
    .expect("file_edit should return a user-facing error result");

    assert_eq!(result.get("is_error").and_then(Value::as_bool), Some(true));
}
