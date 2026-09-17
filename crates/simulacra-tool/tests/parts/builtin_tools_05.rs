struct RegistryProbeTool {
    name: &'static str,
    description: &'static str,
    output_schema: Option<Value>,
    supports_parallel_tool_calls: bool,
    waits_for_runtime_cancellation: bool,
}

impl RegistryProbeTool {
    fn new(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            output_schema: None,
            supports_parallel_tool_calls: false,
            waits_for_runtime_cancellation: false,
        }
    }

    fn with_metadata(mut self) -> Self {
        self.output_schema = Some(json!({
            "type": "object",
            "properties": {
                "ok": { "type": "boolean" }
            }
        }));
        self.supports_parallel_tool_calls = true;
        self.waits_for_runtime_cancellation = true;
        self
    }
}

impl simulacra_tool::Tool for RegistryProbeTool {
    fn definition(&self) -> simulacra_tool::ToolDefinition {
        simulacra_tool::ToolDefinition {
            name: self.name.into(),
            description: self.description.into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        _arguments: Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Value, simulacra_tool::ToolError>> + Send + '_>,
    > {
        Box::pin(async { Ok(json!("ok")) })
    }

    fn output_schema(&self) -> Option<Value> {
        self.output_schema.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.supports_parallel_tool_calls
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        self.waits_for_runtime_cancellation
    }
}

struct ArgumentEchoTool;

impl simulacra_tool::Tool for ArgumentEchoTool {
    fn definition(&self) -> simulacra_tool::ToolDefinition {
        simulacra_tool::ToolDefinition {
            name: "arg_echo".into(),
            description: "Echo arguments".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        arguments: Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Value, simulacra_tool::ToolError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(arguments) })
    }
}

struct OwnHookTool;

impl simulacra_tool::Tool for OwnHookTool {
    fn definition(&self) -> simulacra_tool::ToolDefinition {
        simulacra_tool::ToolDefinition {
            name: "own_hook".into(),
            description: "Owns hooks".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn handles_own_hooks(&self) -> bool {
        true
    }

    fn call(
        &self,
        _arguments: Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Value, simulacra_tool::ToolError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(json!("owned")) })
    }
}

struct RecordingRewriteHook {
    contexts: Arc<Mutex<Vec<(simulacra_hooks::Phase, String)>>>,
    before_arguments: Option<Value>,
}

impl simulacra_hooks::HookModule for RecordingRewriteHook {
    fn name(&self) -> &str {
        "recording_rewrite"
    }

    fn invoke(
        &self,
        phase: simulacra_hooks::Phase,
        _operation: simulacra_hooks::Operation,
        context: &str,
    ) -> Result<simulacra_hooks::Verdict, simulacra_hooks::HookError> {
        self.contexts
            .lock()
            .unwrap()
            .push((phase, context.to_string()));
        if phase == simulacra_hooks::Phase::Before
            && let Some(arguments) = &self.before_arguments
        {
            return Ok(simulacra_hooks::Verdict::Continue(Some(
                json!({
                    "tool": "arg_echo",
                    "arguments": arguments,
                })
                .to_string(),
            )));
        }
        Ok(simulacra_hooks::Verdict::Continue(None))
    }
}

struct DenyHook;

impl simulacra_hooks::HookModule for DenyHook {
    fn name(&self) -> &str {
        "deny"
    }

    fn invoke(
        &self,
        _phase: simulacra_hooks::Phase,
        _operation: simulacra_hooks::Operation,
        _context: &str,
    ) -> Result<simulacra_hooks::Verdict, simulacra_hooks::HookError> {
        Ok(simulacra_hooks::Verdict::Deny("blocked by test".into()))
    }
}

#[test]
fn duplicate_tool_registration_fails_deterministically() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register(Box::new(RegistryProbeTool::new("dupe", "first")))
        .expect("first registration should succeed");

    match registry.try_register(Box::new(RegistryProbeTool::new("dupe", "second"))) {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(
                message.contains("duplicate tool registration") && message.contains("dupe"),
                "duplicate registration error should name the tool, got {message:?}"
            );
        }
        other => panic!("expected duplicate registration failure, got {other:?}"),
    }
}

