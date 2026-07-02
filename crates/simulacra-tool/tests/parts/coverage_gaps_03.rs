// ---------------------------------------------------------------------------
// FT1: SkillTool surface tests
// ---------------------------------------------------------------------------

fn make_skill_tool(
    vfs: &Arc<MemoryFs>,
    catalog: Vec<SkillMeta>,
) -> (SkillTool, Arc<AgentCell>, Arc<MemoryFs>) {
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let capability = full_capability();
    let budget = unlimited_budget();
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs_dyn,
        capability,
        Arc::new(Mutex::new(budget)),
        journal,
        http_client,
    ));
    let tool = SkillTool::new(Arc::clone(&cell), catalog);
    (tool, cell, vfs.clone())
}

fn sample_skill_content() -> &'static str {
    "\
---
name: code-review
description: Review code for quality
---
# Code Review

Review the code carefully.
"
}

#[test]
fn skill_tool_definition_name_is_skill() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let def = tool.definition();
    assert_eq!(def.name, "Skill");
}

#[test]
fn skill_tool_definition_schema_requires_command() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let def = tool.definition();
    let required = def
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .unwrap();
    assert!(required.contains(&json!("command")));
}

#[test]
fn skill_tool_definition_includes_catalog_description_for_model_visible_skills() {
    let vfs = Arc::new(MemoryFs::new());
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/cr/SKILL.md".into(),
        disable_model_invocation: false,
        allow_implicit_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let def = tool.definition();
    assert!(
        def.description.contains("code-review"),
        "expected skill catalog in description, got: {}",
        def.description
    );
    assert!(
        def.description.contains("Review code for quality"),
        "expected skill description in definition, got: {}",
        def.description
    );
}

#[test]
fn skill_tool_definition_excludes_model_disabled_skills_from_catalog() {
    let vfs = Arc::new(MemoryFs::new());
    let catalog = vec![SkillMeta {
        name: "internal-only".into(),
        description: "Should not appear".into(),
        vfs_path: "/skills/internal/SKILL.md".into(),
        disable_model_invocation: true,
        allow_implicit_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let def = tool.definition();
    assert!(
        !def.description.contains("internal-only"),
        "model-disabled skill should be excluded from catalog, got: {}",
        def.description
    );
}

#[test]
fn skill_tool_definition_excludes_implicit_disabled_skills_from_catalog() {
    let vfs = Arc::new(MemoryFs::new());
    let catalog = vec![SkillMeta {
        name: "quiet-only".into(),
        description: "Should not appear".into(),
        vfs_path: "/skills/quiet/SKILL.md".into(),
        disable_model_invocation: false,
        allow_implicit_invocation: false,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let def = tool.definition();
    assert!(
        !def.description.contains("quiet-only"),
        "implicit-disabled skill should be excluded from catalog, got: {}",
        def.description
    );
}

#[test]
fn skill_tool_call_with_unknown_command_returns_error_result() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let capability = full_capability();

    let result = run_async(tool.call(json!({ "command": "nonexistent" }), &capability))
        .expect("unknown skill should return error result, not ToolError");

    assert_error_result_contains(&result, "unknown skill");
}

#[test]
fn skill_tool_call_without_command_returns_invalid_arguments() {
    let vfs = Arc::new(MemoryFs::new());
    let (tool, _, _) = make_skill_tool(&vfs, vec![]);
    let capability = full_capability();

    let result = run_async(tool.call(json!({}), &capability));
    assert_invalid_arguments(result);
}

#[test]
fn skill_tool_call_with_model_disabled_skill_returns_error_result() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write(
        "/skills/internal/SKILL.md",
        sample_skill_content().as_bytes(),
    )
    .unwrap();
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/internal/SKILL.md".into(),
        disable_model_invocation: true,
        allow_implicit_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let capability = full_capability();

    let result = run_async(tool.call(json!({ "command": "code-review" }), &capability))
        .expect("model-disabled skill should return error result");

    assert_error_result_contains(&result, "disable_model_invocation");
}

#[test]
fn skill_tool_call_with_implicit_disabled_skill_returns_error_result() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/skills/quiet/SKILL.md", sample_skill_content().as_bytes())
        .unwrap();
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/quiet/SKILL.md".into(),
        disable_model_invocation: false,
        allow_implicit_invocation: false,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let capability = full_capability();

    let result = run_async(tool.call(json!({ "command": "code-review" }), &capability))
        .expect("implicit-disabled skill should return error result");

    assert_error_result_contains(&result, "allow_implicit_invocation=false");
}

#[test]
fn skill_tool_call_with_capability_denied_skill_returns_error_result() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/skills/cr/SKILL.md", sample_skill_content().as_bytes())
        .unwrap();
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/cr/SKILL.md".into(),
        disable_model_invocation: false,
        allow_implicit_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    // Restrict capability to deny the skill
    let capability = CapabilityToken {
        skill_patterns: vec!["skill:other-skill".into()],
        paths_read: vec![PathPattern("/**".into())],
        ..Default::default()
    };

    let result = run_async(tool.call(json!({ "command": "code-review" }), &capability))
        .expect("denied skill should return error result");

    assert_error_result_contains(&result, "not allowed");
}

