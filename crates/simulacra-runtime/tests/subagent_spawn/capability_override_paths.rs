#[tokio::test]
async fn spawn_agent_tool_normalizes_directory_scope_capability_overrides_before_spawn_config() {
    let (result, captured) = run_spawn_tool_call_with_caller_capability(
        serde_json::json!({
            "placement": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            },
            "capabilities": {
                "paths_write": [
                    "/workspace/specs",
                    "/workspace/crates",
                    "/workspace/tests/",
                    "/workspace/src/**",
                    "/workspace/Cargo.toml",
                    "/workspace/Makefile",
                    "/workspace/LICENSE",
                    "/workspace/../secrets/**"
                ],
                "paths_read": [
                    "/workspace/specs",
                    "/workspace/crates",
                    "/workspace/tests/",
                    "/workspace/src/**",
                    "/workspace/Cargo.toml",
                    "/workspace/Makefile",
                    "/workspace/LICENSE",
                    "/workspace/../secrets/**"
                ]
            }
        }),
        &["researcher"],
        Ok(child_success_output()),
        CapabilityToken {
            paths_write: vec![
                PathPattern("/workspace/specs/**".into()),
                PathPattern("/workspace/crates/**".into()),
                PathPattern("/workspace/tests/**".into()),
                PathPattern("/workspace/src/**".into()),
                PathPattern("/workspace/Cargo.toml".into()),
                PathPattern("/workspace/Makefile".into()),
                PathPattern("/workspace/LICENSE".into()),
                PathPattern("/secrets/**".into()),
            ],
            paths_read: vec![
                PathPattern("/workspace/specs/**".into()),
                PathPattern("/workspace/crates/**".into()),
                PathPattern("/workspace/tests/**".into()),
                PathPattern("/workspace/src/**".into()),
                PathPattern("/workspace/Cargo.toml".into()),
                PathPattern("/workspace/Makefile".into()),
                PathPattern("/workspace/LICENSE".into()),
                PathPattern("/secrets/**".into()),
            ],
            spawn_placements: vec!["researcher".into()],
            ..Default::default()
        },
    )
    .await;

    result.expect("successful child result should still return a tool payload");
    let cap = captured
        .capability
        .as_ref()
        .expect("capability should be Some when LLM provides capabilities");
    assert_eq!(
        cap.paths_write,
        vec![
            PathPattern("/workspace/specs/**".into()),
            PathPattern("/workspace/crates/**".into()),
            PathPattern("/workspace/tests/**".into()),
            PathPattern("/workspace/src/**".into()),
            PathPattern("/workspace/Cargo.toml".into()),
            PathPattern("/workspace/Makefile".into()),
            PathPattern("/workspace/LICENSE".into()),
            PathPattern("/secrets/**".into()),
        ],
        "directory scope overrides should become recursive globs, exact file paths should remain exact, and traversal-shaped subtree globs are normalized first"
    );
    assert_eq!(
        cap.paths_read,
        vec![
            PathPattern("/workspace/specs/**".into()),
            PathPattern("/workspace/crates/**".into()),
            PathPattern("/workspace/tests/**".into()),
            PathPattern("/workspace/src/**".into()),
            PathPattern("/workspace/Cargo.toml".into()),
            PathPattern("/workspace/Makefile".into()),
            PathPattern("/workspace/LICENSE".into()),
            PathPattern("/secrets/**".into()),
        ],
        "directory scope overrides should become recursive globs, exact file paths should remain exact, and traversal-shaped subtree globs are normalized first"
    );
}

fn star_spawn_arguments(paths: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "placement": "researcher",
        "task": "check",
        "budget": { "max_tokens": 1, "max_turns": 1, "max_cost": "0", "max_sub_agents": 0 },
        "capabilities": { "paths_read": paths, "paths_write": paths }
    })
}

fn whole_tree_caller() -> CapabilityToken {
    CapabilityToken {
        paths_write: vec![PathPattern("/**".into())],
        paths_read: vec![PathPattern("/**".into())],
        spawn_placements: vec!["researcher".into()],
        ..Default::default()
    }
}

// A model naturally narrows a child to `/dir/*`; under a `/**` parent that
// grant must reach the directory's files, not intersect to a pattern that
// matches nothing.
#[tokio::test]
async fn a_single_star_override_grants_the_files_directly_inside_the_directory() {
    let (result, captured) = run_spawn_tool_call_with_caller_capability(
        star_spawn_arguments(&["/artifacts/*"]),
        &["researcher"],
        Ok(child_success_output()),
        whole_tree_caller(),
    )
    .await;

    result.expect("the spawn is accepted");
    let cap = captured.capability.as_ref().expect("capability override applied");
    assert_eq!(cap.paths_read, vec![PathPattern("/artifacts/*".into())]);
    assert!(cap.check_path_read("/artifacts/probe.md").is_ok());
    assert!(cap.check_path_write("/artifacts/probe.md").is_ok());
    assert!(cap.check_path_read("/artifacts/sub/deeper.md").is_err());
    assert!(cap.check_path_write("/notes/other.md").is_err());
}

// A pattern no capability can match is refused at the spawn, so the model
// learns to rewrite it instead of starting a child that can reach nothing.
#[tokio::test]
async fn a_pattern_no_capability_can_match_is_refused_at_the_spawn() {
    // Called directly: a refused spawn sends nothing to the supervisor, so the
    // shared harness (which waits for that message) would never return.
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec!["researcher".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        guidance: None,
    };
    let result = tool
        .call(star_spawn_arguments(&["/artifacts/*.md"]), &whole_tree_caller())
        .await;

    match result {
        Err(simulacra_types::ToolError::InvalidArguments(message)) => {
            assert!(message.contains("/artifacts/*.md"), "{message}");
            assert!(message.contains("/dir/*"), "{message}");
        }
        other => panic!("expected an invalid-arguments refusal, got {other:?}"),
    }
    assert!(receiver.try_recv().is_err(), "nothing reached the supervisor");
}
