struct ShellAdvertisedProbeTool {
    name: &'static str,
}

impl simulacra_tool::Tool for ShellAdvertisedProbeTool {
    fn definition(&self) -> simulacra_tool::ToolDefinition {
        simulacra_tool::ToolDefinition {
            name: self.name.into(),
            description: "Only advertised when shell access is available".into(),
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

    fn advertised_to(&self, capability: &CapabilityToken) -> bool {
        capability.check_shell().is_ok()
    }
}

fn definition_names(definitions: Vec<simulacra_tool::ToolDefinition>) -> Vec<String> {
    definitions
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

fn assert_definition_name_set(
    definitions: Vec<simulacra_tool::ToolDefinition>,
    expected: &[&str],
) {
    let names = definition_names(definitions);
    assert_eq!(
        names.len(),
        expected.len(),
        "expected exactly {expected:?}, got {names:?}"
    );
    for expected_name in expected {
        assert!(
            names.iter().any(|name| name == expected_name),
            "expected {expected_name:?} in {names:?}"
        );
    }
}

fn make_cell(capability: CapabilityToken) -> Arc<AgentCell> {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    Arc::new(AgentCell::new(
        vfs,
        capability,
        Arc::new(Mutex::new(unlimited_budget())),
        journal,
        http_client,
    ))
}

#[test]
fn shell_denial_changes_only_the_advertised_view_and_execution_remains_denied() {
    let denied = CapabilityToken {
        shell: false,
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(denied.clone(), unlimited_budget());

    assert!(
        definition_names(harness.registry.definitions_for(&denied))
            .iter()
            .all(|name| name != "shell_exec"),
        "the capability-aware view must omit shell_exec"
    );
    assert!(
        definition_names(harness.registry.definitions())
            .iter()
            .any(|name| name == "shell_exec"),
        "the legacy view must remain unchanged"
    );

    match call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo should-not-run" }),
        &denied,
    ) {
        Err(ToolError::CapabilityDenied(denied)) => {
            assert_eq!(denied.operation, "shell");
        }
        other => panic!("expected shell capability denial, got {other:?}"),
    }
}

#[test]
fn file_tool_advertisement_uses_presence_of_read_and_write_grants() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let read_only = CapabilityToken {
        paths_read: vec![PathPattern("/workspace/**".into())],
        paths_write: vec![],
        ..Default::default()
    };

    let names = definition_names(harness.registry.definitions_for(&read_only));

    assert!(names.iter().any(|name| name == "file_read"));
    assert!(names.iter().any(|name| name == "list_dir"));
    assert!(names.iter().all(|name| name != "file_write"));
    assert!(names.iter().all(|name| name != "apply_patch"));

    let write_only = CapabilityToken {
        paths_read: vec![],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let names = definition_names(harness.registry.definitions_for(&write_only));

    assert!(names.iter().all(|name| name != "file_read"));
    assert!(names.iter().all(|name| name != "list_dir"));
    assert!(names.iter().any(|name| name == "file_write"));
    assert!(names.iter().any(|name| name == "apply_patch"));
}

#[test]
fn memory_only_read_scope_counts_as_a_coarse_file_read_advertisement_grant() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let memory_read_only = CapabilityToken {
        paths_read: vec![],
        paths_write: vec![],
        memory: MemoryCapability {
            enabled: true,
            search_scopes: vec![
                MemoryPath::parse("/var/memory/self").expect("memory read scope should be valid"),
            ],
            write_scopes: vec![],
        },
        ..Default::default()
    };

    let names = definition_names(harness.registry.definitions_for(&memory_read_only));
    assert!(names.iter().any(|name| name == "file_read"));
    assert!(names.iter().any(|name| name == "list_dir"));
    assert!(names.iter().all(|name| name != "file_write"));
    assert!(names.iter().all(|name| name != "apply_patch"));
}

#[test]
fn memory_only_write_scope_counts_as_a_coarse_file_write_advertisement_grant() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let memory_write_only = CapabilityToken {
        paths_read: vec![],
        paths_write: vec![],
        memory: MemoryCapability {
            enabled: true,
            search_scopes: vec![],
            write_scopes: vec![
                MemoryPath::parse("/var/memory/self").expect("memory write scope should be valid"),
            ],
        },
        ..Default::default()
    };

    let names = definition_names(harness.registry.definitions_for(&memory_write_only));
    assert!(names.iter().all(|name| name != "file_read"));
    assert!(names.iter().all(|name| name != "list_dir"));
    assert!(names.iter().any(|name| name == "file_write"));
    assert!(names.iter().any(|name| name == "apply_patch"));
}

#[test]
fn builtin_exec_advertisement_tracks_shell_and_javascript_independently() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let shell_only = CapabilityToken {
        shell: true,
        javascript: false,
        ..Default::default()
    };
    let javascript_only = CapabilityToken {
        shell: false,
        javascript: true,
        ..Default::default()
    };

    let shell_names = definition_names(harness.registry.definitions_for(&shell_only));
    assert!(shell_names.iter().any(|name| name == "shell_exec"));
    assert!(shell_names.iter().all(|name| name != "js_exec"));

    let javascript_names = definition_names(harness.registry.definitions_for(&javascript_only));
    assert!(javascript_names.iter().all(|name| name != "shell_exec"));
    assert!(javascript_names.iter().any(|name| name == "js_exec"));
}

