use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::WorkflowError;

pub type WorkflowWorkerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkflowAgentResult, WorkflowError>> + Send + 'a>>;

pub trait WorkflowWorker: Send + Sync + 'static {
    fn call<'a>(&'a self, call: WorkflowAgentCall) -> WorkflowWorkerFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowScriptMeta {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowScript {
    pub path: String,
    pub source: String,
    pub meta: WorkflowScriptMeta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowAgentCall {
    pub key: String,
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub arguments: Value,
}

impl WorkflowAgentCall {
    pub fn argument(&self, name: &str) -> Option<&Value> {
        self.arguments.get(name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowAgentResult {
    pub key: String,
    pub output: Value,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WorkflowAgentResult {
    pub fn success(key: impl Into<String>, output: Value) -> Self {
        Self {
            key: key.into(),
            output,
            is_error: false,
            error: None,
        }
    }

    pub fn failure(key: impl Into<String>, message: impl Into<String>) -> Self {
        let key = key.into();
        let message = message.into();
        Self {
            key,
            output: Value::Null,
            is_error: true,
            error: Some(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub run_id: String,
    pub script_path: String,
    pub meta: WorkflowScriptMeta,
    pub status: WorkflowStatus,
    #[serde(default)]
    pub results: Vec<WorkflowAgentResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WorkflowRun {
    pub fn pending(run_id: String, script: &WorkflowScript) -> Self {
        Self {
            run_id,
            script_path: script.path.clone(),
            meta: script.meta.clone(),
            status: WorkflowStatus::Pending,
            results: Vec::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowEvent {
    RunStarted {
        run_id: String,
        script_path: String,
        name: String,
    },
    Progress {
        run_id: String,
        message: String,
    },
    PhaseStarted {
        run_id: String,
        name: String,
    },
    PhaseCompleted {
        run_id: String,
        name: String,
    },
    AgentCallStarted {
        run_id: String,
        key: String,
        agent: Option<String>,
        task: Option<String>,
    },
    AgentCallCompleted {
        run_id: String,
        key: String,
        cached: bool,
        is_error: bool,
    },
    RunCompleted {
        run_id: String,
    },
    RunFailed {
        run_id: String,
        error: String,
    },
    RunCancelled {
        run_id: String,
    },
}

impl WorkflowEvent {
    pub fn to_sse_json(&self, seq: u64) -> Value {
        match self {
            WorkflowEvent::RunStarted {
                run_id,
                script_path,
                name,
            } => json_event("workflow.started", run_id, seq)
                .with("script_path", script_path)
                .with("name", name)
                .with("status", "running"),
            WorkflowEvent::Progress { run_id, message } => {
                json_event("workflow.progress", run_id, seq)
                    .with("message", message)
                    .with("status", "running")
            }
            WorkflowEvent::PhaseStarted { run_id, name } => {
                json_event("workflow.phase_start", run_id, seq)
                    .with("phase", name)
                    .with("status", "running")
            }
            WorkflowEvent::PhaseCompleted { run_id, name } => {
                json_event("workflow.phase_finish", run_id, seq)
                    .with("phase", name)
                    .with("status", "running")
            }
            WorkflowEvent::AgentCallStarted {
                run_id,
                key,
                agent,
                task,
            } => {
                let mut value = json_event("workflow.agent_start", run_id, seq)
                    .with("agent_label", key)
                    .with("status", "running");
                if let Some(agent) = agent {
                    value = value.with("agent_type", agent);
                }
                if let Some(task) = task {
                    value = value.with("task", task);
                }
                value
            }
            WorkflowEvent::AgentCallCompleted {
                run_id,
                key,
                cached,
                is_error,
            } => json_event("workflow.agent_finish", run_id, seq)
                .with("agent_label", key)
                .with("cached", *cached)
                .with("is_error", *is_error)
                .with("status", if *is_error { "failed" } else { "running" }),
            WorkflowEvent::RunCompleted { run_id } => {
                json_event("workflow.completed", run_id, seq).with("status", "completed")
            }
            WorkflowEvent::RunFailed { run_id, error } => {
                json_event("workflow.failed", run_id, seq)
                    .with("status", "failed")
                    .with("error", error)
            }
            WorkflowEvent::RunCancelled { run_id } => {
                json_event("workflow.cancelled", run_id, seq).with("status", "cancelled")
            }
        }
    }
}

fn json_event(event: &str, run_id: &str, seq: u64) -> Value {
    serde_json::json!({
        "event": event,
        "run_id": run_id,
        "seq": seq,
    })
}

trait JsonWith {
    fn with(self, key: &str, value: impl Serialize) -> Value;
}

impl JsonWith for Value {
    fn with(mut self, key: &str, value: impl Serialize) -> Value {
        self[key] = serde_json::to_value(value).unwrap_or(Value::Null);
        self
    }
}