#[test]
fn register_builtins_fails_on_duplicate_before_partial_registration() {
    let capability = full_capability();
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        capability,
        Arc::new(Mutex::new(unlimited_budget())),
        journal,
        http_client,
    ));
    let mut registry = ToolRegistry::new();
    registry
        .try_register(Box::new(RegistryProbeTool::new(
            "shell_exec",
            "conflicting tool",
        )))
        .expect("initial conflicting registration should succeed");

    let result = register_builtins(&mut registry, cell);

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(
                message.contains("duplicate tool registration")
                    && message.contains("shell_exec"),
                "duplicate error should name shell_exec, got {message:?}"
            );
        }
        other => panic!("expected duplicate registration failure, got {other:?}"),
    }

    let names: Vec<String> = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(names, vec!["shell_exec".to_string()]);
}

#[test]
fn hidden_tools_are_callable_but_omitted_from_model_visible_definitions() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register_hidden(Box::new(RegistryProbeTool::new(
            "hidden_probe",
            "dispatch only",
        )))
        .expect("hidden registration should succeed");

    assert!(
        registry
            .definitions()
            .iter()
            .all(|definition| definition.name != "hidden_probe"),
        "hidden tools must not be model-visible"
    );

    let result = call_registry(
        &registry,
        "hidden_probe",
        json!({}),
        &CapabilityToken::default(),
    )
    .expect("hidden tool should still dispatch");

    assert_eq!(tool_content(&result), "ok");
}

#[test]
fn deferred_tools_are_omitted_initially_and_discoverable_by_search() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register_deferred(Box::new(RegistryProbeTool::new(
            "weather_lookup",
            "Find weather by city",
        )))
        .expect("deferred registration should succeed");

    assert!(registry.definitions().is_empty());

    let matches = registry.search_deferred("weather");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "weather_lookup");
}

#[test]
fn registry_metadata_exposes_output_parallel_and_cancellation_flags() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register_with_exposure(
            Box::new(RegistryProbeTool::new("metadata_probe", "metadata").with_metadata()),
            simulacra_tool::ToolExposure::Direct,
        )
        .expect("registration should succeed");

    let metadata = registry
        .metadata("metadata_probe")
        .expect("metadata should be present");
    assert_eq!(metadata.exposure, simulacra_tool::ToolExposure::Direct);
    assert_eq!(
        metadata
            .output_schema
            .as_ref()
            .and_then(|schema| schema.get("type")),
        Some(&json!("object"))
    );
    assert!(metadata.supports_parallel_tool_calls);
    assert!(metadata.waits_for_runtime_cancellation);
}

#[test]
fn tool_schema_helpers_build_common_object_string_number_and_boolean_schemas() {
    let schema = simulacra_tool::ToolSchema::object(
        [
            ("name", simulacra_tool::ToolSchema::string("display name")),
            ("retries", simulacra_tool::ToolSchema::integer("retry count")),
            ("score", simulacra_tool::ToolSchema::number("confidence")),
            ("enabled", simulacra_tool::ToolSchema::boolean("feature flag")),
        ],
        ["name", "enabled"],
    );

    assert_eq!(schema.get("type"), Some(&json!("object")));
    assert_eq!(schema.get("additionalProperties"), Some(&json!(false)));
    assert_eq!(
        schema
            .pointer("/properties/name/description")
            .and_then(Value::as_str),
        Some("display name")
    );
    assert_eq!(
        schema.pointer("/properties/retries/type"),
        Some(&json!("integer"))
    );
    assert_eq!(
        schema.pointer("/properties/score/type"),
        Some(&json!("number"))
    );
    assert_eq!(
        schema.pointer("/properties/enabled/type"),
        Some(&json!("boolean"))
    );
}

#[test]
fn generic_hook_rewrite_uses_stable_input_and_output_payloads() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingRewriteHook {
        contexts: Arc::clone(&contexts),
        before_arguments: Some(json!({ "rewritten": true })),
    });
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(simulacra_hooks::Operation::ToolCall, hook);

    let mut registry = ToolRegistry::new();
    registry.set_pipeline(Arc::new(pipeline));
    registry
        .register(Box::new(ArgumentEchoTool))
        .expect("test tool registration should succeed");

    let result = call_registry(
        &registry,
        "arg_echo",
        json!({ "original": true }),
        &CapabilityToken::default(),
    )
    .expect("hooked tool call should succeed");

    assert_eq!(result, json!({ "rewritten": true }));

    let contexts = contexts.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    let before: Value = serde_json::from_str(&contexts[0].1).unwrap();
    assert_eq!(before.get("tool"), Some(&json!("arg_echo")));
    assert_eq!(
        before.get("arguments"),
        Some(&json!({ "original": true }))
    );
    let after: Value = serde_json::from_str(&contexts[1].1).unwrap();
    assert_eq!(after.get("tool"), Some(&json!("arg_echo")));
    assert_eq!(
        after.get("arguments"),
        Some(&json!({ "rewritten": true }))
    );
    assert_eq!(after.get("result"), Some(&json!({ "rewritten": true })));
}

