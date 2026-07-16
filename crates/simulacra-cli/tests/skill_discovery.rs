use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rust_decimal::Decimal;
use serde_json::json;
use simulacra_cli::interactive::{
    InteractiveInput, InteractiveOutput, InteractiveSession, InteractiveSessionConfig,
};
use simulacra_cli::{CliArgs, CliMode, bootstrap};
use simulacra_runtime::{InMemoryJournalStorage, InMemorySessionStorage, SessionStorage};
use simulacra_sandbox::AgentCell;
use simulacra_tool::{
    SkillError, SkillMeta, SkillTool, Tool, discover_and_filter_skills, parse_skill_frontmatter,
};
use simulacra_types::{
    CapabilityToken, FinishReason, JournalStorage, Message, PathPattern, Provider, ProviderError,
    ProviderResponse, ResourceBudget, Role, TokenUsage, ToolDefinition, VirtualFs,
};
use simulacra_vfs::MemoryFs;

#[derive(Debug)]
struct FakeProvider;

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut simulacra_types::ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async {
            Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    content: "ok".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_content: vec![],
                },
                token_usage: TokenUsage::default(),
                finish_reason: FinishReason::EndTurn,
                provider_response_id: Some("resp-1".into()),
                model: "claude-sonnet-4-20250514".into(),
            })
        })
    }
}

#[derive(Debug, Default, Clone)]
struct TestIo {
    tty: bool,
    writes: Vec<String>,
}

impl TestIo {
    fn tty() -> Self {
        Self {
            tty: true,
            ..Self::default()
        }
    }
}

impl InteractiveInput for TestIo {
    fn read_line(&mut self) -> Option<String> {
        None
    }

    fn read_approval(&mut self) -> Option<String> {
        None
    }

    fn is_tty(&self) -> bool {
        self.tty
    }
}

impl InteractiveOutput for TestIo {
    fn write_line(&mut self, line: &str) {
        self.writes.push(line.to_string());
    }

    fn clear(&mut self) {}

    fn restore_terminal(&mut self) {}
}

fn contains_text(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

fn make_session(tool_definitions: Vec<ToolDefinition>) -> InteractiveSession<FakeProvider, TestIo> {
    make_session_with_skills(tool_definitions, vec![])
}

fn make_session_with_skills(
    tool_definitions: Vec<ToolDefinition>,
    skill_catalog: Vec<SkillMeta>,
) -> InteractiveSession<FakeProvider, TestIo> {
    let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    InteractiveSession::new(
        TestIo::tty(),
        Arc::new(FakeProvider),
        storage,
        vfs,
        InteractiveSessionConfig {
            project_name: "simulacra-s017".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 8_192,
            max_turns: 7,
            task: Some("skills red test".into()),
            requested_session_id: None,
            tool_definitions,
            can_spawn: vec![],
            skill_catalog,
        },
    )
}

fn unique_path(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "simulacra-cli-s017-{name}-{stamp}-{}.toml",
        std::process::id()
    ))
}

struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn write(contents: &str) -> Self {
        let path = unique_path("config");
        fs::write(&path, contents).expect("temp config should be written");
        Self { path }
    }

    fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn toml_array(values: &[&str]) -> String {
    let joined = values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{joined}]")
}

fn config_with_skills(default_skills: &[&str], reviewer_skills: &[&str]) -> String {
    let reviewer_block = if reviewer_skills.is_empty() {
        String::new()
    } else {
        format!(
            r#"
[agent_types.reviewer]
model = "gpt-5.4"
skills = {}
max_turns = 3
max_tokens = 1024

[agent_types.reviewer.capabilities]
paths_read = ["/workspace/**"]
"#,
            toml_array(reviewer_skills)
        )
    };

    format!(
        r#"[project]
name = "simulacra-s017"

[agent_types.default]
model = "claude-sonnet-4-20250514"
skills = {}
max_turns = 7
max_tokens = 4321

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]
{}

[task]
entry_agent = "default"
task = "exercise skills"
"#,
        toml_array(default_skills),
        reviewer_block
    )
}

fn skill_markdown(name: &str, description: &str, extra_frontmatter: &str, body: &str) -> String {
    let mut frontmatter =
        format!("---\nname: {name}\ndescription: {description}\n{extra_frontmatter}---\n\n");
    if !extra_frontmatter.is_empty() && !extra_frontmatter.ends_with('\n') {
        frontmatter =
            format!("---\nname: {name}\ndescription: {description}\n{extra_frontmatter}\n---\n\n");
    }
    format!("{frontmatter}{body}")
}

