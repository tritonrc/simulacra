fn child_cell_identity_config() -> SimulacraConfig {
    s060_parse_runtime_config(
        r#"
[project]
name = "child-cell-identity"

[agent_types.root]
model = "parent-model"
allowed_child_placements = ["workspace"]

[child_placements.workspace]
backend = "native"
model = "child-model"

[child_placements.workspace.capabilities]
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]
"#,
    )
}

fn completing_native_child_provider() -> FakeProvider {
    FakeProvider::new(vec![ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: "done".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: TokenUsage::default(),
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("child-cell-identity".into()),
        model: "child-model".into(),
    }])
}

fn child_cell_identity_capability() -> CapabilityToken {
    CapabilityToken {
        spawn_placements: vec!["workspace".into()],
        paths_read: vec![PathPattern("/workspace/**".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..CapabilityToken::default()
    }
}

struct ChildCellIdentityHarness {
    spawn: SpawnAgentTool,
    join: JoinChildAgentTool,
    capability: CapabilityToken,
    journal: Arc<InMemoryJournalStorage>,
    actor: tokio::task::JoinHandle<()>,
    sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

fn child_cell_identity_harness(
    child_cell_configurator: Option<simulacra_runtime::ChildCellConfigurator>,
    child_tool_registrar: Option<simulacra_runtime::ChildToolRegistrar>,
) -> ChildCellIdentityHarness {
    let capability = child_cell_identity_capability();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let parent_id = AgentId("root-cell-identity".into());
    let factory = Arc::new(AgentTaskFactory {
        config: child_cell_identity_config(),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: None,
        script_executor: None,
        child_cell_configurator,
        child_tool_registrar,
        child_provider_factory: Some(Arc::new(|_kind, _model| {
            Ok(Box::new(completing_native_child_provider()))
        })),
        acp_child_runtime: None,
    });
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        4_096,
        128,
        Decimal::ZERO,
        4,
    )));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        factory,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(parent_id.clone());
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    ChildCellIdentityHarness {
        spawn: SpawnAgentTool {
            sender: sender.clone(),
            allowed_placements: vec!["workspace".into()],
            activity_sink: Arc::new(NoopActivitySink),
            parent_id: parent_id.clone(),
            parent_budget: budget,
            guidance: None,
        },
        join: JoinChildAgentTool {
            sender: sender.clone(),
            caller_id: parent_id,
        },
        capability,
        journal,
        actor,
        sender,
    }
}

fn child_cell_identity_arguments(task: &str, task_name: &str) -> serde_json::Value {
    serde_json::json!({
        "placement": "workspace",
        "task": task,
        "task_name": task_name,
        "budget": s060_budget(32, 1, "0", 1)
    })
}

async fn spawn_native_child_and_join(
    harness: &ChildCellIdentityHarness,
    task: &str,
    task_name: &str,
) -> AgentId {
    let acknowledgement = harness
        .spawn
        .call(
            child_cell_identity_arguments(task, task_name),
            &harness.capability,
        )
        .await
        .expect("native spawn should be acknowledged");
    // The acknowledgement is the spawn-minted identity. Assertions compare
    // cell/journal attribution against this value rather than a second copy of
    // the task_name the caller supplied.
    let spawn_id = AgentId(
        acknowledgement
            .get("child_id")
            .and_then(|value| value.as_str())
            .expect("spawn acknowledgement should include child_id")
            .to_string(),
    );
    harness
        .join
        .call(
            serde_json::json!({ "child_id": spawn_id.0 }),
            &harness.capability,
        )
        .await
        .expect("native child should reach a terminal result");
    spawn_id
}

async fn finish_child_cell_identity_harness(harness: ChildCellIdentityHarness) {
    drop(harness.spawn);
    drop(harness.join);
    drop(harness.sender);
    harness.actor.await.expect("supervisor actor should stop");
}

fn journaled_file_writes_for(
    journal: &InMemoryJournalStorage,
    spawn_id: &AgentId,
    path: &str,
) -> Vec<JournalEntry> {
    // InMemoryJournalStorage filters by agent_id. Search both the minted spawn
    // id and the default cell identity so a misattributed write is still found
    // and the assertion can fail on the stamped value rather than a missing row.
    let default_cell_id = AgentId("sandbox".into());
    let mut entries = journal
        .read_all(spawn_id)
        .expect("spawn-id journal should be readable");
    entries.extend(
        journal
            .read_all(&default_cell_id)
            .expect("default cell journal should be readable"),
    );
    entries
        .into_iter()
        .filter(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::FileWrite {
                    path: written,
                    ..
                } if written == path
            )
        })
        .collect()
}

#[tokio::test]
async fn native_child_cell_reports_spawn_minted_id_inside_configurator() {
    let observed = Arc::new(Mutex::new(None::<AgentId>));
    let observed_for_configurator = Arc::clone(&observed);
    let harness = child_cell_identity_harness(
        Some(Arc::new(move |cell: &mut simulacra_sandbox::AgentCell| {
            *observed_for_configurator
                .lock()
                .expect("configurator cell id capture") = Some(cell.agent_id().clone());
        })),
        None,
    );

    let spawn_id = spawn_native_child_and_join(
        &harness,
        "stamp the child cell with this spawn identity",
        "cell_identity",
    )
    .await;
    let cell_id = observed
        .lock()
        .expect("configurator cell id capture")
        .clone()
        .expect("native child construction should invoke the cell configurator");

    assert_eq!(cell_id, spawn_id);

    finish_child_cell_identity_harness(harness).await;
}

