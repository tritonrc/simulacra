use simulacra_config::{SimulacraConfig, build_capability_token};

fn parse_and_validate(source: &str) -> Result<SimulacraConfig, String> {
    let config: SimulacraConfig = toml::from_str(source).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    Ok(config)
}

fn serialized(config: &SimulacraConfig) -> toml::Value {
    toml::Value::try_from(config).expect("validated config should serialize")
}

fn serialized_placement<'a>(value: &'a toml::Value, name: &str) -> &'a toml::value::Table {
    value
        .get("child_placements")
        .and_then(|placements| placements.get(name))
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("serialized config should contain child_placements.{name}"))
}

fn base_config(extra: &str) -> String {
    let default_root =
        if extra.contains("[agent_types.root]") || extra.contains("[agent_types.workspace]") {
            ""
        } else {
            "[agent_types.root]\nmodel = \"root-model\"\n"
        };
    format!(
        r#"
[project]
name = "s060-red"

{default_root}
{extra}
"#
    )
}

fn expect_invalid(source: &str, expected_fragments: &[&str]) {
    let error = parse_and_validate(source).expect_err("configuration should be rejected");
    for fragment in expected_fragments {
        assert!(
            error.contains(fragment),
            "error should contain {fragment:?}; got {error:?}"
        );
    }
}

#[test]
fn s060_a01_child_placement_parses_exact_field_set_and_rejects_unknown_fields() {
    let source = base_config(
        r#"
[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
skills = ["implementation", "review"]
max_turns = 7
max_tokens = 8192
max_cost = "12.50"
max_sub_agents = 2
allowed_child_placements = ["in_process"]

[child_placements.workspace.capabilities]
network = ["net:api.github.com"]
mcp = ["mcp:github:*"]
shell = true
javascript = true
python = false
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]
skill_patterns = ["skill:implementation"]

[child_placements.in_process]
model = "child-model"
"#,
    );
    let config = parse_and_validate(&source).expect("complete placement should parse");
    let value = serialized(&config);
    let placement = serialized_placement(&value, "workspace");

    let mut keys = placement.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "acp_profile",
            "allowed_child_placements",
            "backend",
            "capabilities",
            "max_cost",
            "max_sub_agents",
            "max_tokens",
            "max_turns",
            "skills",
        ]
    );
    assert_eq!(
        serialized_placement(&value, "in_process")
            .get("model")
            .and_then(toml::Value::as_str),
        Some("child-model"),
        "native placement model should deserialize independently of ACP fields"
    );
    assert_eq!(
        placement["skills"],
        toml::Value::Array(vec![
            toml::Value::String("implementation".into()),
            toml::Value::String("review".into()),
        ]),
        "the configured skill order is part of the placement contract"
    );
    assert_eq!(placement["max_turns"].as_integer(), Some(7));
    assert_eq!(placement["max_tokens"].as_integer(), Some(8192));
    assert_eq!(placement["max_cost"].as_str(), Some("12.50"));
    assert_eq!(placement["max_sub_agents"].as_integer(), Some(2));
    assert_eq!(
        placement["allowed_child_placements"],
        toml::Value::Array(vec![toml::Value::String("in_process".into())])
    );
    assert_eq!(placement["capabilities"]["shell"].as_bool(), Some(true));
    assert_eq!(
        placement["capabilities"]["javascript"].as_bool(),
        Some(true)
    );
    assert_eq!(placement["capabilities"]["python"].as_bool(), Some(false));
    assert_eq!(
        placement["capabilities"]["network"],
        toml::Value::Array(vec![toml::Value::String("net:api.github.com".into())])
    );
    assert_eq!(
        placement["capabilities"]["mcp"],
        toml::Value::Array(vec![toml::Value::String("mcp:github:*".into())])
    );
    assert_eq!(
        placement["capabilities"]["skill_patterns"],
        toml::Value::Array(vec![toml::Value::String("skill:implementation".into())])
    );

    expect_invalid(
        &base_config(
            r#"
[child_placements.workspace]
backend = "native"
model = "child-model"
invented = true
"#,
        ),
        &["child_placements.workspace.invented", "unknown"],
    );
}

#[test]
fn s060_a02_backend_defaults_to_native_and_unknown_backend_is_actionable() {
    let config = parse_and_validate(&base_config(
        r#"
[child_placements.in_process]
model = "child-model"
"#,
    ))
    .expect("omitted backend should resolve to native");
    let value = serialized(&config);
    assert_eq!(
        serialized_placement(&value, "in_process")
            .get("backend")
            .and_then(toml::Value::as_str),
        Some("native")
    );

    expect_invalid(
        &base_config(
            r#"
[child_placements.workspace]
backend = "remote"
acp_profile = "workspace-pod"
"#,
        ),
        &[
            "child_placements.workspace.backend",
            "remote",
            "native",
            "acp",
        ],
    );
}

