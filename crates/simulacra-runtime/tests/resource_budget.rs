use rust_decimal::Decimal;
use simulacra_runtime::{
    AgentLoopOutput, AgentSupervisor, BoxTaskFuture, CancellationToken, InMemoryJournalStorage,
    SpawnConfig, TaskFactory,
};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, ExitReason, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalStorage, Message, ResourceBudget, Role, TokenUsage,
};
use std::sync::Arc;

/// Immediately-completing task factory for tests that only care about
/// the spawn bookkeeping (sub_agent increment, journal, deduction paths).
/// Required since spawn_agent rejects supervisors without a task factory.
struct NoopTaskFactory;

impl TaskFactory for NoopTaskFactory {
    fn create_task(&self, _config: SpawnConfig, _token: CancellationToken) -> BoxTaskFuture {
        Box::pin(async {
            Ok(AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![],
                token_usage: TokenUsage::default(),
                reported_tool_uses: None,
                used_turns: 0,
                used_cost: Decimal::ZERO,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// S006: Budget state is serialized into checkpoint data
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_budget_snapshot_survives_serialize_deserialize_roundtrip() {
    let storage = InMemoryJournalStorage::new();
    let agent = AgentId("agent-budget".into());

    // Append an entry so checkpoint has something to follow
    storage
        .append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent.clone(),
            timestamp_ms: 1000,
            entry: JournalEntryKind::TurnStart,
        })
        .unwrap();

    // Create a budget with specific used values
    let mut budget = ResourceBudget::new(100_000, 50, Decimal::new(500, 2), 10);
    budget.used_tokens = 42_000;
    budget.used_turns = 7;
    budget.used_cost = Decimal::new(123, 2);
    budget.used_sub_agents = 2;

    let checkpoint_data = CheckpointData {
        messages: vec![Message {
            role: Role::User,
            content: "test".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }],
        budget_snapshot: budget,
        vfs_snapshot: None,
    };

    // Save checkpoint
    storage.save_checkpoint(&agent, 1, checkpoint_data).unwrap();

    // Read back all entries and find the checkpoint
    let entries = storage.read_all(&agent).unwrap();
    let checkpoint_entry = entries
        .iter()
        .find(|e| matches!(e.entry, JournalEntryKind::Checkpoint { .. }))
        .expect("checkpoint entry should exist");

    // Extract the checkpoint data and verify budget values survived
    if let JournalEntryKind::Checkpoint { snapshot_data } = &checkpoint_entry.entry {
        // snapshot_data is already serialized bytes — deserialize to prove budget survives roundtrip
        let restored: CheckpointData = serde_json::from_slice(snapshot_data)
            .expect("checkpoint data should deserialize from stored bytes");

        assert_eq!(restored.budget_snapshot.max_tokens, 100_000);
        assert_eq!(restored.budget_snapshot.max_turns, 50);
        assert_eq!(restored.budget_snapshot.max_cost, Decimal::new(500, 2));
        assert_eq!(restored.budget_snapshot.max_sub_agents, 10);
        assert_eq!(restored.budget_snapshot.used_tokens, 42_000);
        assert_eq!(restored.budget_snapshot.used_turns, 7);
        assert_eq!(restored.budget_snapshot.used_cost, Decimal::new(123, 2));
        assert_eq!(restored.budget_snapshot.used_sub_agents, 2);
    } else {
        panic!("expected Checkpoint entry kind");
    }
}

// ---------------------------------------------------------------------------
// S006: Child agent budget usage is deducted from parent budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_budget_deduction_increases_parent_used_tokens_turns_cost() {
    let parent_cap = CapabilityToken::default();
    let parent_budget = ResourceBudget::new(100_000, 50, Decimal::new(500, 2), 10);

    let mut supervisor = AgentSupervisor::with_task_factory(
        parent_cap.clone(),
        parent_budget,
        Arc::new(NoopTaskFactory),
    );

    // Create a child config with some simulated usage
    let mut child_budget = ResourceBudget::new(10_000, 5, Decimal::new(50, 2), 0);
    child_budget.used_tokens = 3_000;
    child_budget.used_turns = 2;
    child_budget.used_cost = Decimal::new(15, 2);

    let config = SpawnConfig {
        agent_id: AgentId("child-1".into()),
        parent_id: AgentId("parent".into()),
        capability: Some(parent_cap.clone()),
        budget: child_budget,
        restart_strategy: simulacra_runtime::RestartStrategy::LetCrash,
        agent_type: Some(String::new()),
        task: String::new(),
        system_prompt: None,
        tier: None,
        resolved_tier: None,
    };

    // Spawn the child (increments used_sub_agents)
    let _token = supervisor
        .spawn_agent(SpawnConfig {
            agent_id: AgentId("child-1".into()),
            parent_id: AgentId("parent".into()),
            capability: Some(parent_cap.clone()),
            budget: ResourceBudget::new(10_000, 5, Decimal::new(50, 2), 0),
            restart_strategy: simulacra_runtime::RestartStrategy::LetCrash,
            agent_type: Some(String::new()),
            task: String::new(),
            system_prompt: None,
            tier: None,
            resolved_tier: None,
        })
        .expect("spawn should succeed");

    // Complete the child — this should deduct its usage from the parent
    supervisor.handle_completion(&config);

    // Verify parent budget reflects child usage
    let parent = supervisor.parent_budget();
    assert_eq!(
        parent.used_tokens, 3_000,
        "parent used_tokens should include child usage"
    );
    assert_eq!(
        parent.used_turns, 2,
        "parent used_turns should include child usage"
    );
    assert_eq!(
        parent.used_cost,
        Decimal::new(15, 2),
        "parent used_cost should include child usage"
    );
    assert_eq!(
        parent.used_sub_agents, 1,
        "parent used_sub_agents should be 1 from spawn"
    );
}

#[test]
fn multiple_child_deductions_accumulate_in_parent() {
    let parent_cap = CapabilityToken::default();
    let parent_budget = ResourceBudget::new(100_000, 50, Decimal::new(500, 2), 10);

    let supervisor = AgentSupervisor::new(parent_cap.clone(), parent_budget);

    // Simulate two children completing with different usage
    for (i, (tokens, turns, cost)) in [(1_000u64, 1u32, 10i64), (2_000, 3, 20)].iter().enumerate() {
        let mut child_budget = ResourceBudget::new(10_000, 5, Decimal::new(50, 2), 0);
        child_budget.used_tokens = *tokens;
        child_budget.used_turns = *turns;
        child_budget.used_cost = Decimal::new(*cost, 2);

        let config = SpawnConfig {
            agent_id: AgentId(format!("child-{i}")),
            parent_id: AgentId("parent".into()),
            capability: Some(parent_cap.clone()),
            budget: child_budget,
            restart_strategy: simulacra_runtime::RestartStrategy::LetCrash,
            agent_type: Some(String::new()),
            task: String::new(),
            system_prompt: None,
            tier: None,
            resolved_tier: None,
        };

        supervisor.handle_completion(&config);
    }

    let parent = supervisor.parent_budget();
    assert_eq!(
        parent.used_tokens, 3_000,
        "tokens should accumulate across children"
    );
    assert_eq!(
        parent.used_turns, 4,
        "turns should accumulate across children"
    );
    assert_eq!(
        parent.used_cost,
        Decimal::new(30, 2),
        "cost should accumulate across children"
    );
}