fn make_skill_meta(
    name: &str,
    description: &str,
    vfs_path: &str,
    disable_model_invocation: bool,
    user_invocable: bool,
    allowed_tools: &[&str],
) -> SkillMeta {
    SkillMeta {
        name: name.into(),
        description: description.into(),
        vfs_path: vfs_path.into(),
        disable_model_invocation,
        allow_implicit_invocation: true,
        user_invocable,
        allowed_tools: allowed_tools
            .iter()
            .map(|tool| (*tool).to_string())
            .collect(),
        body: None,
    }
}

fn make_skill_tool(skill_docs: &[(&str, &str)], catalog: Vec<SkillMeta>) -> SkillTool {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    for (path, contents) in skill_docs {
        vfs.write(path, contents.as_bytes())
            .expect("skill fixture should be written to MemoryFs");
    }

    let budget = Arc::new(Mutex::new(ResourceBudget::new(8_192, 7, Decimal::ZERO, 0)));
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        Arc::clone(&vfs),
        CapabilityToken {
            paths_read: vec![PathPattern("/skills/**".into())],
            ..Default::default()
        },
        budget,
        journal,
        http_client,
    ));

    SkillTool::new(cell, catalog)
}

#[test]
fn agent_type_referencing_an_undiscoverable_skill_fails_startup_with_the_missing_name() {
    let config = TempConfig::write(&config_with_skills(&["definitely-missing-s017-skill"], &[]));

    let result = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: Some("exercise skills".into()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
        output_format: simulacra_cli::OutputFormat::Text,
    });

    let error = match result {
        Ok(_) => {
            panic!("bootstrap should fail when an agent type references an undiscoverable skill")
        }
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("default") && message.contains("definitely-missing-s017-skill"),
        "startup error should name both the agent type and the missing skill, got: {message}"
    );
}

#[test]
fn built_in_slash_commands_take_precedence_over_skill_names() {
    let mut session = make_session(Vec::new());

    let view = session.dispatch_command("/help");

    assert!(
        contains_text(&view.visible_output, "/help")
            && contains_text(&view.visible_output, "/tools"),
        "S017 requires built-in slash commands from S015 to win before skill-name resolution"
    );
}

#[test]
fn unknown_skill_names_fall_through_to_the_existing_unknown_command_path() {
    let mut session = make_session(Vec::new());

    let view = session.dispatch_command("/definitely-missing-s017-skill");

    assert_eq!(
        view.error.as_deref(),
        Some("unknown command: /definitely-missing-s017-skill. Type /help for available commands."),
        "S017 requires unknown skill names to fall through to the S015 unknown-command path"
    );
}

#[test]
fn parse_skill_frontmatter_rejects_skill_files_without_a_markdown_body() {
    let error = parse_skill_frontmatter(
        "---\nname: rust-dev\ndescription: Use cargo safely.\n---\n",
        "/skills/rust-dev/SKILL.md",
    )
    .expect_err("a valid skill requires YAML frontmatter plus a markdown body");

    assert!(
        error.contains("markdown body"),
        "missing-body parse errors should explain that SKILL.md needs a markdown body, got: {error}"
    );
}

#[test]
fn parse_skill_frontmatter_uses_frontmatter_name_as_the_canonical_identifier() {
    let meta = parse_skill_frontmatter(
        &skill_markdown(
            "rust-dev",
            "Use cargo and clippy safely.",
            "user_invocable: false\nallowed_tools:\n  - file_read\n  - shell_exec\n",
            "Read Cargo.toml before editing.",
        ),
        "/skills/not-the-name/SKILL.md",
    )
    .expect("frontmatter fixture should parse");

    assert_eq!(meta.name, "rust-dev");
    assert_eq!(meta.description, "Use cargo and clippy safely.");
    assert_eq!(meta.vfs_path, "/skills/not-the-name/SKILL.md");
    assert!(!meta.user_invocable);
    assert_eq!(meta.allowed_tools, vec!["file_read", "shell_exec"]);
}

#[test]
fn discovery_accepts_immediate_skill_children_under_skills() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.write(
        "/skills/rust-dev/SKILL.md",
        skill_markdown(
            "rust-dev",
            "Use cargo safely.",
            "",
            "Run cargo test before returning.",
        )
        .as_bytes(),
    )
    .expect("skill fixture should be written");

    let catalog = discover_and_filter_skills(
        &vfs,
        &["rust-dev".to_string()],
        &CapabilityToken::default(),
        "default",
    )
    .expect("immediate skill children should be discoverable");

    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].name, "rust-dev");
    assert_eq!(catalog[0].vfs_path, "/skills/rust-dev/SKILL.md");
}

