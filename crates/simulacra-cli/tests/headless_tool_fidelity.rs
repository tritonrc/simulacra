use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};
use simulacra_cli::{CliArgs, CliMode, OutputFormat, run_with_provider};
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

fn config_toml() -> &'static str {
    r#"[project]
name = "headless-tool-fidelity"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 4
max_tokens = 4096

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/**"]
paths_write = ["/**"]

[task]
entry_agent = "default"
"#
}

fn headless_args(config_path: String, task: &str) -> CliArgs {
    CliArgs {
        config_path,
        task: Some(task.to_string()),
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

fn tool_call(id: &str, name: &str, arguments: Value) -> ToolCallMessage {
    ToolCallMessage {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
    }
}

fn parse_jsonl(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("headless JSONL line should parse"))
        .collect()
}

fn tool_output_lines(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line["event"]["type"] == "ToolOutput")
        .filter_map(|line| line["event"]["line"].as_str())
        .map(str::to_string)
        .collect()
}

#[test]
fn headless_mode_executes_common_agent_tool_fragments_without_recovery_failures() {
    let config = TempConfig::write(config_toml());
    let shell_probe = r#"missing_tool 2>/dev/null || printf 'dev-null-ok\n'
sleep 0 && printf 'sleep-ok\n'
printf 'left right third\n' | grep -oP '(?<=\s)\S+'
mkdir -p /workspace/src /workspace/docs
printf 'alpha\nneedle here\n' > /workspace/src/lib.rs
printf 'doc needle\n' > /workspace/docs/readme.md
grep -rn 'needle' /workspace
rg needle /workspace
rg --files -g '*.rs' /workspace
rg -l needle /workspace
cat <<'EOF' > /workspace/heredoc.txt
heredoc alpha
heredoc beta
EOF
cat /workspace/heredoc.txt
find /workspace -type f \( -name '*.rs' -o -name '*.md' \)
printf 'keep a\nskip\nkeep b\n' | sed -n 's/^keep //p'
printf 'a,b,c\nx,y,z\n' | awk -F, '{print $NF}'"#;
    let js_probe = r#"import { readFileSync, writeFileSync } from 'fs';
writeFileSync('/workspace/js.txt', 'js-sync-ok');
console.log(readFileSync('/workspace/js.txt'));"#;
    let provider = ScriptedProvider::new(vec![
        tool_response(vec![
            tool_call("shell-probe", "shell_exec", json!({"command": shell_probe})),
            tool_call("js-probe", "js_exec", json!({"code": js_probe})),
        ]),
        final_response("probes complete"),
    ]);

    let output = run_with_provider(
        headless_args(
            config.path_string(),
            "exercise shell and js fragments agents commonly try",
        ),
        Box::new(provider),
    )
    .expect("headless run should return cli output");

    assert_eq!(output.exit_code, 0, "stderr={:?}", output.stderr_content);
    let lines = parse_jsonl(&output.stdout_content);
    let tool_output = tool_output_lines(&lines).join("\n");

    assert!(
        tool_output.contains("dev-null-ok"),
        "stderr redirection to /dev/null should not derail shell recovery: {tool_output}"
    );
    assert!(
        tool_output.contains("sleep-ok"),
        "sleep should support short telemetry-wait snippets: {tool_output}"
    );
    assert!(
        tool_output.contains("right") && tool_output.contains("third"),
        "grep -oP whitespace extraction should produce later fields: {tool_output}"
    );
    assert!(
        tool_output.contains("/workspace/src/lib.rs:2:needle here"),
        "recursive grep with line numbers should support source search: {tool_output}"
    );
    assert!(
        tool_output
            .matches("/workspace/src/lib.rs:2:needle here")
            .count()
            >= 2,
        "rg should support recursive source search from the VFS: {tool_output}"
    );
    assert!(
        tool_output.contains("/workspace/src/lib.rs"),
        "rg --files and rg -l should support Codex-style file targeting: {tool_output}"
    );
    assert!(
        tool_output.contains("/workspace/docs/readme.md")
            && tool_output.contains("/workspace/src/lib.rs"),
        "find -type/-name OR groups should find source and doc files: {tool_output}"
    );
    assert!(
        tool_output.contains("heredoc alpha") && tool_output.contains("heredoc beta"),
        "heredoc stdin should support common file-writing snippets: {tool_output}"
    );
    assert!(
        tool_output.contains("a") && tool_output.contains("b"),
        "sed -n substitution should print matching replacements: {tool_output}"
    );
    assert!(
        tool_output.contains("c") && tool_output.contains("z"),
        "awk field extraction should work over piped input: {tool_output}"
    );
    assert!(
        tool_output.contains("js-sync-ok"),
        "js_exec should support fs readFileSync/writeFileSync import style: {tool_output}"
    );

    let result = lines.last().expect("JSONL result line should be present");
    assert_eq!(result["kind"], "result");
    assert_eq!(result["ok"], true);
    assert_eq!(result["final_message"], "probes complete");
}