#[test]
fn s060_a03_backend_specific_placement_requirements_are_actionable() {
    let cases = [
        (
            r#"[child_placements.worker]
backend = "native"
"#,
            "model",
            "native",
        ),
        (
            r#"[child_placements.worker]
backend = "native"
model = "child-model"
acp_profile = "workspace-pod"
"#,
            "acp_profile",
            "native",
        ),
        (
            r#"[child_placements.worker]
backend = "acp"
"#,
            "acp_profile",
            "acp",
        ),
        (
            r#"[child_placements.worker]
backend = "acp"
acp_profile = "workspace-pod"
model = "child-model"
"#,
            "model",
            "acp",
        ),
    ];

    for (placement, field, backend) in cases {
        expect_invalid(
            &base_config(placement),
            &["child_placements.worker", field, backend],
        );
    }

    for model_literal in [r#""""#, r#"" ""#, r#""\n\t""#, "\"\u{2003}\u{3000}\""] {
        expect_invalid(
            &base_config(&format!(
                "[child_placements.worker]\nbackend = \"native\"\nmodel = {model_literal}\n"
            )),
            &["child_placements.worker", "model", "native"],
        );
    }
    for acp_profile_literal in [r#""""#, r#"" ""#, r#""\n\t""#, "\"\u{2003}\u{3000}\""] {
        expect_invalid(
            &base_config(&format!(
                "[child_placements.worker]\nbackend = \"acp\"\nacp_profile = {acp_profile_literal}\n"
            )),
            &["child_placements.worker", "acp_profile", "acp"],
        );
    }

    parse_and_validate(&base_config(
        r#"
[child_placements.native_blank_opposite]
backend = "native"
model = "child-model"
acp_profile = "  "

[child_placements.acp_blank_opposite]
backend = "acp"
acp_profile = "workspace-pod"
model = "  "
"#,
    ))
    .expect("a blank backend-opposite field is semantically absent");
}

#[test]
fn s060_a04_placement_cannot_author_system_prompt_or_instructions() {
    for field in ["system_prompt", "instructions"] {
        expect_invalid(
            &base_config(&format!(
                r#"
[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
{field} = "do a particular job"
"#
            )),
            &[&format!("child_placements.workspace.{field}"), "unknown"],
        );
    }
}

#[test]
fn s060_a05_allowed_child_placements_populate_capabilities_without_legacy_aliases() {
    let config = parse_and_validate(&base_config(
        r#"
[agent_types.root]
model = "root-model"
allowed_child_placements = ["workspace"]

[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
allowed_child_placements = ["in_process"]

[child_placements.in_process]
model = "child-model"
"#,
    ))
    .expect("renamed authorization fields should parse");

    let root_token = toml::Value::try_from(build_capability_token(
        config.agent_types.get("root").expect("root agent type"),
    ))
    .expect("capability token should serialize");
    assert_eq!(
        root_token.get("spawn_placements"),
        Some(&toml::Value::Array(vec![toml::Value::String(
            "workspace".into()
        )]))
    );
    assert!(root_token.get("spawn_types").is_none());

    for legacy_field in ["can_spawn", "spawn_types"] {
        expect_invalid(
            &base_config(&format!(
                r#"
[agent_types.root]
model = "root-model"
{legacy_field} = ["workspace"]
"#
            )),
            &[&format!("agent_types.root.{legacy_field}"), "unknown"],
        );

        expect_invalid(
            &base_config(&format!(
                r#"
[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
{legacy_field} = ["in_process"]
"#
            )),
            &[
                &format!("child_placements.workspace.{legacy_field}"),
                "unknown",
            ],
        );
    }
}

#[test]
fn s060_a06_backend_fields_belong_only_to_child_placements() {
    for field_line in ["backend = \"acp\"", "acp_profile = \"workspace-pod\""] {
        let field = field_line.split_once(' ').expect("field assignment").0;
        expect_invalid(
            &base_config(&format!(
                r#"
[agent_types.root]
model = "root-model"
{field_line}
"#
            )),
            &[&format!("agent_types.root.{field}"), "unknown"],
        );
    }

    parse_and_validate(&base_config(
        r#"
[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
"#,
    ))
    .expect("ACP backend fields should validate on a child placement");
}

#[test]
fn s060_a07_same_named_agent_type_never_substitutes_for_a_child_placement() {
    let with_placement = parse_and_validate(&base_config(
        r#"
[agent_types.workspace]
model = "root-agent-model"

[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
"#,
    ))
    .expect("same-named root agent and placement should coexist");
    let with_value = serialized(&with_placement);
    assert_eq!(
        serialized_placement(&with_value, "workspace")
            .get("acp_profile")
            .and_then(toml::Value::as_str),
        Some("workspace-pod")
    );

    let without_placement = parse_and_validate(&base_config(
        r#"
[agent_types.workspace]
model = "root-agent-model"
"#,
    ))
    .expect("root agent alone remains valid");
    assert!(
        serialized(&without_placement)
            .get("child_placements")
            .and_then(|placements| placements.get("workspace"))
            .is_none(),
        "an agent type with the same key must not materialize a child placement"
    );
}