#[test]
fn discovery_accepts_nested_skill_directories_under_skills_root() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.write(
        "/skills/group/rust-dev/SKILL.md",
        skill_markdown(
            "rust-dev",
            "Use cargo safely.",
            "",
            "Run cargo test before returning.",
        )
        .as_bytes(),
    )
    .expect("nested skill fixture should be written");

    let catalog = discover_and_filter_skills(
        &vfs,
        &["rust-dev".to_string()],
        &CapabilityToken::default(),
        "default",
    )
    .expect("nested /skills/<group>/<dir>/SKILL.md should be discoverable");

    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].name, "rust-dev");
    assert_eq!(catalog[0].vfs_path, "/skills/group/rust-dev/SKILL.md");
}

#[test]
fn discovery_does_not_walk_up_or_search_for_other_skills_directories() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.write(
        "/workspace/project/skills/rust-dev/SKILL.md",
        skill_markdown(
            "rust-dev",
            "Use cargo safely.",
            "",
            "Run cargo test before returning.",
        )
        .as_bytes(),
    )
    .expect("non-root skill fixture should be written");

    let error = discover_and_filter_skills(
        &vfs,
        &["rust-dev".to_string()],
        &CapabilityToken::default(),
        "default",
    )
    .expect_err("discovery must stay rooted at /skills");

    assert!(
        matches!(
            &error,
            SkillError::UndiscoverableSkill {
                agent_type,
                skill
            } if agent_type == "default" && skill == "rust-dev"
        ),
        "skills outside the mounted /skills root should not be discovered, got: {error:?}"
    );
}

#[test]
fn skill_tool_definition_excludes_implicit_disabled_skills_from_catalog() {
    let mut user_only = make_skill_meta(
        "user-only",
        "Users may load this explicitly.",
        "/skills/user-only/SKILL.md",
        false,
        true,
        &[],
    );
    user_only.allow_implicit_invocation = false;
    let tool = make_skill_tool(&[], vec![user_only]);

    let definition = tool.definition();

    assert!(
        !definition.description.contains("user-only"),
        "allow_implicit_invocation=false skills should not be advertised to the model"
    );
}

#[test]
fn skill_tool_definition_includes_only_name_and_description_for_model_visible_skills() {
    let tool = make_skill_tool(
        &[],
        vec![
            make_skill_meta(
                "rust-dev",
                "Use cargo, rustfmt, and clippy.",
                "/skills/rust-dir/SKILL.md",
                false,
                true,
                &["shell_exec"],
            ),
            make_skill_meta(
                "review-only",
                "Review changes carefully.",
                "/skills/review-only/SKILL.md",
                false,
                false,
                &["file_read"],
            ),
            make_skill_meta(
                "internal-only",
                "Should not be visible to the model.",
                "/skills/internal-only/SKILL.md",
                true,
                true,
                &["shell_exec"],
            ),
        ],
    );

    let definition = tool.definition();

    assert_eq!(definition.name, "Skill");
    assert!(
        definition
            .description
            .contains("rust-dev: Use cargo, rustfmt, and clippy.")
            && definition
                .description
                .contains("review-only: Review changes carefully."),
        "the model-visible catalog should surface skill name + description metadata"
    );
    assert!(
        !definition.description.contains("internal-only")
            && !definition.description.contains("allowed_tools")
            && !definition
                .description
                .contains("Read Cargo.toml before editing."),
        "Skill::definition must not leak model-disabled skills, allowed_tools, or SKILL.md bodies"
    );
    assert_eq!(definition.input_schema["required"], json!(["command"]));
    assert_eq!(
        definition.input_schema["additionalProperties"],
        json!(false)
    );
}

#[tokio::test]
async fn skill_tool_returns_an_error_tool_result_for_unknown_skills() {
    let tool = make_skill_tool(&[], vec![]);

    let result = tool
        .call(
            json!({"command": "does-not-exist"}),
            &CapabilityToken::default(),
        )
        .await
        .expect("unknown skills should surface as an error tool result, not as a Rust Err");

    assert_eq!(
        result,
        json!({
            "is_error": true,
            "content": "unknown skill: \"does-not-exist\""
        }),
        "unknown Skill calls should return a descriptive error tool result"
    );
}

