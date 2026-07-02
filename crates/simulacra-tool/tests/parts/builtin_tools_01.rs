#[test]
fn register_builtins_registers_exactly_six_tools() {
    let harness = Harness::new(full_capability(), unlimited_budget());

    assert_eq!(harness.registry.definitions().len(), 6);
}

#[test]
fn tool_registry_definitions_after_register_builtins_have_correct_names_and_descriptions() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let definitions = harness.registry.definitions();
    let expected = [
        (
            "file_read",
            "Read the contents of a file at the given path.",
        ),
        (
            "file_write",
            "Write content to a file, creating parent directories as needed.",
        ),
        (
            "apply_patch",
            "Apply a Simulacra-style patch to the VFS.",
        ),
        (
            "shell_exec",
            "Execute a shell command in the sandbox shell and return structured output.",
        ),
        (
            "js_exec",
            "Execute JavaScript code in QuickJS and return the string result or stdout. Each call gets a fresh JS global/context: variables, prototypes, and module singletons do not persist between calls. Use ESM `import`, not `require`. Available modules include simulacra:fs/fs, simulacra:console, simulacra:process, simulacra:path, and simulacra:crypto. File, fetch, and module-load host operations are mediated by the sandbox.",
        ),
        ("list_dir", "List the contents of a directory."),
    ];

    for (name, description) in expected {
        assert!(
            definitions
                .iter()
                .any(|definition| definition.name == name && definition.description == description),
            "missing definition for {name} with description {description:?}: {definitions:#?}"
        );
    }
}

#[test]
fn each_tool_definition_has_a_valid_json_schema_as_input_schema() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let definitions = harness.registry.definitions();
    let expected_required = [
        ("file_read", vec!["path"]),
        ("file_write", vec!["path", "content"]),
        ("apply_patch", vec!["patch"]),
        ("shell_exec", vec!["command"]),
        ("js_exec", vec!["code"]),
        ("list_dir", vec!["path"]),
    ];

    for (name, required_fields) in expected_required {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("missing definition for {name}"));
        let schema = &definition.input_schema;

        assert_eq!(schema.get("type"), Some(&json!("object")));
        assert!(
            schema.get("properties").is_some(),
            "missing properties for {name}"
        );
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing required array for {name}"))
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(required, required_fields);
    }
}

#[test]
fn file_edit_is_not_registered_and_returns_unknown_tool_behavior() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert!(
        !harness
            .registry
            .definitions()
            .iter()
            .any(|definition| definition.name == "file_edit"),
        "file_edit must not be exposed in S012 builtins"
    );

    match call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "old",
            "new_string": "new"
        }),
        &capability,
    ) {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(
                message.contains("unknown tool") && message.contains("file_edit"),
                "unknown-tool error should name file_edit, got {message:?}"
            );
        }
        other => panic!("expected unknown-tool behavior for file_edit, got {other:?}"),
    }
}
