use super::tests::{
    S061CompletingFactory, ScriptedAcpRuntime, s056_acp_output, s056_factory, s056_spawn_config,
    s061_arguments, s061_call_and_capture_spawn, s061_capability,
    s061_expect_invalid_arguments_with_tool, s061_spawn_tool,
};
use super::*;
use crate::{AgentSupervisor, InMemoryJournalStorage, NoopActivitySink, TaskFactory};
use simulacra_types::{ExitReason, JournalEntryKind, TokenUsage, Tool};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn s062_schema_exposes_optional_placement_target() {
    let (tool, _receiver) = s061_spawn_tool("s062-schema");
    let schema = tool.definition().input_schema;
    let property = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .and_then(|properties| properties.get("placement_target"))
        .expect("spawn_agent schema should expose placement_target");

    assert_eq!(
        property.get("type").and_then(serde_json::Value::as_str),
        Some("string")
    );
    assert_eq!(
        schema.get("required"),
        Some(&serde_json::json!(["placement", "task", "budget"]))
    );
}

#[tokio::test]
async fn s062_tool_submits_config_with_placement_target() {
    let (tool, mut receiver) = s061_spawn_tool("s062-tool-with-target");
    let (_acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s062_arguments(Some(serde_json::json!("ws-7f3a"))),
    )
    .await;

    assert_eq!(config.placement_target.as_deref(), Some("ws-7f3a"));
}

#[tokio::test]
async fn s062_tool_submits_config_without_placement_target() {
    let (tool, mut receiver) = s061_spawn_tool("s062-tool-without-target");
    let (_acknowledgement, config) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s062_arguments(None)).await;

    assert_eq!(config.placement_target, None);
}

#[tokio::test]
async fn s062_non_string_placement_target_is_invalid() {
    let (tool, mut receiver) = s061_spawn_tool("s062-non-string-target");

    s061_expect_invalid_arguments_with_tool(
        &tool,
        &mut receiver,
        s062_arguments(Some(serde_json::json!(7))),
        &["placement_target"],
    )
    .await;
}

#[tokio::test]
async fn s062_placement_target_preserves_whitespace() {
    let (tool, mut receiver) = s061_spawn_tool("s062-whitespace-target");
    let (_acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s062_arguments(Some(serde_json::json!("  ws-7f3a  "))),
    )
    .await;

    assert_eq!(config.placement_target.as_deref(), Some("  ws-7f3a  "));
}

#[tokio::test]
async fn s062_empty_placement_target_reaches_the_config() {
    let (tool, mut receiver) = s061_spawn_tool("s062-empty-target");
    let (_acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s062_arguments(Some(serde_json::json!(""))),
    )
    .await;

    assert_eq!(config.placement_target.as_deref(), Some(""));
}

#[tokio::test]
async fn s062_acp_request_carries_placement_target() {
    let request = s062_acp_request(Some("ws-7f3a".into())).await;

    assert_eq!(request.placement_target.as_deref(), Some("ws-7f3a"));
}

#[tokio::test]
async fn s062_acp_request_carries_absent_placement_target() {
    let request = s062_acp_request(None).await;

    assert_eq!(request.placement_target, None);
}

#[tokio::test]
async fn s062_acp_request_round_trips_placement_target() {
    let request = s062_acp_request(Some("ws-7f3a".into())).await;
    let value = serde_json::to_value(&request).expect("AcpChildRequest should serialize");

    assert_eq!(value["placement_target"], serde_json::json!("ws-7f3a"));

    let decoded: AcpChildRequest =
        serde_json::from_value(value).expect("AcpChildRequest should deserialize");

    assert_eq!(decoded.placement_target.as_deref(), Some("ws-7f3a"));
}

#[tokio::test]
async fn s062_journaled_spawn_carries_placement_target() {
    let journaled =
        s062_journaled_placement_target("s062-journal-with-target", Some("ws-7f3a".into())).await;

    assert_eq!(journaled.as_deref(), Some("ws-7f3a"));
}

#[tokio::test]
async fn s062_journaled_spawn_carries_absent_placement_target() {
    let journaled = s062_journaled_placement_target("s062-journal-without-target", None).await;

    assert_eq!(journaled, None);
}

fn s062_arguments(placement_target: Option<serde_json::Value>) -> serde_json::Value {
    let mut arguments = s061_arguments("review the focused change", None);
    if let Some(placement_target) = placement_target {
        arguments
            .as_object_mut()
            .expect("S062 arguments should be an object")
            .insert("placement_target".into(), placement_target);
    }
    arguments
}

async fn s062_acp_request(placement_target: Option<String>) -> AcpChildRequest {
    let requests = Arc::new(Mutex::new(Vec::<AcpChildRequest>::new()));
    let requests_for_runtime = Arc::clone(&requests);
    let runtime = ScriptedAcpRuntime::new(
        move |request, _cancellation, _activity_sink, _input_queue| {
            let requests = Arc::clone(&requests_for_runtime);
            Box::pin(async move {
                requests.lock().unwrap().push(request);
                Ok(s056_acp_output(
                    ExitReason::Complete,
                    "ACP terminal summary",
                    TokenUsage::default(),
                    1,
                ))
            })
        },
    );
    let factory = s056_factory(
        Some(runtime),
        Arc::new(NoopActivitySink),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut config = s056_spawn_config();
    config.placement_target = placement_target;

    factory
        .create_task(config, CancellationToken::new(Duration::from_millis(50)))
        .await
        .expect("ACP child should run through injected runtime");

    requests
        .lock()
        .unwrap()
        .pop()
        .expect("ACP runtime should receive one request")
}

async fn s062_journaled_placement_target(
    parent: &str,
    placement_target: Option<String>,
) -> Option<String> {
    let parent_id = AgentId(parent.into());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let mut supervisor = AgentSupervisor::with_task_factory(
        s061_capability(),
        ResourceBudget::new(10_000, 100, Decimal::ZERO, 100),
        Arc::new(S061CompletingFactory),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(supervisor_rx).await;
    });
    let tool = SpawnAgentTool {
        sender: supervisor_tx.clone(),
        allowed_placements: vec!["reviewer".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: parent_id.clone(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(
            10_000,
            100,
            Decimal::ZERO,
            100,
        ))),
        guidance: None,
    };

    tool.call(
        s062_arguments(placement_target.map(serde_json::Value::String)),
        &s061_capability(),
    )
    .await
    .expect("spawn_agent should accept the spawn");

    drop(tool);
    drop(supervisor_tx);
    supervisor_task
        .await
        .expect("supervisor should exit after channel close");

    journal
        .read_all(&parent_id)
        .expect("parent journal should remain readable")
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::SubAgentSpawned {
                placement_target, ..
            } => Some(placement_target),
            _ => None,
        })
        .expect("the spawn should journal a SubAgentSpawned entry")
}