#[tokio::test]
async fn skill_tool_returns_an_error_tool_result_when_capability_policy_denies_the_skill() {
    let skill_path = "/skills/rust-dev/SKILL.md";
    let tool = make_skill_tool(
        &[(
            skill_path,
            &skill_markdown(
                "rust-dev",
                "Use cargo safely.",
                "",
                "Run cargo fmt before returning.",
            ),
        )],
        vec![make_skill_meta(
            "rust-dev",
            "Use cargo safely.",
            skill_path,
            false,
            true,
            &["shell_exec"],
        )],
    );

    let result = tool
        .call(
            json!({"command": "rust-dev"}),
            &CapabilityToken {
                skill_patterns: vec!["skill:review-*".into()],
                ..Default::default()
            },
        )
        .await
        .expect(
            "capability-denied Skill calls should be returned to the model as error tool results",
        );

    let content = result
        .get("content")
        .and_then(|value| value.as_str())
        .expect("error tool results should carry a string content field");
    assert_eq!(result.get("is_error"), Some(&json!(true)));
    assert!(
        content.contains("skill \"rust-dev\" not allowed by capability token"),
        "capability denials should preserve the denial reason, got: {content}"
    );
}

#[tokio::test]
async fn skill_tool_returns_an_error_tool_result_for_model_disabled_skills() {
    let skill_path = "/skills/internal/SKILL.md";
    let tool = make_skill_tool(
        &[(
            skill_path,
            &skill_markdown(
                "internal-only",
                "Only users may load this skill.",
                "disable_model_invocation: true\n",
                "Never expose this body to the model.",
            ),
        )],
        vec![make_skill_meta(
            "internal-only",
            "Only users may load this skill.",
            skill_path,
            true,
            true,
            &[],
        )],
    );

    let result = tool
        .call(
            json!({"command": "internal-only"}),
            &CapabilityToken::default(),
        )
        .await
        .expect("model-disabled Skill calls should produce an error tool result instead of Err");

    assert_eq!(result.get("is_error"), Some(&json!(true)));
    assert!(
        result["content"]
            .as_str()
            .expect("error content should be a string")
            .contains("disable_model_invocation=true"),
        "the tool result should explain why model invocation is blocked"
    );
}

#[tokio::test]
async fn skill_tool_returns_only_the_skill_markdown_body_without_frontmatter() {
    let skill_path = "/skills/rust-dev/SKILL.md";
    let tool = make_skill_tool(
        &[(
            skill_path,
            &skill_markdown(
                "rust-dev",
                "Use cargo safely.",
                "allowed_tools:\n  - shell_exec\n",
                "Read reference.md explicitly before editing.\nDo not assume it is preloaded.",
            ),
        )],
        vec![make_skill_meta(
            "rust-dev",
            "Use cargo safely.",
            skill_path,
            false,
            true,
            &["shell_exec"],
        )],
    );

    let result = tool
        .call(json!({"command": "rust-dev"}), &CapabilityToken::default())
        .await
        .expect("known skills should load successfully");

    assert_eq!(
        result,
        json!("Read reference.md explicitly before editing.\nDo not assume it is preloaded."),
        "Skill(command=...) should return the markdown body only, with YAML frontmatter removed"
    );
}

#[tokio::test]
async fn multiple_skill_calls_in_one_turn_can_load_multiple_bodies_independently() {
    let first_path = "/skills/rust-dev/SKILL.md";
    let second_path = "/skills/reviewer/SKILL.md";
    let tool = make_skill_tool(
        &[
            (
                first_path,
                &skill_markdown(
                    "rust-dev",
                    "Use cargo safely.",
                    "",
                    "Implement the Rust change and run cargo fmt.",
                ),
            ),
            (
                second_path,
                &skill_markdown(
                    "reviewer",
                    "Review changes carefully.",
                    "",
                    "Review the diff for correctness and risk.",
                ),
            ),
        ],
        vec![
            make_skill_meta(
                "rust-dev",
                "Use cargo safely.",
                first_path,
                false,
                true,
                &["shell_exec"],
            ),
            make_skill_meta(
                "reviewer",
                "Review changes carefully.",
                second_path,
                false,
                true,
                &["file_read"],
            ),
        ],
    );

    let first = tool
        .call(json!({"command": "rust-dev"}), &CapabilityToken::default())
        .await
        .expect("first skill load should succeed");
    let second = tool
        .call(json!({"command": "reviewer"}), &CapabilityToken::default())
        .await
        .expect("second skill load should succeed");

    assert_eq!(first, json!("Implement the Rust change and run cargo fmt."));
    assert_eq!(second, json!("Review the diff for correctness and risk."));
}

