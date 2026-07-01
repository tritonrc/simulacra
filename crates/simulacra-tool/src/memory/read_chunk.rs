//! `memory_read_chunk` tool implementation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_hooks::{HookError, HookPipeline, Operation, Verdict};
use simulacra_memory::{HitIdCache, MemoryStore, VectorIndex};
use simulacra_types::{
    CapabilityToken, HitId, MemoryCapability, TenantId, Tool, ToolDefinition, ToolError,
};

// ─── memory_read_chunk ───────────────────────────────────────────────────────

/// The `memory_read_chunk` tool — resolves a hit id to full chunk content,
/// with a TOCTOU guard against stale/deleted paths.
pub struct MemoryReadChunkTool {
    tenant: TenantId,
    capability: MemoryCapability,
    memory_store: Arc<dyn MemoryStore>,
    vector_index: Arc<dyn VectorIndex>,
    hit_cache: Arc<HitIdCache>,
    /// Governance hook pipeline consulted at tool_call before/after phases.
    /// Owned here (not via `ToolRegistry`) so deny yields the spec-mandated
    /// `{ error: "denied", code: 403 }` shape rather than a generic error.
    hook_pipeline: Option<Arc<HookPipeline>>,
}

impl MemoryReadChunkTool {
    pub fn new(
        tenant: TenantId,
        capability: MemoryCapability,
        memory_store: Arc<dyn MemoryStore>,
        vector_index: Arc<dyn VectorIndex>,
        hit_cache: Arc<HitIdCache>,
        hook_pipeline: Option<Arc<HookPipeline>>,
    ) -> Self {
        Self {
            tenant,
            capability,
            memory_store,
            vector_index,
            hit_cache,
            hook_pipeline,
        }
    }
}

