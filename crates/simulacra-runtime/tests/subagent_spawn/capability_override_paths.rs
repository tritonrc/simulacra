#[tokio::test]
async fn spawn_agent_tool_normalizes_directory_scope_capability_overrides_before_spawn_config() {
    let (result, captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
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