#[test]
fn tools_that_own_hooks_do_not_receive_generic_hook_wrapping() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingRewriteHook {
        contexts: Arc::clone(&contexts),
        before_arguments: None,
    });
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(simulacra_hooks::Operation::ToolCall, hook);

    let mut registry = ToolRegistry::new();
    registry.set_pipeline(Arc::new(pipeline));
    registry
        .register(Box::new(OwnHookTool))
        .expect("test tool registration should succeed");

    let result = call_registry(
        &registry,
        "own_hook",
        json!({}),
        &CapabilityToken::default(),
    )
    .expect("own-hook tool should succeed");

    assert_eq!(tool_content(&result), "owned");
    assert!(
        contexts.lock().unwrap().is_empty(),
        "generic hook pipeline must not wrap tools that own hooks"
    );
}

#[test]
fn apply_patch_adds_a_file_through_the_vfs() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/new.txt\n+hello\n+world\n*** End Patch"
        }),
        &capability,
    )
    .expect("add patch should succeed");

    assert_eq!(
        harness.vfs.read("/workspace/new.txt").unwrap(),
        b"hello\nworld\n"
    );
}

#[test]
fn apply_patch_rejects_add_beneath_file_parent_without_mutating() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/file-parent", b"parent\n")
        .unwrap();

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/file-parent/child.txt\n+child\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("not a directory"), "got {message:?}");
        }
        other => panic!("expected not-a-directory failure, got {other:?}"),
    }
    assert_eq!(
        harness.vfs.read("/workspace/file-parent").unwrap(),
        b"parent\n"
    );
    assert!(!harness.vfs.exists("/workspace/file-parent/child.txt"));
}

#[test]
fn apply_patch_add_with_write_capability_does_not_require_read_capability() {
    let capability = CapabilityToken {
        paths_read: vec![],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(capability.clone(), unlimited_budget());

    call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/write-only.txt\n+created\n*** End Patch"
        }),
        &capability,
    )
    .expect("add patch should only require write capability");

    assert_eq!(
        harness.vfs.read("/workspace/write-only.txt").unwrap(),
        b"created\n"
    );
}

#[test]
fn apply_patch_updates_a_file_when_hunk_matches_current_content() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"one\ntwo\nthree\n")
        .unwrap();

    call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/edit.txt\n@@\n one\n-two\n+TWO\n three\n*** End Patch"
        }),
        &capability,
    )
    .expect("update patch should succeed");

    assert_eq!(
        harness.vfs.read("/workspace/edit.txt").unwrap(),
        b"one\nTWO\nthree\n"
    );
}

#[test]
fn apply_patch_deletes_a_file() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/delete.txt", b"bye").unwrap();

    call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Delete File: /workspace/delete.txt\n*** End Patch"
        }),
        &capability,
    )
    .expect("delete patch should succeed");

    assert!(!harness.vfs.exists("/workspace/delete.txt"));
    let entries = harness
        .journal
        .read_all(&AgentId("sandbox".into()))
        .expect("journal should be readable");
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::FileDelete { path } if path == "/workspace/delete.txt"
            )
        }),
        "delete patch should journal the deleted path, got {entries:#?}"
    );
}

#[test]
fn apply_patch_delete_file_rejects_directories_without_mutating_children() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.mkdir("/workspace/delete-dir").unwrap();
    harness
        .vfs
        .write("/workspace/delete-dir/child.txt", b"keep")
        .unwrap();

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Delete File: /workspace/delete-dir\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("not a file"), "got {message:?}");
        }
        other => panic!("expected directory delete to fail, got {other:?}"),
    }
    assert!(harness.vfs.exists("/workspace/delete-dir"));
    assert_eq!(
        harness.vfs.read("/workspace/delete-dir/child.txt").unwrap(),
        b"keep"
    );
}

#[test]
fn apply_patch_moves_a_file() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/old.txt", b"moved").unwrap();

    call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/old.txt\n*** Move to: /workspace/new.txt\n*** End Patch"
        }),
        &capability,
    )
    .expect("move patch should succeed");

    assert!(!harness.vfs.exists("/workspace/old.txt"));
    assert_eq!(harness.vfs.read("/workspace/new.txt").unwrap(), b"moved");
    let entries = harness
        .journal
        .read_all(&AgentId("sandbox".into()))
        .expect("journal should be readable");
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::FileMove { from, to }
                    if from == "/workspace/old.txt" && to == "/workspace/new.txt"
            )
        }),
        "move patch should journal source and destination, got {entries:#?}"
    );
}

