#[test]
fn shell_exec_echo_hello_returns_stdout_stderr_and_exit_code() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello" }),
        &capability,
    )
    .expect("shell_exec should succeed");

    assert_eq!(
        result,
        json!({
            "stdout": "hello\n",
            "stderr": "",
            "exit_code": 0
        })
    );
}

#[test]
fn shell_exec_nonexistent_command_returns_non_zero_exit_code_not_tool_error() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "nonexistent_command" }),
        &capability,
    )
    .expect("shell_exec should return a normal result even for failed commands");

    let exit_code = result
        .get("exit_code")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    assert_ne!(exit_code, 0);
}

#[test]
fn shell_exec_without_command_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "shell_exec", json!({}), &capability));
}

#[test]
fn js_exec_one_plus_one_returns_the_string_result() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(&harness, "js_exec", json!({ "code": "1 + 1" }), &capability)
        .expect("js_exec should succeed");

    assert_eq!(result, json!("2"));
}

#[test]
fn js_exec_with_a_syntax_error_returns_tool_result_is_error_true_with_the_error_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "js_exec",
        json!({ "code": "function {" }),
        &capability,
    )
    .expect("js_exec should return a user-facing error result");

    assert_error_result_contains(&result, "error");
}

#[test]
fn js_exec_without_code_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "js_exec", json!({}), &capability));
}

#[test]
fn list_dir_root_returns_entries_in_the_root_directory() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/readme.md", b"hi").unwrap();
    harness.vfs.write("/todo.txt", b"todo").unwrap();

    let result = call_tool(&harness, "list_dir", json!({ "path": "/" }), &capability)
        .expect("list_dir should succeed");

    let listing = string_result(&result);
    assert!(listing.contains("workspace/"));
    assert!(listing.contains("todo.txt"));
}

#[test]
fn list_dir_on_a_non_existent_path_returns_tool_result_is_error_true() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace/missing" }),
        &capability,
    )
    .expect("list_dir should return a user-facing error result");

    assert_eq!(result.get("is_error").and_then(Value::as_bool), Some(true));
}

#[test]
fn directory_entries_are_suffixed_with_slash_in_the_output() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/src/main.rs", b"fn main() {}")
        .unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace" }),
        &capability,
    )
    .expect("list_dir should succeed");

    let listing = string_result(&result);
    assert!(listing.contains("src/"));
}

#[test]
fn list_dir_directory_suffix_metadata_is_mediated_by_agent_cell_capability() {
    let capability = CapabilityToken {
        paths_read: vec![PathPattern("/workspace".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/secret/file.txt", b"classified")
        .unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(denied)) => {
            assert_eq!(denied.operation, "path_read");
            assert!(
                denied.reason.contains("/workspace/secret"),
                "denial should identify the child path whose metadata was checked, got {:?}",
                denied.reason
            );
        }
        other => panic!("expected mediated metadata capability denial, got {other:?}"),
    }
}

#[test]
fn agent_cell_capability_denial_surfaces_as_tool_error_capability_denied_through_the_tool() {
    let capability = no_read_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let result = call_tool(
        &harness,
        "file_read",
        json!({ "path": "/workspace/hello.txt" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error, got {other:?}"),
    }
}

#[test]
fn agent_cell_budget_exhaustion_surfaces_as_tool_error_execution_failed_with_resource_details() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), budget_with_vfs_bytes_exhausted());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/budget.txt",
            "content": "data"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            let lower = message.to_ascii_lowercase();
            assert!(lower.contains("vfs_bytes"));
            assert!(lower.contains("1"));
        }
        other => panic!("expected execution failed error, got {other:?}"),
    }
}

#[test]
fn vfs_errors_from_agent_cell_surface_as_tool_error_execution_failed_with_path_and_error_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/",
            "content": "root"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            let lower = message.to_ascii_lowercase();
            assert!(message.contains('/'));
            assert!(lower.contains("not a file"));
        }
        other => panic!("expected execution failed error, got {other:?}"),
    }
}

