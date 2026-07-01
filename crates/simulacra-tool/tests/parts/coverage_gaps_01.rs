// ---------------------------------------------------------------------------
// FT6: file_edit missing old_string / new_string error paths
// ---------------------------------------------------------------------------

#[test]
fn file_edit_without_old_string_returns_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"content")
        .unwrap();

    assert_invalid_arguments(call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "new_string": "replacement"
        }),
        &capability,
    ));
}

#[test]
fn file_edit_without_new_string_returns_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"content")
        .unwrap();

    assert_invalid_arguments(call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "content"
        }),
        &capability,
    ));
}

// ---------------------------------------------------------------------------
// FT7: list_dir on a file path (not a directory)
// ---------------------------------------------------------------------------

#[test]
fn list_dir_on_a_file_returns_error_result() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/readme.md", b"hello").unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace/readme.md" }),
        &capability,
    )
    .expect("list_dir on a file should return a user-facing error result, not a ToolError");

    assert_error_result_contains(&result, "not a directory");
}

// ---------------------------------------------------------------------------
// GFT3: list_dir without path argument
// ---------------------------------------------------------------------------

#[test]
fn list_dir_without_path_argument_returns_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "list_dir", json!({}), &capability));
}

// ---------------------------------------------------------------------------
// FT8: capability-denial tests for file_write, shell_exec, list_dir
// ---------------------------------------------------------------------------

fn no_write_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![], // no write paths
        ..Default::default()
    }
}

fn no_shell_capability() -> CapabilityToken {
    CapabilityToken {
        shell: false, // shell denied
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn no_read_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![], // no read paths
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

#[test]
fn file_write_with_denied_write_capability_returns_capability_denied() {
    let capability = no_write_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/secret.txt",
            "content": "should be denied"
        }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error for file_write, got {other:?}"),
    }
}

#[test]
fn shell_exec_with_denied_shell_capability_returns_capability_denied() {
    let capability = no_shell_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error for shell_exec, got {other:?}"),
    }
}

#[test]
fn list_dir_with_denied_read_capability_returns_capability_denied() {
    let capability = no_read_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/file.txt", b"data").unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error for list_dir, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// GFT5: file_write budget exhaustion via max_vfs_bytes
// (file_write checks VFS bytes budget, not turns budget)
// ---------------------------------------------------------------------------

fn budget_with_vfs_bytes_exhausted() -> ResourceBudget {
    ResourceBudget {
        max_vfs_bytes: 1,
        used_vfs_bytes: 1,
        ..ResourceBudget::new(0, 0, Decimal::ZERO, 0)
    }
}

#[test]
fn file_write_with_exhausted_vfs_bytes_budget_returns_execution_failed() {
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
            assert!(
                lower.contains("vfs_bytes"),
                "expected budget error to mention 'vfs_bytes', got: {message}"
            );
        }
        other => panic!("expected execution failed error for budget exhaustion, got {other:?}"),
    }
}

// shell_exec checks turns budget; verify it surfaces as ExecutionFailed.
fn budget_with_turns_exhausted() -> ResourceBudget {
    ResourceBudget {
        used_turns: 1,
        ..ResourceBudget::new(0, 1, Decimal::ZERO, 0)
    }
}

#[test]
fn shell_exec_with_exhausted_turns_budget_returns_execution_failed() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), budget_with_turns_exhausted());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello" }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            let lower = message.to_ascii_lowercase();
            assert!(
                lower.contains("turns"),
                "expected budget error to mention 'turns', got: {message}"
            );
        }
        other => panic!("expected execution failed error for budget exhaustion, got {other:?}"),
    }
}