#[test]
fn apply_patch_move_requires_read_capability_for_source() {
    let capability = CapabilityToken {
        paths_read: vec![PathPattern("/workspace/public/**".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/secret.txt", b"secret")
        .unwrap();

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/secret.txt\n*** Move to: /workspace/public/moved.txt\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected source read capability denial, got {other:?}"),
    }
    assert_eq!(
        harness.vfs.read("/workspace/secret.txt").unwrap(),
        b"secret"
    );
    assert!(!harness.vfs.exists("/workspace/public/moved.txt"));
}

#[test]
fn apply_patch_pure_move_over_vfs_budget_fails_before_mutating() {
    let capability = full_capability();
    let budget = ResourceBudget {
        max_vfs_bytes: 5,
        ..ResourceBudget::new(0, 0, Decimal::ZERO, 0)
    };
    let harness = Harness::new(capability.clone(), budget);
    harness
        .vfs
        .write("/workspace/large.txt", b"123456")
        .unwrap();

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/large.txt\n*** Move to: /workspace/moved.txt\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("vfs_bytes"), "got {message:?}");
        }
        other => panic!("expected pure move budget failure, got {other:?}"),
    }
    assert_eq!(harness.vfs.read("/workspace/large.txt").unwrap(), b"123456");
    assert!(!harness.vfs.exists("/workspace/moved.txt"));
}

#[test]
fn apply_patch_malformed_patch_returns_error_result_without_mutating() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Add File: /workspace/new.txt\n+missing begin\n*** End Patch"
        }),
        &capability,
    )
    .expect("malformed patch should be a user-facing error result");

    assert_error_result_contains(&result, "malformed patch");
    assert!(!harness.vfs.exists("/workspace/new.txt"));
}

#[test]
fn apply_patch_update_without_hunks_returns_error_result_without_mutating() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"unchanged\n")
        .unwrap();

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/edit.txt\n*** End Patch"
        }),
        &capability,
    )
    .expect("malformed update should be a user-facing error result");

    assert_error_result_contains(&result, "malformed update");
    assert_eq!(harness.vfs.read("/workspace/edit.txt").unwrap(), b"unchanged\n");
}

#[test]
fn apply_patch_stale_hunk_returns_error_result_without_mutating() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"current\ncontent\n")
        .unwrap();

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/edit.txt\n@@\n-old\n+new\n*** End Patch"
        }),
        &capability,
    )
    .expect("stale hunk should be a user-facing error result");

    assert_error_result_contains(&result, "stale hunk");
    assert_eq!(
        harness.vfs.read("/workspace/edit.txt").unwrap(),
        b"current\ncontent\n"
    );
}

#[test]
fn apply_patch_duplicate_target_path_returns_error_result_without_mutating() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/same.txt\n+one\n*** Add File: /workspace/same.txt\n+two\n*** End Patch"
        }),
        &capability,
    )
    .expect("duplicate patch path should be a user-facing error result");

    assert_error_result_contains(&result, "duplicate path");
    assert!(!harness.vfs.exists("/workspace/same.txt"));
}

#[test]
fn apply_patch_duplicate_normalized_target_path_returns_error_result_without_mutating() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/same.txt", b"old\n").unwrap();

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/a/../same.txt\n@@\n-old\n+one\n*** Update File: /workspace/same.txt\n@@\n-old\n+two\n*** End Patch"
        }),
        &capability,
    )
    .expect("duplicate normalized patch path should be a user-facing error result");

    assert_error_result_contains(&result, "duplicate path");
    assert_eq!(harness.vfs.read("/workspace/same.txt").unwrap(), b"old\n");
}

#[test]
fn apply_patch_denied_path_fails_before_any_mutation() {
    let capability = CapabilityToken {
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/allowed.txt\n+allowed\n*** Add File: /secret.txt\n+denied\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denial, got {other:?}"),
    }
    assert!(!harness.vfs.exists("/workspace/allowed.txt"));
    assert!(!harness.vfs.exists("/secret.txt"));
}