#[test]
fn skill_tool_call_with_valid_skill_returns_markdown_body() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/skills/cr/SKILL.md", sample_skill_content().as_bytes())
        .unwrap();
    let catalog = vec![SkillMeta {
        name: "code-review".into(),
        description: "Review code for quality".into(),
        vfs_path: "/skills/cr/SKILL.md".into(),
        disable_model_invocation: false,
        allow_implicit_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let capability = full_capability();

    let result = run_async(tool.call(json!({ "command": "code-review" }), &capability))
        .expect("valid skill call should succeed");

    let body = result.as_str().expect("result should be a string");
    assert!(
        body.contains("Review the code carefully"),
        "expected skill body content, got: {body}"
    );
    // Should NOT contain frontmatter
    assert!(
        !body.contains("---"),
        "frontmatter should be stripped, got: {body}"
    );
}

#[test]
fn skill_tool_catalog_truncation_indicates_partial_when_budget_exceeded() {
    let vfs = Arc::new(MemoryFs::new());
    // Create a catalog with many skills that exceed a small budget
    let mut catalog = Vec::new();
    for i in 0..100 {
        catalog.push(SkillMeta {
            name: format!("skill-with-a-very-long-name-number-{i}"),
            description: format!("This is a very long description for skill number {i} that should consume budget quickly"),
            vfs_path: format!("/skills/s{i}/SKILL.md"),
            disable_model_invocation: false,
            allow_implicit_invocation: true,
            user_invocable: true,
            allowed_tools: vec![],
            body: Some("body".into()),
        });
    }
    let (tool, _, _) = make_skill_tool(&vfs, catalog);
    let def = tool.definition();

    // The default budget is 4096 chars; 100 skills with long names should exceed it
    assert!(
        def.description.contains("partial"),
        "expected partial catalog indication when budget exceeded, got: {}",
        def.description
    );
}

#[test]
fn skill_tool_catalog_truncates_long_descriptions_before_omitting_the_skill() {
    let vfs = Arc::new(MemoryFs::new());
    let (_, cell, _) = make_skill_tool(&vfs, vec![]);
    let catalog = vec![SkillMeta {
        name: "oversized".into(),
        description: "x".repeat(10_000),
        vfs_path: "/skills/oversized/SKILL.md".into(),
        disable_model_invocation: false,
        allow_implicit_invocation: true,
        user_invocable: true,
        allowed_tools: vec![],
        body: Some("body".into()),
    }];
    let tool = SkillTool::new_with_metadata_budget(cell, catalog, 64);
    let def = tool.definition();

    assert!(
        def.description.contains("oversized"),
        "a single oversized skill should be retained with a truncated description, got: {}",
        def.description
    );
    assert!(
        def.description.contains("..."),
        "the retained oversized entry should indicate description truncation, got: {}",
        def.description
    );
    assert!(
        !def.description.contains("partial"),
        "truncating one oversized description should not mark the catalog partial"
    );
}
