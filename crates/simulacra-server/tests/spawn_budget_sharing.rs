use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, NewAgent};
use simulacra_config::{
    AgentBackend, CatalogConfig, ChildPlacementConfig, ProjectConfig, SimulacraConfig, VfsConfig,
};
use simulacra_server::{
    BudgetPoolConfig, ProviderFactory, SimulacraEngine, TaskManager, TaskState, TenantConfig,
};
use simulacra_types::{
    FinishReason, Message, Provider, ProviderError, ProviderResponse, ResourceBudget, Role,
    TokenUsage, ToolCallMessage, ToolDefinition,
};

struct RootBudgetScenarioProvider {
    calls: AtomicUsize,
    recorded_messages: Mutex<Vec<Vec<Message>>>,
}

impl RootBudgetScenarioProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            recorded_messages: Mutex::new(Vec::new()),
        }
    }

    fn response(message: Message, finish_reason: FinishReason) -> ProviderResponse {
        ProviderResponse {
            message,
            token_usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
            finish_reason,
            provider_response_id: None,
            model: "root-model".into(),
        }
    }

    fn tool_call(id: &str, name: &str, arguments: Value) -> ProviderResponse {
        Self::response(
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                }],
                tool_call_id: None,
                provider_content: vec![],
            },
            FinishReason::ToolUse,
        )
    }

    fn spawn_arguments(max_tokens: u64) -> Value {
        json!({
            "placement": "workspace",
            "task": "bounded child work",
            "budget": {
                "max_tokens": max_tokens,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        })
    }

    fn child_id_from_first_ack(messages: &[Message]) -> String {
        let result = messages
            .iter()
            .rev()
            .find(|message| message.tool_call_id.as_deref() == Some("spawn-1"))
            .expect("first spawn acknowledgement should be in parent history");
        serde_json::from_str::<Value>(&result.content)
            .expect("spawn acknowledgement should be JSON")["child_id"]
            .as_str()
            .expect("spawn acknowledgement should contain child_id")
            .to_string()
    }

    fn second_spawn_result(&self) -> String {
        self.recorded_messages
            .lock()
            .expect("recorded messages")
            .iter()
            .rev()
            .flat_map(|batch| batch.iter().rev())
            .find(|message| message.tool_call_id.as_deref() == Some("spawn-2"))
            .expect("second spawn result should reach the parent")
            .content
            .clone()
    }
}

impl Provider for RootBudgetScenarioProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        let messages = messages.to_vec();
        Box::pin(async move {
            self.recorded_messages
                .lock()
                .expect("recorded messages")
                .push(messages.clone());
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let response = match call {
                0 => Self::tool_call("spawn-1", "spawn_agent", Self::spawn_arguments(3)),
                1 => Self::tool_call(
                    "join-1",
                    "join_child_agent",
                    json!({ "child_id": Self::child_id_from_first_ack(&messages) }),
                ),
                2 => Self::tool_call("spawn-2", "spawn_agent", Self::spawn_arguments(5)),
                3 => Self::response(
                    Message {
                        role: Role::Assistant,
                        content: "done".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    },
                    FinishReason::EndTurn,
                ),
                unexpected => panic!("unexpected root provider call {unexpected}"),
            };
            Ok(response)
        })
    }
}

struct SharedRootProvider(Arc<RootBudgetScenarioProvider>);

impl Provider for SharedRootProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        self.0.chat(messages, tools, budget)
    }
}

struct ChildUsageProvider;

impl Provider for ChildUsageProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async {
            Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    content: "child complete".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_content: vec![],
                },
                token_usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
                finish_reason: FinishReason::EndTurn,
                provider_response_id: None,
                model: "child-model".into(),
            })
        })
    }
}

fn s060_config() -> SimulacraConfig {
    let mut child_placements = HashMap::new();
    child_placements.insert(
        "workspace".into(),
        ChildPlacementConfig {
            backend: AgentBackend::Native,
            model: Some("child-model".into()),
            acp_profile: None,
            skills: vec![],
            capabilities: None,
            max_turns: Some(1),
            max_tokens: Some(5),
            max_cost: None,
            max_sub_agents: None,
            allowed_child_placements: vec![],
        },
    );
    SimulacraConfig {
        project: ProjectConfig {
            name: "s060-server-budget".into(),
            description: None,
        },
        agent_types: HashMap::new(),
        child_placements,
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

async fn wait_for_terminal(manager: &TaskManager, task_id: &str) {
    let start = tokio::time::Instant::now();
    loop {
        let task = manager.get_task(task_id).expect("task remains visible");
        if task.state.is_terminal() {
            assert_eq!(task.state, TaskState::Completed);
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "server task did not terminate"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn s060_server_supervisor_and_root_share_combined_remaining_budget() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = catalog
        .tenants()
        .get_or_create("default", Some("default"))
        .await
        .expect("tenant");
    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "root",
                description: Some("budget-sharing root"),
                system_prompt: "exercise child budget sharing",
                model: "root-model",
                max_turns: Some(8),
                max_tokens: Some(10),
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &["spawn:workspace".into()],
                channel_ids: &[],
            },
        )
        .await
        .expect("catalog root agent");

    let root = Arc::new(RootBudgetScenarioProvider::new());
    let root_for_factory = Arc::clone(&root);
    let provider_factory: ProviderFactory = Arc::new(move |_kind, model| {
        if model == "child-model" {
            Ok(Box::new(ChildUsageProvider) as Box<dyn Provider>)
        } else {
            Ok(Box::new(SharedRootProvider(Arc::clone(&root_for_factory))) as Box<dyn Provider>)
        }
    });
    let engine = SimulacraEngine::new(
        s060_config(),
        None,
        Arc::new(catalog.agents()) as Arc<dyn AgentRepository>,
        Arc::new(catalog.skills()) as Arc<dyn SkillRepository>,
        Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>,
        Arc::new(catalog.tenants()) as Arc<dyn TenantRepository>,
    )
    .expect("engine")
    .with_provider_factory(provider_factory);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "exercise combined root and child budget",
            &TenantConfig {
                namespace: "default".into(),
                agent_type: "root".into(),
                vfs_root: PathBuf::from("/tmp/s060-server-budget"),
                budget_pool: BudgetPoolConfig {
                    max_tokens: 10,
                    max_cost: "0".into(),
                },
                hooks: vec![],
                integrations: vec![],
                mcp_servers: vec![],
            },
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn server task");

    wait_for_terminal(&manager, &handle.task_id).await;
    let second_spawn = root.second_spawn_result();
    assert_eq!(
        second_spawn, "ERROR: invalid arguments: max_tokens requested 5 exceeds parent remaining 4",
        "the first child used 3 tokens and the root used 3, so a second 5-token reservation must be denied against combined remaining 4"
    );
}