#[test]
fn apply_patch_budget_failure_fails_before_any_mutation() {
    let capability = full_capability();
    let budget = ResourceBudget {
        max_vfs_bytes: 5,
        ..ResourceBudget::new(0, 0, Decimal::ZERO, 0)
    };
    let harness = Harness::new(capability.clone(), budget);

    let result = call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/a.txt\n+abc\n*** Add File: /workspace/b.txt\n+def\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("vfs_bytes"), "got {message:?}");
        }
        other => panic!("expected budget failure, got {other:?}"),
    }
    assert!(!harness.vfs.exists("/workspace/a.txt"));
    assert!(!harness.vfs.exists("/workspace/b.txt"));
}

#[test]
fn apply_patch_move_and_update_succeeds_when_write_bytes_exactly_match_budget() {
    let capability = full_capability();
    let budget = ResourceBudget {
        max_vfs_bytes: 4,
        ..ResourceBudget::new(0, 0, Decimal::ZERO, 0)
    };
    let harness = Harness::new(capability.clone(), budget);
    harness.vfs.write("/workspace/old.txt", b"old\n").unwrap();

    call_tool(
        &harness,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/old.txt\n*** Move to: /workspace/new.txt\n@@\n-old\n+new\n*** End Patch"
        }),
        &capability,
    )
    .expect("move-and-update should reserve budget once for the batch");

    assert!(!harness.vfs.exists("/workspace/old.txt"));
    assert_eq!(harness.vfs.read("/workspace/new.txt").unwrap(), b"new\n");
}

struct RemoveFailingFs {
    inner: Arc<MemoryFs>,
    fail_remove_path: String,
    write_on_failed_remove: Option<(String, Vec<u8>)>,
}

impl VirtualFs for RemoveFailingFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, simulacra_types::VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), simulacra_types::VfsError> {
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, simulacra_types::VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), simulacra_types::VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), simulacra_types::VfsError> {
        if path == self.fail_remove_path {
            if let Some((write_path, data)) = &self.write_on_failed_remove {
                self.inner.write(write_path, data)?;
            }
            return Err(simulacra_types::VfsError::Io(format!(
                "forced remove failure for {path}"
            )));
        }
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<simulacra_types::FsMetadata, simulacra_types::VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<simulacra_types::VfsSnapshot, simulacra_types::VfsError> {
        self.inner.snapshot()
    }

    fn restore(
        &self,
        snapshot: &simulacra_types::VfsSnapshot,
    ) -> Result<(), simulacra_types::VfsError> {
        self.inner.restore(snapshot)
    }
}

#[derive(Debug, Default)]
struct FailFirstAppendJournal {
    entries: Mutex<Vec<JournalEntry>>,
    fail_next_append: std::sync::atomic::AtomicBool,
}

impl FailFirstAppendJournal {
    fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            fail_next_append: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl JournalStorage for FailFirstAppendJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        if self
            .fail_next_append
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(JournalError::Storage("forced append failure".into()));
        }
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.agent_id == *agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|error| JournalError::Storage(error.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

#[test]
fn apply_patch_journal_append_failure_fails_before_mutating() {
    let capability = full_capability();
    let vfs = Arc::new(MemoryFs::new());
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let journal: Arc<dyn JournalStorage> = Arc::new(FailFirstAppendJournal::new());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs_dyn,
        capability.clone(),
        Arc::new(Mutex::new(unlimited_budget())),
        journal,
        http_client,
    ));
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry, cell).expect("built-in registration should succeed");

    let result = call_registry(
        &registry,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/new.txt\n+hello\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("journal append failed"), "got {message:?}");
        }
        other => panic!("expected journal append failure, got {other:?}"),
    }
    assert!(!vfs.exists("/workspace/new.txt"));
}

#[test]
fn apply_patch_rolls_back_when_late_vfs_operation_fails() {
    let capability = full_capability();
    let inner = Arc::new(MemoryFs::new());
    inner.write("/workspace/source.txt", b"old\n").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(RemoveFailingFs {
        inner: Arc::clone(&inner),
        fail_remove_path: "/workspace/source.txt".into(),
        write_on_failed_remove: Some(("/workspace/unrelated.txt".into(), b"concurrent\n".to_vec())),
    });
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        capability.clone(),
        Arc::new(Mutex::new(unlimited_budget())),
        journal,
        http_client,
    ));
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry, cell).expect("built-in registration should succeed");

    let result = call_registry(
        &registry,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/source.txt\n*** Move to: /workspace/dest.txt\n@@\n-old\n+new\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("forced remove failure"), "got {message:?}");
        }
        other => panic!("expected late VFS failure, got {other:?}"),
    }
    assert_eq!(inner.read("/workspace/source.txt").unwrap(), b"old\n");
    assert!(!inner.exists("/workspace/dest.txt"));
    assert_eq!(
        inner.read("/workspace/unrelated.txt").unwrap(),
        b"concurrent\n"
    );
}

