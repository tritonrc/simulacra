use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use simulacra_types::{
    ActivityEvent, CapabilityToken, Tool, ToolDefinition, ToolError, ToolOutput, ToolSchema,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::activity_sink::ActivitySink;

pub const REQUEST_INPUT_TOOL_NAME: &str = "request_input";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApprovalResponse {
    pub tool_call_id: String,

    pub approved: bool,
    pub reason: Option<String>,
}

#[derive(Clone)]
pub struct AgentHitlRuntime {
    inner: Arc<AgentHitlInner>,
}

struct AgentHitlInner {
    input_rx: AsyncMutex<mpsc::Receiver<String>>,
    approval_rx: AsyncMutex<mpsc::Receiver<ToolApprovalResponse>>,
    require_tool_approval: bool,
}

impl AgentHitlRuntime {
    pub fn new(
        input_rx: mpsc::Receiver<String>,
        approval_rx: mpsc::Receiver<ToolApprovalResponse>,
        require_tool_approval: bool,
    ) -> Self {
        Self {
            inner: Arc::new(AgentHitlInner {
                input_rx: AsyncMutex::new(input_rx),
                approval_rx: AsyncMutex::new(approval_rx),
                require_tool_approval,
            }),
        }
    }

    pub fn channel_pair(require_tool_approval: bool) -> (AgentHitlSenders, Self) {
        let (input_tx, input_rx) = mpsc::channel(8);
        let (approval_tx, approval_rx) = mpsc::channel(8);
        (
            AgentHitlSenders {
                input_tx,
                approval_tx,
            },
            Self::new(input_rx, approval_rx, require_tool_approval),
        )
    }

    pub fn require_tool_approval(&self) -> bool {
        self.inner.require_tool_approval
    }

    pub async fn recv_input(&self) -> Option<String> {
        self.inner.input_rx.lock().await.recv().await
    }

    pub async fn recv_approval(&self) -> Option<ToolApprovalResponse> {
        self.inner.approval_rx.lock().await.recv().await
    }
}

#[derive(Clone)]
pub struct AgentHitlSenders {
    pub input_tx: mpsc::Sender<String>,
    pub approval_tx: mpsc::Sender<ToolApprovalResponse>,
}

pub struct RequestInputTool {
    hitl: AgentHitlRuntime,
    sink: Arc<dyn ActivitySink>,
}

impl RequestInputTool {
    pub fn new(hitl: AgentHitlRuntime, sink: Arc<dyn ActivitySink>) -> Self {
        Self { hitl, sink }
    }
}

impl Tool for RequestInputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: REQUEST_INPUT_TOOL_NAME.into(),
            description: "Ask the user for additional input and wait for their response.".into(),
            input_schema: ToolSchema::object(
                [
                    (
                        "prompt",
                        ToolSchema::string("Prompt shown to the user when requesting input."),
                    ),
                    (
                        "schema",
                        serde_json::json!({
                            "type": "object",
                            "description": "Optional JSON schema describing the expected response."
                        }),
                    ),
                ],
                ["prompt"],
            ),
        }
    }

    fn call(
        &self,
        arguments: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let prompt = arguments
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("missing required field: prompt".into())
                })?
                .to_string();
            let schema = arguments.get("schema").cloned();

            self.sink
                .emit(ActivityEvent::InputRequired { prompt, schema });

            let response = self.hitl.recv_input().await.ok_or_else(|| {
                ToolError::ExecutionFailed("input response channel closed".into())
            })?;
            Ok(ToolOutput::success(response).to_value())
        })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        false
    }
}