#[test]
fn slash_skill_invocation_injects_the_skill_body_and_preserves_trailing_args_for_the_next_turn() {
    let mut skill = make_skill_meta(
        "rust-dev",
        "Use cargo safely.",
        "/skills/rust-dir/SKILL.md",
        false,
        true,
        &["shell_exec"],
    );
    skill.body = Some("Implement the Rust change and run cargo fmt.".into());
    let mut session = make_session_with_skills(Vec::new(), vec![skill]);

    let view = session.dispatch_command("/rust-dev fix the failing clippy lint");

    assert!(
        view.messages.iter().any(|message| {
            message.role == Role::User
                && message
                    .content
                    .contains("Implement the Rust change and run cargo fmt.")
        }),
        "/skill-name should inject the resolved skill body into the upcoming turn context"
    );
    assert!(
        view.messages.iter().any(|message| {
            message.role == Role::User && message.content == "fix the failing clippy lint"
        }),
        "the trailing args after /skill-name should be forwarded as the user's instruction"
    );
}

#[test]
fn non_user_invocable_skill_names_fall_through_to_the_unknown_command_path() {
    let mut session = make_session_with_skills(
        Vec::new(),
        vec![make_skill_meta(
            "internal-only",
            "Users must not invoke this directly.",
            "/skills/internal-only/SKILL.md",
            false,
            false,
            &[],
        )],
    );

    let view = session.dispatch_command("/internal-only");

    assert_eq!(
        view.error.as_deref(),
        Some("unknown command: /internal-only. Type /help for available commands."),
        "user_invocable: false skills must fall through to the existing unknown-command behavior"
    );
}

#[test]
fn child_skill_patterns_can_attenuate_from_a_parent_prefix_wildcard_to_an_exact_skill() {
    let parent = CapabilityToken {
        skill_patterns: vec!["skill:rust-*".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        skill_patterns: vec!["skill:rust-dev".into()],
        ..Default::default()
    };

    assert!(
        child.is_subset_of(&parent),
        "skill capability attenuation should accept an exact child skill covered by a parent wildcard"
    );
}

#[test]
fn child_skill_patterns_can_attenuate_from_the_global_skill_wildcard_to_an_exact_skill() {
    let parent = CapabilityToken {
        skill_patterns: vec!["skill:*".into()],
        ..Default::default()
    };
    let child = CapabilityToken {
        skill_patterns: vec!["skill:reviewer".into()],
        ..Default::default()
    };

    assert!(
        child.is_subset_of(&parent),
        "skill capability attenuation should accept an exact child skill covered by skill:*"
    );
}

#[test]
fn skill_frontmatter_rejects_a_non_array_mcp_servers_dependency() {
    let error = parse_skill_frontmatter(
        &skill_markdown(
            "repo-work",
            "Work with repository issues.",
            "mcp_servers: github\n",
            "Use the repository catalog.",
        ),
        "/skills/repo-work/SKILL.md",
    )
    .expect_err("mcp_servers must be an array of configured server names");

    assert!(
        error.contains("mcp_servers"),
        "the invalid dependency error should name mcp_servers, got: {error}"
    );
}

#[test]
fn skill_frontmatter_rejects_non_string_mcp_server_dependencies() {
    let error = parse_skill_frontmatter(
        &skill_markdown(
            "repo-work",
            "Work with repository issues.",
            "mcp_servers:\n  - github\n  - 42\n",
            "Use the repository catalog.",
        ),
        "/skills/repo-work/SKILL.md",
    )
    .expect_err("every mcp_servers dependency must be a server-name string");

    assert!(
        error.contains("mcp_servers"),
        "the invalid dependency error should name mcp_servers, got: {error}"
    );
}

#[test]
fn skill_frontmatter_rejects_blank_mcp_server_dependencies() {
    let error = parse_skill_frontmatter(
        &skill_markdown(
            "repo-work",
            "Work with repository issues.",
            "mcp_servers:\n  - github\n  - '   '\n",
            "Use the repository catalog.",
        ),
        "/skills/repo-work/SKILL.md",
    )
    .expect_err("blank configured MCP server names must invalidate the skill");

    assert!(
        error.contains("mcp_servers"),
        "the invalid dependency error should name mcp_servers, got: {error}"
    );
}