#[test]
fn capability_views_are_derived_without_mutating_registry_state() {
    let mut harness = Harness::new(full_capability(), unlimited_budget());
    let restricted = CapabilityToken::default();
    let broad = full_capability();

    let restricted_before = definition_names(harness.registry.definitions_for(&restricted));
    let broad_names = definition_names(harness.registry.definitions_for(&broad));
    let restricted_after = definition_names(harness.registry.definitions_for(&restricted));

    assert_ne!(restricted_before, broad_names);
    assert_eq!(restricted_before, restricted_after);
    assert_eq!(
        harness
            .registry
            .metadata("shell_exec")
            .expect("shell_exec metadata should remain registered")
            .exposure,
        simulacra_tool::ToolExposure::Direct
    );
    assert_eq!(harness.registry.definitions().len(), 6);

    match harness
        .registry
        .try_register(Box::new(RegistryProbeTool::new(
            "shell_exec",
            "duplicate after derived views",
        ))) {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("duplicate tool registration"));
            assert!(message.contains("shell_exec"));
        }
        other => panic!("expected duplicate registration failure, got {other:?}"),
    }
}

#[test]
fn definitions_for_preserves_direct_exposure_filter() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register(Box::new(RegistryProbeTool::new(
            "direct_default",
            "Direct and advertised by default",
        )))
        .expect("direct registration should succeed");
    registry
        .try_register(Box::new(ShellAdvertisedProbeTool {
            name: "direct_shell",
        }))
        .expect("direct conditional registration should succeed");
    registry
        .try_register_hidden(Box::new(RegistryProbeTool::new(
            "hidden_default",
            "Hidden but advertised by default",
        )))
        .expect("hidden registration should succeed");
    registry
        .try_register_deferred(Box::new(RegistryProbeTool::new(
            "deferred_default",
            "Deferred but advertised by default",
        )))
        .expect("deferred registration should succeed");

    assert_definition_name_set(
        registry.definitions_for(&CapabilityToken::default()),
        &["direct_default"],
    );
    assert_definition_name_set(
        registry.definitions_for(&CapabilityToken {
            shell: true,
            ..Default::default()
        }),
        &["direct_default", "direct_shell"],
    );
}

#[test]
fn deferred_search_for_preserves_query_and_exposure_filters_without_changing_plain_search() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register_deferred(Box::new(ShellAdvertisedProbeTool {
            name: "shell_weather",
        }))
        .expect("deferred registration should succeed");
    registry
        .try_register_deferred(Box::new(RegistryProbeTool::new(
            "deferred_calendar",
            "Calendar reference",
        )))
        .expect("nonmatching deferred registration should succeed");
    registry
        .try_register(Box::new(RegistryProbeTool::new(
            "direct_weather",
            "Weather tool exposed directly",
        )))
        .expect("matching direct registration should succeed");
    registry
        .try_register_hidden(Box::new(RegistryProbeTool::new(
            "hidden_weather",
            "Weather tool hidden from model views",
        )))
        .expect("matching hidden registration should succeed");

    let denied = CapabilityToken::default();
    assert!(
        registry
            .search_deferred_for("weather", &denied)
            .is_empty(),
        "the capability-aware deferred view must omit the tool"
    );

    assert_definition_name_set(registry.search_deferred("weather"), &["shell_weather"]);

    let allowed = CapabilityToken {
        shell: true,
        ..Default::default()
    };
    assert_definition_name_set(
        registry.search_deferred_for("weather", &allowed),
        &["shell_weather"],
    );
    assert!(
        registry
            .search_deferred_for("no-such-query", &allowed)
            .is_empty(),
        "the capability-aware deferred view must retain query matching"
    );
}

#[test]
fn default_advertisement_contract_keeps_tools_visible_for_every_token() {
    let mut registry = ToolRegistry::new();
    registry
        .try_register(Box::new(RegistryProbeTool::new(
            "backward_compatible",
            "Uses Tool::advertised_to default",
        )))
        .expect("registration should succeed");

    let denied = CapabilityToken::default();
    let broad = full_capability();

    assert_eq!(
        definition_names(registry.definitions_for(&denied)),
        vec!["backward_compatible".to_string()]
    );
    assert_eq!(
        definition_names(registry.definitions_for(&broad)),
        vec!["backward_compatible".to_string()]
    );
}

#[test]
fn granular_registration_separates_file_and_exec_tools_and_builtins_remain_complete() {
    let cell = make_cell(full_capability());
    let mut file_registry = ToolRegistry::new();
    register_file_tools(&mut file_registry, Arc::clone(&cell))
        .expect("file tool registration should succeed");

    assert_definition_name_set(
        file_registry.definitions(),
        &["file_read", "file_write", "apply_patch", "list_dir"],
    );
    let file_names = definition_names(file_registry.definitions());
    assert!(file_names.iter().all(|name| name != "shell_exec"));
    assert!(file_names.iter().all(|name| name != "js_exec"));

    let mut exec_registry = ToolRegistry::new();
    register_exec_tools(&mut exec_registry, Arc::clone(&cell))
        .expect("exec tool registration should succeed");
    assert_definition_name_set(exec_registry.definitions(), &["shell_exec", "js_exec"]);

    let mut builtin_registry = ToolRegistry::new();
    register_builtins(&mut builtin_registry, cell)
        .expect("full builtin registration should succeed");
    assert_definition_name_set(
        builtin_registry.definitions(),
        &[
            "file_read",
            "file_write",
            "apply_patch",
            "shell_exec",
            "js_exec",
            "list_dir",
        ],
    );
}

#[test]
fn register_builtins_preserves_legacy_definition_order() {
    let cell = make_cell(full_capability());
    let mut registry = ToolRegistry::new();
    register_builtins(&mut registry, cell).expect("full builtin registration should succeed");

    assert_eq!(
        definition_names(registry.definitions()),
        vec![
            "file_read".to_string(),
            "file_write".to_string(),
            "apply_patch".to_string(),
            "shell_exec".to_string(),
            "js_exec".to_string(),
            "list_dir".to_string(),
        ]
    );
}
