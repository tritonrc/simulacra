use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use simulacra_config::AgentBackend;
#[cfg(feature = "spawn")]
use simulacra_runtime::SpawnAgentTool;
use simulacra_runtime::{
    AgentLoopOutput, AgentSupervisor, BoxTaskFuture, CancellationToken, InMemoryJournalStorage,
    RestartStrategy, RuntimeError, SpawnConfig, TaskFactory,
};
use simulacra_types::{
    ActivityEvent, AgentId, CapabilityToken, ExitReason, JournalEntry, JournalStorage,
    ResourceBudget, TokenUsage,
};
#[cfg(feature = "spawn")]
use simulacra_types::{JournalEntryKind, Tool, ToolError};

const CONFIGURED_PLACEMENT: &str = "in_process";
const UNCONFIGURED_PLACEMENT: &str = "arbitrary-unconfigured";

fn parent_capability(placement: &str) -> CapabilityToken {
    CapabilityToken {
        spawn_placements: vec![placement.to_string()],
        ..Default::default()
    }
}

fn parent_budget() -> ResourceBudget {
    ResourceBudget::new(100, 10, Decimal::new(10, 0), 4)
}

fn child_budget() -> ResourceBudget {
    ResourceBudget::new(10, 1, Decimal::new(1, 0), 1)
}

fn assert_budget_unchanged(actual: &ResourceBudget, initial: &ResourceBudget) {
    assert_eq!(
        serde_json::to_value(actual).expect("actual budget should serialize"),
        serde_json::to_value(initial).expect("initial budget should serialize"),
        "rejected spawn must not reserve or consume parent budget"
    );
}

fn spawn_config(child_id: &str, parent_id: &str, placement: &str) -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId(child_id.to_string()),
        parent_id: AgentId(parent_id.to_string()),
        capability: None,
        budget: child_budget(),
        restart_strategy: RestartStrategy::LetCrash,
        placement: placement.to_string(),
        task: "preserve this task".to_string(),
        instructions: Some("preserve these instructions".to_string()),
    }
}

#[cfg(feature = "spawn")]
fn spawn_arguments(placement: &str) -> serde_json::Value {
    serde_json::json!({
        "placement": placement,
        "instructions": "preserve these instructions",
        "task": "preserve this task",
        "budget": {
            "max_tokens": 10,
            "max_turns": 1,
            "max_cost": "1",
            "max_sub_agents": 1
        }
    })
}

fn completed_output() -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: Vec::new(),
        token_usage: TokenUsage::default(),
        reported_tool_uses: None,
        used_turns: 0,
        used_cost: Decimal::ZERO,
    }
}

#[derive(Default)]
#[cfg(feature = "spawn")]
struct DefaultOnlyFactory {
    create_calls: Arc<AtomicUsize>,
}

#[cfg(feature = "spawn")]
impl TaskFactory for DefaultOnlyFactory {
    fn create_task(&self, _config: SpawnConfig, _cancellation: CancellationToken) -> BoxTaskFuture {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(completed_output()) })
    }
}

#[derive(Clone)]
struct ConfigBackedFactory {
    configured_placement: &'static str,
    validate_calls: Arc<AtomicUsize>,
    prepare_calls: Arc<AtomicUsize>,
    create_calls: Arc<AtomicUsize>,
    after_calls: Arc<AtomicUsize>,
    runtime_finished: Arc<AtomicBool>,
    journal: Option<Arc<dyn JournalStorage>>,
    journal_at_create: Arc<Mutex<Vec<JournalEntry>>>,
    parent_id: AgentId,
}

impl ConfigBackedFactory {
    fn new(journal: Option<Arc<dyn JournalStorage>>, parent_id: AgentId) -> Self {
        Self {
            configured_placement: CONFIGURED_PLACEMENT,
            validate_calls: Arc::new(AtomicUsize::new(0)),
            prepare_calls: Arc::new(AtomicUsize::new(0)),
            create_calls: Arc::new(AtomicUsize::new(0)),
            after_calls: Arc::new(AtomicUsize::new(0)),
            runtime_finished: Arc::new(AtomicBool::new(false)),
            journal,
            journal_at_create: Arc::new(Mutex::new(Vec::new())),
            parent_id,
        }
    }
}

impl TaskFactory for ConfigBackedFactory {
    fn validate_spawn_config(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        self.validate_calls.fetch_add(1, Ordering::SeqCst);
        if config.placement == self.configured_placement {
            Ok(())
        } else {
            Err(RuntimeError::CapabilityViolation(format!(
                "unknown child placement {:?}; available placements: {}",
                config.placement, self.configured_placement
            )))
        }
    }

    fn placement_backend(&self, _config: &SpawnConfig) -> AgentBackend {
        AgentBackend::Native
    }