#[tokio::test]
async fn native_child_cell_journals_file_writes_under_spawn_minted_id() {
    const WRITE_PATH: &str = "/workspace/child-cell-attribution.txt";
    let harness = child_cell_identity_harness(
        Some(Arc::new(|cell: &mut simulacra_sandbox::AgentCell| {
            cell.write_file(WRITE_PATH, b"attributed")
                .expect("child placement grants this workspace write");
        })),
        None,
    );

    let spawn_id = spawn_native_child_and_join(
        &harness,
        "journal a sandbox write through the child cell",
        "journal_attribution",
    )
    .await;
    let writes = journaled_file_writes_for(&harness.journal, &spawn_id, WRITE_PATH);
    assert_eq!(
        writes.len(),
        1,
        "the child cell should journal exactly one write for {WRITE_PATH}"
    );
    let journaled_id = writes[0].agent_id.clone();

    assert_eq!(journaled_id, spawn_id);

    finish_child_cell_identity_harness(harness).await;
}

#[tokio::test]
async fn shared_child_tool_registrar_observes_each_native_childs_own_cell_id() {
    let observed = Arc::new(Mutex::new(Vec::<AgentId>::new()));
    let observed_for_registrar = Arc::clone(&observed);
    let harness = child_cell_identity_harness(
        None,
        Some(Arc::new(move |_registry, cell| {
            observed_for_registrar
                .lock()
                .expect("registrar cell id capture")
                .push(cell.agent_id().clone());
            Ok(())
        })),
    );

    let first_spawn_id = spawn_native_child_and_join(
        &harness,
        "first child whose registrar should see its own id",
        "registrar_alpha",
    )
    .await;
    let second_spawn_id = spawn_native_child_and_join(
        &harness,
        "second child whose registrar should see its own id",
        "registrar_beta",
    )
    .await;
    let observed_ids = observed.lock().expect("registrar cell id capture").clone();
    assert_eq!(
        observed_ids.len(),
        2,
        "the shared registrar should run once per native child"
    );

    assert_eq!(observed_ids[0], first_spawn_id);
    assert_eq!(observed_ids[1], second_spawn_id);

    finish_child_cell_identity_harness(harness).await;
}

#[tokio::test]
async fn sibling_native_child_cells_carry_distinct_ids_matching_their_spawn_ids() {
    let observed = Arc::new(Mutex::new(Vec::<AgentId>::new()));
    let observed_for_configurator = Arc::clone(&observed);
    let harness = child_cell_identity_harness(
        Some(Arc::new(move |cell: &mut simulacra_sandbox::AgentCell| {
            observed_for_configurator
                .lock()
                .expect("sibling cell id capture")
                .push(cell.agent_id().clone());
        })),
        None,
    );

    let first_spawn_id = spawn_native_child_and_join(
        &harness,
        "first sibling in the same supervision tree",
        "sibling_alpha",
    )
    .await;
    let second_spawn_id = spawn_native_child_and_join(
        &harness,
        "second sibling in the same supervision tree",
        "sibling_beta",
    )
    .await;
    let cell_ids = observed.lock().expect("sibling cell id capture").clone();
    assert_eq!(
        cell_ids.len(),
        2,
        "each sibling spawn should configure one cell"
    );

    assert_eq!(cell_ids[0], first_spawn_id);
    assert_eq!(cell_ids[1], second_spawn_id);
    assert_ne!(first_spawn_id, second_spawn_id);
    assert_ne!(cell_ids[0], cell_ids[1]);

    finish_child_cell_identity_harness(harness).await;
}

#[test]
fn directly_constructed_agent_cell_reports_sandbox_and_set_id_updates_journal_attribution() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let budget = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let mut cell = simulacra_sandbox::AgentCell::new(
        vfs,
        CapabilityToken {
            paths_write: vec![PathPattern("/**".into())],
            ..Default::default()
        },
        budget,
        Arc::clone(&journal) as Arc<dyn JournalStorage>,
        http_client,
    );

    assert_eq!(cell.agent_id(), &AgentId("sandbox".into()));
    cell.write_file("/sandbox-default.txt", b"default")
        .expect("unrestricted write grant should allow the default-identity write");
    let default_entries = journal
        .read_all(&AgentId("sandbox".into()))
        .expect("default cell journal should be readable");
    assert!(default_entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, .. } if path == "/sandbox-default.txt"
        )
    }));

    let updated_id = AgentId("/forge/explicit_cell".into());
    cell.set_agent_id(updated_id.clone());
    assert_eq!(cell.agent_id(), &updated_id);
    cell.write_file("/sandbox-updated.txt", b"updated")
        .expect("unrestricted write grant should allow the updated-identity write");

    let updated_entries = journal
        .read_all(&updated_id)
        .expect("updated cell journal should be readable");
    assert!(updated_entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, .. } if path == "/sandbox-updated.txt"
        )
    }));
    let default_after = journal
        .read_all(&AgentId("sandbox".into()))
        .expect("default cell journal should remain readable");
    assert!(!default_after.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, .. } if path == "/sandbox-updated.txt"
        )
    }));
}