#[test]
fn apply_patch_rollback_does_not_remove_newer_same_path_write() {
    let capability = full_capability();
    let inner = Arc::new(MemoryFs::new());
    inner.write("/workspace/source.txt", b"old\n").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(RemoveFailingFs {
        inner: Arc::clone(&inner),
        fail_remove_path: "/workspace/source.txt".into(),
        write_on_failed_remove: Some(("/workspace/dest.txt".into(), b"concurrent\n".to_vec())),
    });
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        capability.clone(),
        Arc::new(Mutex::new(unlimited_budget())),
        journal,
        http_client,
    ));
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry, cell).expect("built-in registration should succeed");

    let result = call_registry(
        &registry,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/source.txt\n*** Move to: /workspace/dest.txt\n@@\n-old\n+new\n*** End Patch"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("forced remove failure"), "got {message:?}");
        }
        other => panic!("expected late VFS failure, got {other:?}"),
    }
    assert_eq!(inner.read("/workspace/source.txt").unwrap(), b"old\n");
    assert_eq!(inner.read("/workspace/dest.txt").unwrap(), b"concurrent\n");
}

#[test]
fn shell_exec_workdir_applies_to_one_call_without_persisting() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/subdir/file.txt", b"data")
        .unwrap();

    let first = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "pwd", "workdir": "/workspace/subdir" }),
        &capability,
    )
    .expect("shell workdir call should succeed");
    assert_eq!(
        tool_structured(&first).get("stdout"),
        Some(&json!("/workspace/subdir\n"))
    );

    let second = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "pwd" }),
        &capability,
    )
    .expect("plain shell call should succeed");
    assert_eq!(tool_structured(&second).get("stdout"), Some(&json!("/\n")));
}

#[test]
fn shell_exec_invalid_workdir_fails_without_running_command() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo should-not-run > /workspace/out.txt", "workdir": "/workspace/missing" }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("not found"), "got {message:?}");
        }
        other => panic!("expected missing workdir to fail, got {other:?}"),
    }
    assert!(!harness.vfs.exists("/workspace/out.txt"));
}

#[test]
fn shell_exec_truncates_stdout_with_length_metadata() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo 1234567890", "max_output_tokens": 1 }),
        &capability,
    )
    .expect("shell call should succeed");
    let structured = tool_structured(&result);

    assert_eq!(structured.get("stdout"), Some(&json!("1234")));
    assert_eq!(structured.get("truncated"), Some(&json!(true)));
    assert_eq!(structured.get("stdout_original_len"), Some(&json!(11)));
    assert_eq!(structured.get("stdout_truncated_len"), Some(&json!(4)));
}

#[test]
fn shell_exec_length_metadata_counts_characters_not_bytes() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo ééééé", "max_output_tokens": 1 }),
        &capability,
    )
    .expect("shell call should succeed");
    let structured = tool_structured(&result);

    assert_eq!(structured.get("stdout"), Some(&json!("éééé")));
    assert_eq!(structured.get("stdout_original_len"), Some(&json!(6)));
    assert_eq!(structured.get("stdout_truncated_len"), Some(&json!(4)));
}

#[test]
fn shell_exec_large_max_output_tokens_does_not_overflow() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo ok", "max_output_tokens": u64::MAX }),
        &capability,
    )
    .expect("large max_output_tokens should not panic or fail");

    let structured = tool_structured(&result);
    assert_eq!(structured.get("stdout"), Some(&json!("ok\n")));
    assert_eq!(structured.get("truncated"), Some(&json!(false)));
}

#[test]
fn shell_exec_rejects_unsupported_persistent_session_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello", "session_id": 7 }),
        &capability,
    ));
}

#[test]
fn shell_exec_rejects_stdin_continuation_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "cat", "stdin": "hello" }),
        &capability,
    ));
    assert_invalid_arguments(call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "cat", "input": "hello" }),
        &capability,
    ));
}

#[test]
fn shell_exec_rejects_unknown_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello", "background": true }),
        &capability,
    ));
}

#[test]
fn shell_exec_accepts_yield_time_ms_without_creating_a_session() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello", "yield_time_ms": 10 }),
        &capability,
    )
    .expect("yield_time_ms should be accepted for one-shot shell calls");

    assert_eq!(tool_structured(&result).get("stdout"), Some(&json!("hello\n")));
}
