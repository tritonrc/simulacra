//! Memory tool registration.

use std::sync::{Arc, Mutex};

use simulacra_hooks::HookPipeline;
use simulacra_memory::{Embedder, HitIdCache, MemoryStore, RecentWritesBuffer, VectorIndex};
use simulacra_types::{MemoryCapability, TenantId};

use super::{MemoryReadChunkTool, SemanticSearchTool};
use crate::ToolRegistry;

// ─── register_memory_tools ───────────────────────────────────────────────────

/// Handle bundling everything needed to register the memory tools.
pub struct MemoryToolHandles {
    pub tenant: TenantId,
    pub capability: MemoryCapability,
    pub memory_store: Arc<dyn MemoryStore>,
    pub vector_index: Arc<dyn VectorIndex>,
    pub embedder: Arc<dyn Embedder>,
    pub hit_cache: Arc<HitIdCache>,
    pub rrwb: Option<Arc<Mutex<RecentWritesBuffer>>>,
    /// Governance hook pipeline. `None` skips hook interception entirely;
    /// `Some` threads before/after `tool_call` hooks through the memory
    /// tools with the graceful-deny shapes required by S037 §20.
    pub hook_pipeline: Option<Arc<HookPipeline>>,
}

/// Register [`SemanticSearchTool`] and [`MemoryReadChunkTool`] into the given
/// registry IF the supplied `MemoryCapability` has `enabled = true`.
///
/// When memory is disabled, this function is a no-op — the tools do not
/// appear in the registry and the agent cannot call them. See S037 §11
/// (opt-in default).
pub fn register_memory_tools(registry: &mut ToolRegistry, handles: MemoryToolHandles) {
    if !handles.capability.enabled {
        return;
    }
    let search = SemanticSearchTool::new(
        handles.tenant.clone(),
        handles.capability.clone(),
        Arc::clone(&handles.vector_index),
        Arc::clone(&handles.embedder),
        Arc::clone(&handles.hit_cache),
        handles.rrwb.clone(),
        handles.hook_pipeline.clone(),
    );
    let read = MemoryReadChunkTool::new(
        handles.tenant,
        handles.capability,
        handles.memory_store,
        handles.vector_index,
        handles.hit_cache,
        handles.hook_pipeline,
    );
    registry.register(Box::new(search));
    registry.register(Box::new(read));
}
