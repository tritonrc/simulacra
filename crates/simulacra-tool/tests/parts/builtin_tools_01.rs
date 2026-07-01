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
            "file_edit",
            "Apply a search-and-replace edit to an existing file.",
        ),
        (
            "shell_exec",
            "Execute a shell command in the agent's virtual shell and return \
                stdout, stderr, and exit code. \
                Supported builtins: echo, cat, ls, mkdir, cp, mv, rm, head, tail, sed, grep, \
                wc, find, sort, uniq, cut, tr, tee, true, false, cd, pwd, env, which, export, \
                curl, wget. \
                Operators: pipes (|), redirects (>, >>), conditional chains (&&, ||), \
                sequence (;). State that persists across calls: env vars and the working \
                directory (cd /tmp; later calls see /tmp as cwd). Interpreter aliases: \
                node <file.js>, node -e <code>, node - for stdin, python <script.py>, \
                python -c <code>, and python - for stdin run through mediated sandbox \
                runtimes. All paths resolve inside the agent's sandbox VFS — there is no \
                host filesystem access.",
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
        ("file_edit", vec!["path", "old_string", "new_string"]),
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
