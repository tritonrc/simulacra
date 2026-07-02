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

    assert_eq!(tool_content(&result), "hello from simulacra");
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
    let message = tool_content(&result);
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