    fn prepare_spawn_config(&self, _config: &mut SpawnConfig) -> Result<(), RuntimeError> {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn create_task(&self, _config: SpawnConfig, _cancellation: CancellationToken) -> BoxTaskFuture {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(journal) = &self.journal {
            *self
                .journal_at_create
                .lock()
                .expect("journal-at-create lock") = journal
                .read_all(&self.parent_id)
                .expect("journal should be readable when child construction begins");
        }
        let runtime_finished = Arc::clone(&self.runtime_finished);
        Box::pin(async move {
            runtime_finished.store(true, Ordering::SeqCst);
            Ok(completed_output())
        })
    }

    fn after_spawn(
        &self,
        _config: &SpawnConfig,
        _result: &simulacra_runtime::SpawnResult,
    ) -> Result<(), RuntimeError> {
        self.after_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingActivitySink(Mutex<Vec<ActivityEvent>>);

impl simulacra_runtime::ActivitySink for RecordingActivitySink {
    fn emit(&self, event: ActivityEvent) {
        self.0.lock().expect("activity lock").push(event);
    }
}

impl RecordingActivitySink {
    fn assert_empty(&self) {
        assert!(
            self.0.lock().expect("activity lock").is_empty(),
            "rejected spawn must emit no activity"
        );
    }
}

fn assert_missing_spawn_journal(error: &RuntimeError) {
    // The dedicated production variant is introduced by GREEN. Comparing its
    // Debug identity keeps this RED suite compiling before that variant exists
    // while still rejecting a generic Session/Journal string once GREEN lands.
    assert_eq!(format!("{error:?}"), "SpawnMissingJournal");
    assert_eq!(
        error.to_string(),
        "spawn_agent called on a supervisor with no journal storage configured; call set_journal_storage before spawning"
    );
}

#[cfg(feature = "spawn")]
fn assert_spawn_tool_runtime_error(error: ToolError, placement: &str, runtime_error: &str) {
    let message = match error {
        ToolError::ExecutionFailed(message) => message,
        other => panic!("supervisor runtime rejection must remain an execution failure: {other}"),
    };
    let suffix = format!(" (placement={placement:?}) failed: {runtime_error}");
    let child_id = message
        .strip_prefix("child ")
        .and_then(|message| message.strip_suffix(&suffix))
        .expect("tool error must preserve exact child, placement, and runtime error vocabulary");
    // S061: task_name-less spawns with slug-able tasks mint path-shaped ids
    // (`/forge/<slug>`); legacy hex ids remain only for un-slug-able tasks.
    // The slug is non-empty and bounded (32 base chars plus `_100` suffix headroom).
    let is_path_id = child_id.strip_prefix("/forge/").is_some_and(|slug| {
        (1..=36).contains(&slug.len())
            && slug
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    });
    let is_legacy_id = child_id.len() == "child-".len() + 32
        && child_id.starts_with("child-")
        && child_id["child-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    assert!(
        is_path_id || is_legacy_id,
        "tool error child id must be path-shaped or legacy hex: {child_id}"
    );
}

#[cfg(feature = "spawn")]
fn start_actor(
    supervisor: Arc<AgentSupervisor>,
) -> (
    tokio::sync::mpsc::Sender<simulacra_runtime::SupervisorMessage>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    (sender, actor)
}

#[tokio::test]
#[cfg(feature = "spawn")]
async fn s060_default_only_task_factory_cannot_accept_an_unconfigured_placement() {
    let parent_id = AgentId("parent-default-factory".to_string());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let create_calls = Arc::new(AtomicUsize::new(0));
    let factory = DefaultOnlyFactory {
        create_calls: Arc::clone(&create_calls),
    };
    let initial_budget = parent_budget();
    let shared_budget = Arc::new(Mutex::new(initial_budget.clone()));
    let events = Arc::new(RecordingActivitySink::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability(UNCONFIGURED_PLACEMENT),
        Arc::clone(&shared_budget),
        Arc::new(factory),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_activity_sink(Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>);
    let supervisor = Arc::new(supervisor);
    let (sender, actor) = start_actor(Arc::clone(&supervisor));
    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec![UNCONFIGURED_PLACEMENT.to_string()],
        activity_sink: Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>,
        parent_id: parent_id.clone(),
        parent_budget: Arc::clone(&shared_budget),
        guidance: None,
    };

    let result = tool
        .call(
            spawn_arguments(UNCONFIGURED_PLACEMENT),
            &parent_capability(UNCONFIGURED_PLACEMENT),
        )
        .await;
    drop(tool);
    actor.await.expect("supervisor actor should stop");

    assert_spawn_tool_runtime_error(
        result.expect_err("trait defaults must not authorize an arbitrary placement"),
        UNCONFIGURED_PLACEMENT,
        "session error: unknown child placement \"arbitrary-unconfigured\"",
    );
    assert_eq!(create_calls.load(Ordering::SeqCst), 0);
    assert_budget_unchanged(&supervisor.parent_budget(), &initial_budget);
    assert!(
        journal
            .read_all(&parent_id)
            .expect("journal read should succeed")
            .is_empty(),
        "a rejected unconfigured placement must have no journal effects"
    );
    events.assert_empty();
}

#[tokio::test]
#[cfg(feature = "spawn")]
async fn s060_actor_spawn_without_journal_fails_before_ack_factory_or_reservation() {
    let parent_id = AgentId("parent-actor-no-journal".to_string());
    let factory = ConfigBackedFactory::new(None, parent_id.clone());
    let initial_budget = parent_budget();
    let shared_budget = Arc::new(Mutex::new(initial_budget.clone()));
    let events = Arc::new(RecordingActivitySink::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability(CONFIGURED_PLACEMENT),
        Arc::clone(&shared_budget),
        Arc::new(factory.clone()),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_activity_sink(Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>);
    let supervisor = Arc::new(supervisor);
    let (sender, actor) = start_actor(Arc::clone(&supervisor));
    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec![CONFIGURED_PLACEMENT.to_string()],
        activity_sink: Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>,
        parent_id,
        parent_budget: Arc::clone(&shared_budget),
        guidance: None,
    };

    let result = tool
        .call(
            spawn_arguments(CONFIGURED_PLACEMENT),
            &parent_capability(CONFIGURED_PLACEMENT),
        )
        .await;
    drop(tool);
    actor.await.expect("supervisor actor should stop");

    assert_spawn_tool_runtime_error(
        result.expect_err("an accepted-spawn path requires journal storage"),
        CONFIGURED_PLACEMENT,
        "spawn_agent called on a supervisor with no journal storage configured; call set_journal_storage before spawning",
    );
    assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.prepare_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.after_calls.load(Ordering::SeqCst), 0);
    assert_budget_unchanged(&supervisor.parent_budget(), &initial_budget);
    events.assert_empty();
}

#[tokio::test]
#[cfg(feature = "spawn")]
async fn s060_actor_unknown_placement_wins_over_missing_journal_without_side_effects() {
    let parent_id = AgentId("parent-actor-dual-defect".to_string());
    let factory = ConfigBackedFactory::new(None, parent_id.clone());
    let initial_budget = parent_budget();
    let shared_budget = Arc::new(Mutex::new(initial_budget.clone()));
    let events = Arc::new(RecordingActivitySink::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability(UNCONFIGURED_PLACEMENT),
        Arc::clone(&shared_budget),
        Arc::new(factory.clone()),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_activity_sink(Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>);
    let supervisor = Arc::new(supervisor);
    let (sender, actor) = start_actor(Arc::clone(&supervisor));
    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec![UNCONFIGURED_PLACEMENT.to_string()],
        activity_sink: Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>,
        parent_id,
        parent_budget: Arc::clone(&shared_budget),
        guidance: None,
    };

    let result = tool
        .call(
            spawn_arguments(UNCONFIGURED_PLACEMENT),
            &parent_capability(UNCONFIGURED_PLACEMENT),
        )
        .await;
    drop(tool);
    actor.await.expect("supervisor actor should stop");

    assert_spawn_tool_runtime_error(
        result.expect_err("ordinary placement validation must precede journal wiring validation"),
        UNCONFIGURED_PLACEMENT,
        "capability violation: unknown child placement \"arbitrary-unconfigured\"; available placements: in_process",
    );
    assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.prepare_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.after_calls.load(Ordering::SeqCst), 0);
    assert_budget_unchanged(&supervisor.parent_budget(), &initial_budget);
    events.assert_empty();
}

#[tokio::test]
async fn s060_direct_spawn_without_journal_fails_before_factory_or_reservation() {
    let parent_id = AgentId("parent-direct-no-journal".to_string());
    let factory = ConfigBackedFactory::new(None, parent_id.clone());
    let initial_budget = parent_budget();
    let shared_budget = Arc::new(Mutex::new(initial_budget.clone()));
    let events = Arc::new(RecordingActivitySink::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability(CONFIGURED_PLACEMENT),
        Arc::clone(&shared_budget),
        Arc::new(factory.clone()),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_activity_sink(Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>);

    let result = supervisor.spawn_agent(spawn_config(
        "child-direct-no-journal",
        &parent_id.0,
        CONFIGURED_PLACEMENT,
    ));

    let error = result.expect_err("direct accepted-spawn path requires journal storage");
    assert_missing_spawn_journal(&error);
    assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.prepare_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.after_calls.load(Ordering::SeqCst), 0);
    assert_budget_unchanged(&supervisor.parent_budget(), &initial_budget);
    events.assert_empty();

    let journal = Arc::new(InMemoryJournalStorage::new());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor
        .spawn_agent(spawn_config(
            "child-after-journal-installed",
            &parent_id.0,
            CONFIGURED_PLACEMENT,
        ))
        .expect("the explicitly bound root should succeed after journal installation");
    assert_eq!(
        journal
            .read_all(&parent_id)
            .expect("accepted root journal should be readable")
            .len(),
        2
    );
}

#[tokio::test]
async fn s060_unknown_placement_wins_over_missing_journal_without_side_effects() {
    let rejected_parent = AgentId("root-dual-defect".to_string());
    let factory = ConfigBackedFactory::new(None, rejected_parent.clone());
    let initial_budget = parent_budget();
    let shared_budget = Arc::new(Mutex::new(initial_budget.clone()));
    let events = Arc::new(RecordingActivitySink::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability(UNCONFIGURED_PLACEMENT),
        Arc::clone(&shared_budget),
        Arc::new(factory.clone()),
    );
    supervisor.set_root_agent_id(rejected_parent.clone());
    supervisor.set_activity_sink(Arc::clone(&events) as Arc<dyn simulacra_runtime::ActivitySink>);

    let error = supervisor
        .spawn_agent(spawn_config(
            "child-dual-defect",
            &rejected_parent.0,
            UNCONFIGURED_PLACEMENT,
        ))
        .expect_err("ordinary placement validation must precede journal wiring validation");

    assert_eq!(
        error.to_string(),
        "capability violation: unknown child placement \"arbitrary-unconfigured\"; available placements: in_process"
    );
    assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.prepare_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.after_calls.load(Ordering::SeqCst), 0);
    assert_budget_unchanged(&supervisor.parent_budget(), &initial_budget);
    events.assert_empty();
}

