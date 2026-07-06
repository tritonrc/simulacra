use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use simulacra_types::{AgentId, CapabilityToken, ResourceBudget};

use crate::{ActivitySink, AgentLoopOutput, CancellationToken, RuntimeError};

/// A boxed ACP child runtime future.
pub type AcpChildFuture =
    Pin<Box<dyn Future<Output = Result<AgentLoopOutput, RuntimeError>> + Send + 'static>>;

/// Opaque request passed to an injected ACP child runtime.
#[derive(Debug, Clone)]
pub struct AcpChildRequest {
    pub child_id: AgentId,
    pub parent_id: AgentId,
    pub agent_type: String,
    pub acp_profile: String,
    pub task: String,
    pub budget: ResourceBudget,
    pub capability: CapabilityToken,
}

/// Runtime port for ACP-backed child agents.
///
/// Implementations are responsible for mapping `cancellation` into the ACP
/// session and returning a cancelled terminal `AgentLoopOutput` when the child
/// is cancelled.
pub trait AcpChildRuntime: Send + Sync + 'static {
    fn start_child(
        &self,
        request: AcpChildRequest,
        cancellation: CancellationToken,
        activity_sink: Arc<dyn ActivitySink>,
    ) -> AcpChildFuture;
}