impl Tool for MemoryReadChunkTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_read_chunk".into(),
            description: "Retrieve the full text of a chunk previously returned by \
                semantic_search."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "hit_id": { "type": "string" }
                },
                "required": ["hit_id"]
            }),
        }
    }

    /// Memory tools own their own hook lifecycle per S037 §20.
    fn handles_own_hooks(&self) -> bool {
        true
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            // Per S037 §18 — explicit named span for memory_read_chunk.
            let span = tracing::info_span!(
                "memory_read_chunk",
                memory.tenant = self.tenant.as_str(),
                memory.path = tracing::field::Empty,
                memory.chunk_index = tracing::field::Empty,
                memory.version = tracing::field::Empty,
                memory.outcome = tracing::field::Empty,
                // Per S037 §20 line 614/1174: hook-denied reads flip this
                // alongside the outcome field.
                memory.read_chunk.denied = tracing::field::Empty,
            );
            let _enter = span.enter();

            let hit_id_str = args
                .get("hit_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("missing field: hit_id".into()))?;
            let hit_id = HitId(hit_id_str.to_string());

            // ── Resolve hit id ──
            let entry = match self.hit_cache.get(&hit_id) {
                Some(entry) => entry,
                None => {
                    span.record("memory.outcome", "hit_not_found");
                    return Ok(json!({
                        "error": "hit_not_found",
                        "code": 404,
                    }));
                }
            };

            // Tenant isolation: a hit issued for a different tenant cannot
            // be resolved through this tool (the tool is constructed with
            // a fixed tenant). Treat as not found to avoid leaking state.
            // Hook is NOT consulted — capability/tenancy is the outer ring.
            if entry.tenant != self.tenant {
                span.record("memory.outcome", "tenant_mismatch");
                return Ok(json!({
                    "error": "hit_not_found",
                    "code": 404,
                }));
            }

            // Belt-and-braces capability check — the hit should only have
            // been issued for an in-scope path, but verify again here in
            // case the capability was attenuated since minting. Hook NOT
            // consulted: capability is the outer ring; hooks are inner.
            if !self.capability.can_read(&entry.path) {
                span.record("memory.outcome", "out_of_scope");
                return Ok(json!({
                    "error": "hit_not_found",
                    "code": 404,
                }));
            }

            // ── TOCTOU guard ──
            // Also outer to the hook: stale/deleted hits are reported
            // regardless of governance policy.
            let current = self
                .memory_store
                .current_version(&entry.tenant, &entry.path)
                .map_err(|e| ToolError::ExecutionFailed(format!("current_version failed: {e}")))?;
            match current {
                None => {
                    span.record("memory.outcome", "chunk_deleted");
                    return Ok(json!({
                        "error": "chunk_deleted",
                        "code": 410,
                        "path": entry.path.as_str(),
                    }));
                }
                Some(current_version) if current_version > entry.version => {
                    span.record("memory.outcome", "chunk_stale");
                    return Ok(json!({
                        "error": "chunk_stale",
                        "code": 410,
                        "path": entry.path.as_str(),
                        "hint": "re-run semantic_search",
                    }));
                }
                Some(_) => {}
            }

            // Build the arguments the hook sees: both the original hit id
            // and the resolved coordinates, so governance can audit the
            // concrete target of the fetch. The resolved path is NOT yet
            // written to the span — per S037 §20 line 1101 ("span attributes
            // are post-hook"), a denying hook must be able to prevent the
            // path from surfacing in traces. Span coords are recorded below
            // only if the before-phase clears.
            let before_args = json!({
                "hit_id": hit_id.0,
                "path": entry.path.as_str(),
                "chunk_index": entry.chunk_index,
                "version": entry.version.0,
            });

            // ── Before-phase hook ──
            if let Some(ref pipeline) = self.hook_pipeline {
                let before_ctx = json!({
                    "tool": "memory_read_chunk",
                    "arguments": &before_args,
                })
                .to_string();
                match pipeline.run_before(Operation::ToolCall, &before_ctx) {
                    Ok((Verdict::Deny(_), _)) => {
                        // Record only `memory.outcome` + `memory.read_chunk.denied`
                        // per S037 §20 — no path/chunk/version so the denied
                        // target does not land in traces.
                        span.record("memory.outcome", "denied");
                        span.record("memory.read_chunk.denied", true);
                        return Ok(json!({
                            "error": "denied",
                            "code": 403,
                        }));
                    }
                    Ok((Verdict::Kill(_), _)) => {
                        return Err(ToolError::ExecutionFailed(
                            "hook kill: memory_read_chunk".into(),
                        ));
                    }
                    Ok((Verdict::Continue(_), _modified_ctx)) => {
                        // The hook may rewrite `arguments` for audit purposes
                        // but the tool continues to fetch using the resolved
                        // coordinates — governance does not redirect reads
                        // (TOCTOU would be bypassed if the hook could redirect
                        // the fetch to a different version or path).
                    }
                    Err(HookError::Killed { hook, reason }) => {
                        return Err(ToolError::ExecutionFailed(format!(
                            "hook kill: {hook}: {reason}"
                        )));
                    }
                    Err(e) => {
                        return Err(ToolError::ExecutionFailed(format!("hook error: {e}")));
                    }
                }
            }

            // Hook has cleared (or no hook was installed) — now record the
            // resolved coordinates. These may be overwritten by the after-hook
            // re-record below if the hook redacts `result.path`.
            span.record("memory.path", entry.path.as_str());
            span.record("memory.chunk_index", entry.chunk_index as i64);
            span.record("memory.version", entry.version.0 as i64);

            // ── Fetch chunk ──
            let chunk = self
                .vector_index
                .get_chunk(&entry.tenant, &entry.path, entry.version, entry.chunk_index)
                .map_err(|e| ToolError::ExecutionFailed(format!("get_chunk failed: {e}")))?;

            let (locator, content) = match chunk {
                Some(c) => c,
                None => {
                    span.record("memory.outcome", "chunk_unavailable");
                    return Ok(json!({
                        "error": "chunk_unavailable",
                        "code": 503,
                        "hint": "reindex in progress",
                    }));
                }
            };

            let mut result_json = json!({
                "path": entry.path.as_str(),
                "locator": locator,
                "content": content,
            });

            // ── After-phase hook ──
            if let Some(ref pipeline) = self.hook_pipeline {
                let after_ctx = json!({
                    "tool": "memory_read_chunk",
                    "arguments": &before_args,
                    "result": &result_json,
                })
                .to_string();
                match pipeline.run_after(Operation::ToolCall, &after_ctx) {
                    Ok((_verdict, modified_ctx)) => {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&modified_ctx)
                            && let Some(new_result) = parsed.get("result")
                        {
                            result_json = new_result.clone();
                        }
                    }
                    Err(e) => {
                        return Err(ToolError::ExecutionFailed(format!("hook error: {e}")));
                    }
                }
            }

            // Re-record span attrs from the post-hook result so redactions
            // (e.g. path rewrites) are reflected in observability.
            if let Some(path_str) = result_json.get("path").and_then(Value::as_str) {
                span.record("memory.path", path_str);
            }
            span.record("memory.outcome", "ok");

            Ok(result_json)
        })
    }
}
