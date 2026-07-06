use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use simulacra_cli::{CliArgs, CliMode, OutputFormat, run_with_provider_and_child_provider_factory};
use simulacra_runtime::ChildProviderFactory;
use simulacra_types::{
    FinishReason, Message, Provider, ProviderError, ProviderResponse, ResourceBudget, Role,
    TokenUsage, ToolCallMessage, ToolDefinition,
};
use tempfile::TempDir;

#[derive(Debug)]
struct TempConfig {
    _dir: TempDir,
    path: PathBuf,
}

impl TempConfig {
    fn write(contents: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp config dir should be created");
        let path = dir.path().join("simulacra.toml");
        fs::write(&path, contents).expect("temp config should be written");
        Self { _dir: dir, path }
    }

    fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

#[derive(Debug)]
struct ScriptedProvider {
    responses: Mutex<VecDeque<Result<ProviderResponse, ProviderError>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
        }
    }
}

impl Provider for ScriptedProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.responses
                .lock()
                .expect("scripted provider lock should not be poisoned")
                .pop_front()
                .unwrap_or_else(|| Err(ProviderError::Other("provider script exhausted".into())))
        })
    }
}

#[derive(Debug)]
struct ParentOrchestratorProvider {
    call_count: Mutex<usize>,
}

impl ParentOrchestratorProvider {
    fn new() -> Self {
        Self {
            call_count: Mutex::new(0),
        }
    }
}

impl Provider for ParentOrchestratorProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut call_count = self
                .call_count
                .lock()
                .map_err(|error| ProviderError::Other(format!("poisoned provider: {error}")))?;
            let response = match *call_count {
                0 => tool_response(vec![tool_call(
                    "spawn-researcher",
                    "spawn_agent",
                    json!({
                        "agent_type": "researcher",
                        "task": "inspect the fixture and report the finding",
                        "budget": {
                            "max_tokens": 256,
                            "max_turns": 1,
                            "max_cost": "0",
                            "max_sub_agents": 0
                        }
                    }),
                )]),
                1 => {
                    let child_id = child_id_from_messages(messages)?;
                    tool_response(vec![tool_call(
                        "wait-researcher",
                        "wait_child_agent",
                        json!({"child_id": child_id, "timeout_ms": 1000}),
                    )])
                }
                2 => {
                    let child_id = child_id_from_messages(messages)?;
                    tool_response(vec![tool_call(
                        "join-researcher",
                        "join_child_agent",
                        json!({"child_id": child_id}),
                    )])
                }
                3 => {
                    let child_id = child_id_from_messages(messages)?;
                    tool_response(vec![tool_call(
                        "close-researcher",
                        "close_child_agent",
                        json!({"child_id": child_id}),
                    )])
                }
                4 => final_response("parent integrated child finding"),
                _ => {
                    return Err(ProviderError::Other(
                        "parent orchestration script exhausted".into(),
                    ));
                }
            };
            *call_count += 1;
            Ok(response)
        })
    }
}

fn child_id_from_messages(messages: &[Message]) -> Result<String, ProviderError> {
    messages
        .iter()
        .rev()
        .filter_map(|message| serde_json::from_str::<Value>(&message.content).ok())
        .find_map(|value| {
            value
                .get("child_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| ProviderError::Other("missing child_id in parent transcript".into()))
}

fn tool_call(id: &str, name: &str, arguments: Value) -> ToolCallMessage {
    ToolCallMessage {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
    }
}

fn tool_response(tool_calls: Vec<ToolCallMessage>) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls,
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-tools".into()),
        model: "claude-sonnet-4-20250514".into(),
    }
}

fn final_response(text: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: text.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-final".into()),
        model: "claude-sonnet-4-20250514".into(),
    }
}

fn config_toml() -> &'static str {
    r#"[project]
name = "headless-spawn-orchestration"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 8
max_tokens = 4096
max_sub_agents = 2
can_spawn = ["researcher"]

[agent_types.default.capabilities]
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]

[agent_types.researcher]
model = "claude-sonnet-4-20250514"
max_turns = 1
max_tokens = 256

[agent_types.researcher.capabilities]
paths_read = ["/workspace/**"]

[task]
entry_agent = "default"
"#
}

fn headless_jsonl_args(config_path: String) -> CliArgs {
    CliArgs {
        config_path,
        task: Some("delegate and integrate the fixture finding".to_string()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: true,
        output_format: OutputFormat::Jsonl,
    }
}

fn parse_jsonl(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("headless JSONL line should parse"))
        .collect()
}

fn tool_outputs(lines: &[Value]) -> Vec<Value> {
    lines
        .iter()
        .filter(|line| line["event"]["type"] == "ToolOutput")
        .filter_map(|line| line["event"]["line"].as_str())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn headless_spawn_wait_join_close_can_use_injected_child_provider_factory() {
    let config = TempConfig::write(config_toml());
    let child_factory_calls = Arc::new(AtomicUsize::new(0));
    let child_factory: ChildProviderFactory = {
        let calls = Arc::clone(&child_factory_calls);
        Arc::new(move |_provider_kind, _model| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(ScriptedProvider::new(vec![final_response(
                "child finding: fixture inspected offline",
            )])))
        })
    };

    let output = run_with_provider_and_child_provider_factory(
        headless_jsonl_args(config.path_string()),
        Box::new(ParentOrchestratorProvider::new()),
        child_factory,
    )
    .expect("headless run should return cli output");

    assert_eq!(output.exit_code, 0, "stderr={:?}", output.stderr_content);
    assert_eq!(child_factory_calls.load(Ordering::SeqCst), 1);

    let lines = parse_jsonl(&output.stdout_content);
    let outputs = tool_outputs(&lines);
    assert!(
        outputs.iter().any(|output| output["status"] == "running"),
        "spawn_agent should return a live handle: {outputs:#?}"
    );
    assert!(
        outputs
            .iter()
            .any(|output| output["message"] == "child finding: fixture inspected offline"),
        "wait/join should surface the injected child's terminal message: {outputs:#?}"
    );
    assert!(
        outputs.iter().any(|output| output["status"] == "closed"),
        "close_child_agent should close the terminal handle: {outputs:#?}"
    );

    let result = lines.last().expect("JSONL result line should be present");
    assert_eq!(result["kind"], "result");
    assert_eq!(result["ok"], true);
    assert_eq!(result["final_message"], "parent integrated child finding");
}
