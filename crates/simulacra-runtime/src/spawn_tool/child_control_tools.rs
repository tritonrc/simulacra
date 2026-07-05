use super::json_output::{
    child_status_json, child_terminal_json, wait_child_json, wait_children_json,
};
use super::*;

// ---------------------------------------------------------------------------
// Child-control tools
// ---------------------------------------------------------------------------
//
// These tools operate on live or completed child handles created by
// `SpawnAgentTool`. Each is a thin wrapper that sends a single
// `SupervisorMessage` over the supervisor's mpsc channel and awaits the
// matching oneshot acknowledgement.

const JOIN_CHILD_AGENT_DESCRIPTION: &str = "\
Wait for a child agent when its terminal result is needed and return the \
canonical terminal summary. This is the potentially blocking API for consuming \
the final child outcome after spawn_agent has returned a live handle.";

const CHILD_STATUS_DESCRIPTION: &str = "\
Cheap nonblocking probe for a child handle. Use child_status to inspect \
whether a live or completed child is running, ready, completed, failed, or \
cancelled without waiting for or consuming the terminal result.";

const WAIT_CHILD_AGENT_DESCRIPTION: &str = "\
Bounded, non-consuming wait for one child or for any child in child_ids to \
become terminal. timeout_ms = 0 polls once without waiting. Supplying \
child_ids performs wait-any orchestration and returns the first terminal child \
in listed order when several are already ready. A timeout while the child is \
still running is a successful non-error result with status running and ready \
false; join_child_agent can still return the same terminal result later.";

const CLOSE_CHILD_AGENT_DESCRIPTION: &str = "\
Clean up a terminal child handle and cached terminal result after the parent no \
longer needs it. close_child_agent is only for completed, failed, or cancelled \
children; it is not cancellation and must not be used to stop running work.";

pub struct JoinChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for JoinChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "join_child_agent".to_string(),
            description: JOIN_CHILD_AGENT_DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = arguments
                .get("child_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: child_id".into(),
                    )
                })?
                .to_string();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::JoinChild(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let terminal = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped join response channel".into(),
                )
            })?;
            let terminal = terminal.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(child_terminal_json(terminal, None))
        })
    }
}

pub struct ChildStatusTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for ChildStatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "child_status".to_string(),
            description: CHILD_STATUS_DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = parse_required_child_id(&arguments)?;
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::ChildStatus(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let status = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped child_status response channel".into(),
                )
            })?;
            let status = status.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(child_status_json(status))
        })
    }
}

pub struct WaitChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for WaitChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wait_child_agent".to_string(),
            description: WAIT_CHILD_AGENT_DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    },
                    "child_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Child agent ids returned by spawn_agent. Waits until any listed child is terminal."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Maximum time to wait in milliseconds. Zero polls once without waiting."
                    }
                },
                "required": ["timeout_ms"],
                "oneOf": [
                    { "required": ["child_id"], "not": { "required": ["child_ids"] } },
                    { "required": ["child_ids"], "not": { "required": ["child_id"] } }
                ],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let target = parse_wait_target(&arguments)?;
            let timeout_ms = arguments
                .get("timeout_ms")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing or invalid required field: timeout_ms".into(),
                    )
                })?;
            match target {
                WaitTarget::Single(child_id) => {
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    self.sender
                        .send(SupervisorMessage {
                            agent_id: AgentId(child_id.clone()),
                            priority: MessagePriority::Command,
                            payload: SupervisorPayload::WaitChild(
                                AgentId(child_id),
                                Duration::from_millis(timeout_ms),
                                result_tx,
                            ),
                        })
                        .await
                        .map_err(|_| {
                            simulacra_types::ToolError::ExecutionFailed(
                                "supervisor channel closed".into(),
                            )
                        })?;
                    let wait = result_rx.await.map_err(|_| {
                        simulacra_types::ToolError::ExecutionFailed(
                            "supervisor dropped wait_child_agent response channel".into(),
                        )
                    })?;
                    let wait = wait.map_err(simulacra_types::ToolError::ExecutionFailed)?;
                    Ok(wait_child_json(wait))
                }
                WaitTarget::Any(child_ids) => {
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    self.sender
                        .send(SupervisorMessage {
                            agent_id: AgentId(child_ids[0].clone()),
                            priority: MessagePriority::Command,
                            payload: SupervisorPayload::WaitChildren(
                                child_ids.into_iter().map(AgentId).collect(),
                                Duration::from_millis(timeout_ms),
                                result_tx,
                            ),
                        })
                        .await
                        .map_err(|_| {
                            simulacra_types::ToolError::ExecutionFailed(
                                "supervisor channel closed".into(),
                            )
                        })?;
                    let wait = result_rx.await.map_err(|_| {
                        simulacra_types::ToolError::ExecutionFailed(
                            "supervisor dropped wait_child_agent response channel".into(),
                        )
                    })?;
                    let wait = wait.map_err(simulacra_types::ToolError::ExecutionFailed)?;
                    Ok(wait_children_json(wait))
                }
            }
        })
    }
}

