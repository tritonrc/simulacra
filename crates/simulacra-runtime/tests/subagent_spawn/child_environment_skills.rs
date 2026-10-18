// A host keeps one task factory across many wakes while its skill shelf
// changes, so the environment it answers with can name the skills this spawn
// may discover. "Offered" is read off the child's own first model request: the
// `Skill` tool definition its provider was handed.
#[allow(dead_code)] // the shared support file serves more tests than this module uses
mod child_environment_skills {
    use super::*;
    include!("child_environment_support.rs");

    fn skills_config(placement_skills: &[&str], capability_line: &str) -> SimulacraConfig {
        let listed = placement_skills
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ");
        s060_parse_runtime_config(&format!(
            r#"
[project]
name = "child-environment-skills"

[agent_types.root]
model = "parent-model"
allowed_child_placements = ["workspace"]

[child_placements.workspace]
backend = "native"
model = "child-model"
skills = [{listed}]

[child_placements.workspace.capabilities]
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]
{capability_line}
"#
        ))
    }

    /// A filesystem whose shelf holds exactly these skills.
    fn shelf_with(names: &[&str]) -> Arc<dyn VirtualFs> {
        let vfs = child_env_vfs_with_marker("shelf");
        vfs.mkdir("/skills").expect("skills root should be created");
        for name in names {
            vfs.mkdir(&format!("/skills/{name}"))
                .expect("skill directory should be created");
            vfs.write(
                &format!("/skills/{name}/SKILL.md"),
                format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\nBody of {name}.\n")
                    .as_bytes(),
            )
            .expect("skill should be written");
        }
        vfs
    }

    struct SkillsRun {
        joined: serde_json::Value,
        requests: usize,
        skill_tool_descriptions: Vec<String>,
    }

    /// Runs one child under a placement listing `placement_skills`, whose host
    /// answers with `shelf` as `/skills` and `answer` as the skills override.
    async fn run_child(
        placement_skills: &[&str],
        capability_line: &str,
        shelf: &[&str],
        answer: Option<Vec<String>>,
    ) -> SkillsRun {
        let seen: ChildEnvRequests = Arc::default();
        let tools: ChildEnvTools = Arc::default();
        let shelf_vfs = shelf_with(shelf);
        let harness = child_env_harness_with_config(
            skills_config(placement_skills, capability_line),
            Some(child_env_provider(move |_spawn| {
                Ok(simulacra_runtime::ChildFsEnvironment {
                    vfs: Arc::clone(&shelf_vfs),
                    file_presenter: None,
                    skills: answer.clone(),
                })
            })),
            child_env_vfs_with_marker("factory-shared"),
            child_env_read_marker,
            Arc::clone(&seen),
            Arc::clone(&tools),
        );
        let (_id, joined) = child_env_spawn_and_join(&harness, "skills").await;
        finish_child_cell_identity_harness(harness).await;
        let requests = seen.lock().unwrap().len();
        let skill_tool_descriptions = tools
            .lock()
            .unwrap()
            .first()
            .map(|first| {
                first
                    .iter()
                    .filter(|tool| tool.name == "Skill")
                    .map(|tool| tool.description.clone())
                    .collect()
            })
            .unwrap_or_default();
        SkillsRun {
            joined,
            requests,
            skill_tool_descriptions,
        }
    }

    #[tokio::test]
    async fn a_provider_skills_list_replaces_the_placements_and_is_what_the_child_is_offered() {
        let run = run_child(
            &["stale"],
            "",
            &["fresh", "other"],
            Some(vec!["fresh".into()]),
        ).await;

        assert_eq!(run.joined["status"], "completed", "got {}", run.joined);
        assert!(run.requests > 0, "the child should have reached its model");
        assert_eq!(run.skill_tool_descriptions.len(), 1);
        let offered = &run.skill_tool_descriptions[0];
        assert!(offered.contains("- fresh: The fresh skill."), "got {offered}");
        assert!(!offered.contains("stale"), "got {offered}");
        assert!(!offered.contains("other"), "got {offered}");
    }

    #[tokio::test]
    async fn without_a_provider_skills_list_the_placements_list_still_has_to_be_discoverable() {
        let run = run_child(&["stale"], "", &["fresh"], None).await;

        assert_eq!(run.joined["status"], "failed", "got {}", run.joined);
        let message = run.joined["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("stale") && message.contains("no valid /skills/stale/SKILL.md"),
            "the failure should name the placement's undiscoverable skill, got {}",
            run.joined
        );
        assert_eq!(run.requests, 0, "the child must never reach its model");
    }

    #[tokio::test]
    async fn an_empty_provider_skills_list_offers_the_child_no_skills() {
        let run = run_child(&["fresh"], "", &["fresh"], Some(vec![])).await;

        assert_eq!(run.joined["status"], "completed", "got {}", run.joined);
        assert!(run.requests > 0, "the child should have reached its model");
        assert!(
            run.skill_tool_descriptions.is_empty(),
            "no Skill tool should be offered, got {:?}",
            run.skill_tool_descriptions
        );
    }

    #[tokio::test]
    async fn a_provider_skills_list_is_still_filtered_by_the_placements_skill_patterns() {
        let run = run_child(
            &["stale"],
            r#"skill_patterns = ["skill:allowed"]"#,
            &["allowed", "denied"],
            Some(vec!["allowed".into(), "denied".into()]),
        )
        .await;

        assert_eq!(run.joined["status"], "completed", "got {}", run.joined);
        assert_eq!(run.skill_tool_descriptions.len(), 1);
        let offered = &run.skill_tool_descriptions[0];
        assert!(offered.contains("- allowed: The allowed skill."), "got {offered}");
        assert!(!offered.contains("denied"), "got {offered}");
    }
}
