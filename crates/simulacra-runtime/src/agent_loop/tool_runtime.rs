use super::*;
use tracing::Instrument;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolExecutionResult {
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) cancelled: bool,
}

impl ToolExecutionResult {
    fn new(content: String, is_error: bool) -> Self {
        Self {
            content,
            is_error,
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            content: "cancelled by user".into(),
            is_error: true,
            cancelled: true,
        }
    }
}

pub(super) struct ToolCallRuntime {
    tools: Arc<ToolRegistry>,
    capability: CapabilityToken,
    agent_name: String,
    cancellation: Option<crate::CancellationToken>,
}

impl ToolCallRuntime {
    pub(super) fn new(
        tools: Arc<ToolRegistry>,
        capability: CapabilityToken,
        agent_name: String,
        cancellation: Option<crate::CancellationToken>,
    ) -> Self {
        Self {
            tools,
            capability,
            agent_name,
            cancellation,
        }
    }

    pub(super) fn supports_parallel_batch(
        &self,
        calls: &[simulacra_types::ToolCallMessage],
    ) -> bool {
        calls.len() > 1
            && calls.iter().all(|call| {
                self.tools
                    .metadata(&call.name)
                    .is_some_and(|metadata| metadata.supports_parallel_tool_calls)
            })
    }

    pub(super) async fn execute_one(
        &self,
        call: &simulacra_types::ToolCallMessage,
    ) -> ToolExecutionResult {
        let handle = self.spawn_tool(call.clone());
        self.await_tool(call, handle).await
    }

    pub(super) async fn execute_parallel_batch(
        &self,
        calls: &[simulacra_types::ToolCallMessage],
    ) -> Vec<ToolExecutionResult> {
        let handles = calls
            .iter()
            .cloned()
            .map(|call| self.spawn_tool(call))
            .collect::<Vec<_>>();

        let mut results = Vec::with_capacity(calls.len());
        for (call, handle) in calls.iter().zip(handles) {
            results.push(self.await_tool(call, handle).await);
        }
        results
    }

    fn spawn_tool(
        &self,
        call: simulacra_types::ToolCallMessage,
    ) -> tokio::task::JoinHandle<ToolExecutionResult> {
        let tools = Arc::clone(&self.tools);
        let capability = self.capability.clone();
        let agent_name = self.agent_name.clone();
        let span = tracing::Span::current();
        tokio::spawn(
            async move {
                let (content, is_error) =
                    execute_tool_live(tools.as_ref(), &call, &capability, &agent_name).await;
                ToolExecutionResult::new(content, is_error)
            }
            .instrument(span),
        )
    }

    async fn await_tool(
        &self,
        call: &simulacra_types::ToolCallMessage,
        mut handle: tokio::task::JoinHandle<ToolExecutionResult>,
    ) -> ToolExecutionResult {
        let Some(cancellation) = self.cancellation.clone() else {
            return Self::join_tool(handle.await);
        };

        if cancellation.is_cancelled() {
            return self.cancel_tool(call, handle).await;
        }

        tokio::select! {
            result = &mut handle => Self::join_tool(result),
            () = wait_for_cancellation(cancellation) => self.cancel_tool(call, handle).await,
        }
    }

    async fn cancel_tool(
        &self,
        call: &simulacra_types::ToolCallMessage,
        handle: tokio::task::JoinHandle<ToolExecutionResult>,
    ) -> ToolExecutionResult {
        let waits_for_cleanup = self
            .tools
            .metadata(&call.name)
            .is_some_and(|metadata| metadata.waits_for_runtime_cancellation);

        if waits_for_cleanup {
            let _ = handle.await;
        } else {
            handle.abort();
            let _ = handle.await;
        }

        ToolExecutionResult::cancelled()
    }

    fn join_tool(
        result: Result<ToolExecutionResult, tokio::task::JoinError>,
    ) -> ToolExecutionResult {
        match result {
            Ok(result) => result,
            Err(err) => ToolExecutionResult::new(format!("tool task failed: {err}"), true),
        }
    }
}

async fn wait_for_cancellation(cancellation: crate::CancellationToken) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