pub struct CloseChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for CloseChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "close_child_agent".to_string(),
            description: CLOSE_CHILD_AGENT_DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = parse_required_child_id(&arguments)?;
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::CloseChild(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let result = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped close_child_agent response channel".into(),
                )
            })?;
            result.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(serde_json::json!({
                "child_id": child_id,
                "status": "closed"
            }))
        })
    }
}

pub struct SteerChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for SteerChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "steer_child_agent".to_string(),
            description:
                "Queue additional instructions for a live child agent before its next model turn."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    },
                    "message": {
                        "type": "string",
                        "description": "Additional instruction to queue for the child agent"
                    }
                },
                "required": ["child_id", "message"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = arguments
                .get("child_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: child_id".into(),
                    )
                })?
                .to_string();
            let message = arguments
                .get("message")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: message".into(),
                    )
                })?
                .to_string();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::SteerChild(
                        AgentId(child_id.clone()),
                        message,
                        result_tx,
                    ),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let result = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped steer response channel".into(),
                )
            })?;
            result.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(serde_json::json!({
                "child_id": child_id,
                "status": "queued"
            }))
        })
    }
}

pub struct CancelChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for CancelChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "cancel_child_agent".to_string(),
            description: "Request cancellation for a live child agent.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional cancellation reason"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = arguments
                .get("child_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: child_id".into(),
                    )
                })?
                .to_string();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Signal,
                    payload: SupervisorPayload::CancelChild(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let result = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped cancel response channel".into(),
                )
            })?;
            result.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(serde_json::json!({
                "child_id": child_id,
                "status": "cancel_requested"
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// Argument parsing helpers (shared by the child-control tools)
// ---------------------------------------------------------------------------

pub(crate) fn parse_required_child_id(arguments: &serde_json::Value) -> Result<String, ToolError> {
    arguments
        .get("child_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidArguments("missing required field: child_id".into()))
}

enum WaitTarget {
    Single(String),
    Any(Vec<String>),
}

fn parse_wait_target(arguments: &serde_json::Value) -> Result<WaitTarget, ToolError> {
    let has_child_id = arguments.get("child_id").is_some();
    let has_child_ids = arguments.get("child_ids").is_some();
    if has_child_id == has_child_ids {
        return Err(ToolError::InvalidArguments(
            "provide exactly one of child_id or child_ids".into(),
        ));
    }

    if has_child_id {
        return parse_required_child_id(arguments).map(WaitTarget::Single);
    }

    let child_ids = arguments
        .get("child_ids")
        .and_then(|value| value.as_array())
        .filter(|values| !values.is_empty())
        .ok_or_else(|| {
            ToolError::InvalidArguments("missing or invalid required field: child_ids".into())
        })?;

    let mut parsed = Vec::with_capacity(child_ids.len());
    for child_id in child_ids {
        let child_id = child_id
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArguments("child_ids must contain non-empty strings".into())
            })?;
        parsed.push(child_id.to_string());
    }

    Ok(WaitTarget::Any(parsed))
}