#[tokio::test]
#[cfg(feature = "spawn")]
async fn s060_config_backed_spawn_journals_spawned_before_runtime_and_completed_after() {
    let parent_id = AgentId("parent-journal-complete".to_string());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let journal_port = Arc::clone(&journal) as Arc<dyn JournalStorage>;
    let factory = ConfigBackedFactory::new(Some(Arc::clone(&journal_port)), parent_id.clone());
    let initial_budget = parent_budget();
    let shared_budget = Arc::new(Mutex::new(initial_budget.clone()));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability(CONFIGURED_PLACEMENT),
        Arc::clone(&shared_budget),
        Arc::new(factory.clone()),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_journal_storage(journal_port);
    let supervisor = Arc::new(supervisor);
    let (sender, actor) = start_actor(Arc::clone(&supervisor));
    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec![CONFIGURED_PLACEMENT.to_string()],
        activity_sink: Arc::new(RecordingActivitySink::default()),
        parent_id: parent_id.clone(),
        parent_budget: Arc::clone(&shared_budget),
        guidance: None,
    };

    let acknowledgement = tool
        .call(
            spawn_arguments(CONFIGURED_PLACEMENT),
            &parent_capability(CONFIGURED_PLACEMENT),
        )
        .await
        .expect("configured placement with a journal should be accepted");
    let child_id = acknowledgement["child_id"]
        .as_str()
        .expect("accepted spawn should return a child id")
        .to_string();
    drop(tool);
    actor.await.expect("supervisor actor should stop");

    assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.prepare_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.after_calls.load(Ordering::SeqCst), 1);
    assert!(factory.runtime_finished.load(Ordering::SeqCst));
    let at_create = factory
        .journal_at_create
        .lock()
        .expect("journal-at-create lock")
        .clone();
    assert_eq!(at_create.len(), 1, "only Spawned exists before runtime");
    assert_eq!(at_create[0].schema_version, 3);
    assert_eq!(at_create[0].agent_id, parent_id);
    assert!(matches!(
        &at_create[0].entry,
        JournalEntryKind::SubAgentSpawned {
            child_id: recorded_child,
            placement,
            backend,
            task,
            instructions,
        } if recorded_child.0 == child_id
            && placement == CONFIGURED_PLACEMENT
            && backend == "native"
            && task == "preserve this task"
            && instructions.as_deref() == Some("preserve these instructions")
    ));

    let completed = journal
        .read_all(&parent_id)
        .expect("completed journal should be readable");
    assert_eq!(
        completed.len(),
        2,
        "accepted lifecycle must be journal-complete"
    );
    assert!(completed.iter().all(|entry| entry.schema_version == 3));
    assert!(completed.iter().all(|entry| entry.agent_id == parent_id));
    assert_eq!(
        shared_budget
            .lock()
            .expect("shared root budget")
            .used_sub_agents,
        1,
        "supervisor and tool must observe one shared accepted-spawn budget"
    );
    assert!(matches!(
        &completed[1].entry,
        JournalEntryKind::SubAgentCompleted {
            child_id: recorded_child,
            success: true,
        } if recorded_child.0 == child_id
    ));
}
