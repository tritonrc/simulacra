//! Memory tools: `semantic_search` and `memory_read_chunk`.
//!
//! These tools implement the retrieval surface described in S037 §9. They are
//! opt-in per agent via [`simulacra_types::MemoryCapability`]. Registration is
//! handled by [`register_memory_tools`] — `register_builtins` does NOT register
//! them.
//!
//! See the spec for the full contract; the high-level flow for
//! `semantic_search` is:
//!
//! 1. Parse the `scope` argument as a [`MemoryPath`], rejecting traversal.
//! 2. Check the scope against `MemoryCapability.search_scopes`. A scope
//!    outside the grant returns `{hits: []}` (no error, to avoid leaking the
//!    shape of the grant).
//! 3. Embed the query and call [`VectorIndex::search`].
//! 4. Consult the per-run [`RecentWritesBuffer`] (if wired) and merge
//!    persistent + RRWB hits: RRWB first (strictly newer), deduped by path,
//!    persistent hits sorted by cosine score, truncated to `k`.
//! 5. Mint a [`HitId`] for each surviving hit via [`HitIdCache::mint`] and
//!    return a list of `ToolSearchHit` as JSON.
//!
//! For `memory_read_chunk`:
//!
//! 1. Resolve the `hit_id` in the [`HitIdCache`]. Missing/expired → 404.
//! 2. TOCTOU guard: check the current path version in the `MemoryStore`. If
//!    the path is gone → 410 `chunk_deleted`. If the current version is
//!    higher than the cached one → 410 `chunk_stale`.
//! 3. Fetch the chunk via [`VectorIndex::get_chunk`] and return the content.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use simulacra_hooks::{HookError, HookPipeline, Operation, Verdict};
use simulacra_memory::{
    Embedder, HitIdCache, MemoryStore, RecentWritesBuffer, SearchHit, ToolSearchHit, VectorIndex,
};
use simulacra_types::{
    CapabilityToken, HitId, MEMORY_SNIPPET_CHARS, MemoryCapability, MemoryPath, TenantId, Tool,
    ToolDefinition, ToolError,
};

use crate::ToolRegistry;

// ─── semantic_search ─────────────────────────────────────────────────────────

/// The `semantic_search` tool — top-K chunk retrieval scoped to a VFS subtree
/// the agent has read access to.
pub struct SemanticSearchTool {
    tenant: TenantId,
    capability: MemoryCapability,
    vector_index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    hit_cache: Arc<HitIdCache>,
    /// Per-run recent-writes buffer. If `None`, the tool falls back to
    /// persistent-index only — the run will see its own writes only after the
    /// background embedder catches up (Guarantee 3). When wired, the same-run
    /// read-your-writes contract of Guarantee 2 holds.
    rrwb: Option<Arc<Mutex<RecentWritesBuffer>>>,
    /// Governance hook pipeline consulted at tool_call before/after phases.
    /// Memory tools own their hook invocation (not via `ToolRegistry`'s
    /// generic wrapper) so deny can map to `{hits: []}` and after-phase
    /// redactions can land in post-hook span attributes — see S037 §20.
    hook_pipeline: Option<Arc<HookPipeline>>,
}

impl SemanticSearchTool {
    pub fn new(
        tenant: TenantId,
        capability: MemoryCapability,
        vector_index: Arc<dyn VectorIndex>,
        embedder: Arc<dyn Embedder>,
        hit_cache: Arc<HitIdCache>,
        rrwb: Option<Arc<Mutex<RecentWritesBuffer>>>,
        hook_pipeline: Option<Arc<HookPipeline>>,
    ) -> Self {
        Self {
            tenant,
            capability,
            vector_index,
            embedder,
            hit_cache,
            rrwb,
            hook_pipeline,
        }
    }

    /// Default parameter values matching the spec contract.
    const DEFAULT_K: usize = 5;
    const MAX_K: usize = 20;
    const MAX_QUERY_LEN: usize = 2048;
    /// Cosine similarity is bounded to `[-1.0, 1.0]` by definition. S037 §9.3:
    /// `min_cosine` outside this range is clamped so pathological values
    /// cannot be used to force always-empty or always-match behavior.
    const MIN_COSINE_LOWER: f32 = -1.0;
    const MIN_COSINE_UPPER: f32 = 1.0;
}

fn empty_hits() -> Value {
    json!({ "hits": [] })
}

fn search_hit_to_json(hit: &ToolSearchHit) -> Value {
    json!({
        "hit_id": hit.hit_id.0,
        "path": hit.path.as_str(),
        "snippet": hit.snippet,
        "locator": hit.locator,
        "cosine_score": hit.cosine_score,
    })
}

impl Tool for SemanticSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "semantic_search".into(),
            description: "Retrieve the top-K most relevant memory chunks matching a query, \
                scoped to a VFS subtree the agent has read access to."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language query (max 2048 characters).",
                        "maxLength": Self::MAX_QUERY_LEN,
                    },
                    "scope": {
                        "type": "string",
                        "description": "MemoryPath prefix, e.g. /var/memory/self/ or /mnt/policies/",
                    },
                    "k": {
                        "type": "integer",
                        "description": "Number of hits to return (default 5, max 20).",
                        "default": Self::DEFAULT_K,
                        "maximum": Self::MAX_K,
                    },
                    "min_cosine": {
                        "type": "number",
                        "description": "Floor on cosine similarity (default 0.0, range [-1.0, 1.0]).",
                        "default": 0.0,
                        "minimum": Self::MIN_COSINE_LOWER,
                        "maximum": Self::MIN_COSINE_UPPER,
                    }
                },
                "required": ["query", "scope"]
            }),
        }
    }

    /// Memory tools own their own hook lifecycle per S037 §20 — the generic
    /// before/after wrapper in `ToolRegistry::call` is bypassed so deny
    /// verdicts can map to `{hits: []}` and after-phase redactions can be
    /// reflected in span attributes.
    fn handles_own_hooks(&self) -> bool {
        true
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            // Per S037 §18 — explicit named span for memory_search with the
            // attributes the spec calls out (query length, scope, k,
            // hit_count, top_score, denied). Wraps the entire tool body so
            // operators can drill from agent activity → memory queries.
            let span = tracing::info_span!(
                "memory_search",
                memory.scope = tracing::field::Empty,
                memory.query_length = tracing::field::Empty,
                memory.k = tracing::field::Empty,
                memory.min_cosine = tracing::field::Empty,
                memory.hit_count = tracing::field::Empty,
                memory.top_score = tracing::field::Empty,
                // Per S037 §9.3 design note: `memory.query` and
                // `memory.hit_paths` are recorded POST-hook so a redacting
                // hook's output (not the raw pre-hook data) lands in traces.
                // This is the no-DLP-bypass guarantee (§20 line 1184).
                memory.query = tracing::field::Empty,
                memory.hit_paths = tracing::field::Empty,
                // Per S037 §20 line 1170: denied hooks flip this flag.
                memory.search.denied = tracing::field::Empty,
                // Per S037 §13 line 1144: true when the tenant has
                // pending reindex work in memory_embed_backlog. Recorded
                // unconditionally so span consumers can rely on the
                // field being present.
                memory.search.reindexing = tracing::field::Empty,
                memory.tenant = self.tenant.as_str(),
            );
            let _enter = span.enter();

            // Record reindexing-pending flag up-front so it appears on the
            // span even if the tool errors later. Defaults to false on
            // backlog_count failure — we don't want a gauge-read error to
            // claim the tenant is healthy when it isn't, but we also don't
            // want to block the search; the gauge itself will surface the
            // failure.
            let reindexing = self
                .vector_index
                .backlog_count(&self.tenant)
                .map(|n| n > 0)
                .unwrap_or(false);
            span.record("memory.search.reindexing", reindexing);

            // ── Parse arguments ──
            let initial_query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("missing field: query".into()))?
                .to_string();
            if initial_query.chars().count() > Self::MAX_QUERY_LEN {
                return Err(ToolError::InvalidArguments(format!(
                    "query too long: max {} chars",
                    Self::MAX_QUERY_LEN
                )));
            }
            span.record("memory.query_length", initial_query.chars().count());

            let initial_scope_str = args
                .get("scope")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("missing field: scope".into()))?
                .to_string();

            let initial_scope = match MemoryPath::parse(&initial_scope_str) {
                Ok(p) => p,
                Err(e) => {
                    return Err(ToolError::InvalidArguments(format!("invalid scope: {e}")));
                }
            };
            span.record("memory.scope", initial_scope.as_str());

            let initial_k = args
                .get("k")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(Self::DEFAULT_K)
                .clamp(1, Self::MAX_K);
            span.record("memory.k", initial_k);

            let initial_min_cosine = args
                .get("min_cosine")
                .and_then(Value::as_f64)
                .map(|v| (v as f32).clamp(Self::MIN_COSINE_LOWER, Self::MIN_COSINE_UPPER));
            if let Some(mc) = initial_min_cosine {
                span.record("memory.min_cosine", mc as f64);
            }

            // Assemble the "effective" arguments the tool will use. The
            // before-phase hook may mutate any of these fields; if so, we
            // reparse and re-record span attrs before the capability gate.
            let mut effective_args = json!({
                "query": initial_query,
                "scope": initial_scope.as_str(),
                "k": initial_k,
                "min_cosine": initial_min_cosine,
            });
            let mut query = initial_query;
            let mut scope = initial_scope;
            let mut k = initial_k;
            let mut min_cosine = initial_min_cosine;

            // ── Before-phase hook ──
            if let Some(ref pipeline) = self.hook_pipeline {
                let before_ctx = json!({
                    "tool": "semantic_search",
                    "arguments": &effective_args,
                })
                .to_string();
                match pipeline.run_before(Operation::ToolCall, &before_ctx) {
                    Ok((Verdict::Deny(_), _)) => {
                        span.record("memory.search.denied", true);
                        span.record("memory.hit_count", 0_i64);
                        return Ok(empty_hits());
                    }
                    Ok((Verdict::Kill(_), _)) => {
                        // `run_before` normally returns Kill as Err; keep this
                        // branch defensively.
                        return Err(ToolError::ExecutionFailed(
                            "hook kill: semantic_search".into(),
                        ));
                    }
                    Ok((Verdict::Continue(_), modified_ctx)) => {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&modified_ctx)
                            && let Some(new_args) = parsed.get("arguments")
                        {
                            effective_args = new_args.clone();
                            if let Some(q) = effective_args.get("query").and_then(Value::as_str) {
                                if q.len() > Self::MAX_QUERY_LEN {
                                    return Err(ToolError::InvalidArguments(format!(
                                        "query too long: max {} chars",
                                        Self::MAX_QUERY_LEN
                                    )));
                                }
                                query = q.to_string();
                                span.record("memory.query_length", query.len());
                            }
                            if let Some(s) = effective_args.get("scope").and_then(Value::as_str) {
                                match MemoryPath::parse(s) {
                                    Ok(p) => {
                                        span.record("memory.scope", p.as_str());
                                        scope = p;
                                    }
                                    Err(e) => {
                                        return Err(ToolError::InvalidArguments(format!(
                                            "invalid scope: {e}"
                                        )));
                                    }
                                }
                            }
                            if let Some(new_k) = effective_args.get("k").and_then(Value::as_u64) {
                                k = (new_k as usize).clamp(1, Self::MAX_K);
                                span.record("memory.k", k);
                            }
                            if let Some(new_mc) =
                                effective_args.get("min_cosine").and_then(Value::as_f64)
                            {
                                let clamped = (new_mc as f32)
                                    .clamp(Self::MIN_COSINE_LOWER, Self::MIN_COSINE_UPPER);
                                min_cosine = Some(clamped);
                                span.record("memory.min_cosine", clamped as f64);
                            }
                        }
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

            // ── Capability gate ──
            // An out-of-scope request returns an empty hit list, not an
            // error. This avoids leaking the shape of the grant. The
            // denial is recorded in the span attribute. Capability denial
            // is an outer-ring boundary and does NOT consult the after-hook.
            if !self.capability.can_read(&scope) {
                span.record("memory.search.denied", true);
                span.record("memory.hit_count", 0_i64);
                return Ok(empty_hits());
            }

            // ── Embed query ──
            let embeddings = self
                .embedder
                .embed(&[query.as_str()])
                .map_err(|e| ToolError::ExecutionFailed(format!("embed failed: {e}")))?;
            let query_vec = embeddings
                .into_iter()
                .next()
                .ok_or_else(|| ToolError::ExecutionFailed("embedder returned no vector".into()))?;

            // ── Persistent index search ──
            let persistent_hits = self
                .vector_index
                .search(
                    &self.tenant,
                    &scope,
                    &query_vec,
                    self.embedder.id(),
                    k,
                    min_cosine,
                )
                .map_err(|e| ToolError::ExecutionFailed(format!("vector search failed: {e}")))?;

            // ── RRWB search (same-run read-your-writes) ──
            let rrwb_hits: Vec<SearchHit> = if let Some(ref buf) = self.rrwb {
                let guard = buf.lock().expect("rrwb poisoned");
                guard.search(&query, &scope)
            } else {
                Vec::new()
            };

            // ── Merge: RRWB first, then dedup-by-path persistent hits. ──
            // Per §7 Guarantee 2 point 6, RRWB hits are strictly newer and
            // always win on path collisions. Persistent hits are sorted by
            // cosine_score descending; the final list is truncated to `k`.
            let mut merged: Vec<SearchHit> =
                Vec::with_capacity(rrwb_hits.len() + persistent_hits.len());
            let rrwb_paths: std::collections::HashSet<_> =
                rrwb_hits.iter().map(|h| h.path.clone()).collect();
            merged.extend(rrwb_hits);

            let mut persistent_sorted = persistent_hits;
            persistent_sorted.sort_by(|a, b| b.cosine_score.total_cmp(&a.cosine_score));
            for hit in persistent_sorted {
                if rrwb_paths.contains(&hit.path) {
                    continue;
                }
                merged.push(hit);
            }
            merged.truncate(k);

            // ── Mint hit ids ──
            let tool_hits: Vec<ToolSearchHit> = merged
                .into_iter()
                .map(|hit| {
                    // Cap snippet length per MEMORY_SNIPPET_CHARS.
                    let snippet: String = hit.snippet.chars().take(MEMORY_SNIPPET_CHARS).collect();
                    let hit_id: HitId = self.hit_cache.mint(
                        self.tenant.clone(),
                        hit.path.clone(),
                        hit.chunk_index,
                        hit.version,
                    );
                    ToolSearchHit {
                        hit_id,
                        path: hit.path,
                        chunk_index: hit.chunk_index,
                        locator: hit.locator,
                        snippet,
                        cosine_score: hit.cosine_score,
                    }
                })
                .collect();

            let mut result_json = json!({
                "hits": tool_hits.iter().map(search_hit_to_json).collect::<Vec<_>>(),
            });

            // ── After-phase hook ──
            // Runs BEFORE we re-record span attrs so the span reflects the
            // post-hook payload (e.g. a redacting hook that drops a hit
            // should also drop the hit from `memory.hit_count`).
            if let Some(ref pipeline) = self.hook_pipeline {
                let after_ctx = json!({
                    "tool": "semantic_search",
                    "arguments": &effective_args,
                    "result": &result_json,
                })
                .to_string();
                match pipeline.run_after(Operation::ToolCall, &after_ctx) {
                    // Note: `run_after` demotes Deny to Continue (see
                    // pipeline.rs). Both arms converge on extracting the
                    // possibly-mutated `result`.
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

            // Record post-hook result attributes. Per S037 §9.3 and §20
            // line 1184, these reflect the post-hook values so a redacting
            // hook's output (not the raw pre-hook data) lands in traces.
            let post_hits = result_json.get("hits").and_then(Value::as_array);
            let post_hits_len = post_hits.map(|a| a.len()).unwrap_or(0);
            span.record("memory.hit_count", post_hits_len as i64);
            if let Some(top_score) = post_hits
                .and_then(|a| a.first())
                .and_then(|h| h.get("cosine_score"))
                .and_then(Value::as_f64)
            {
                span.record("memory.top_score", top_score);
            }
            // Post-hook query (what the hook let through, possibly mutated).
            if let Some(query) = effective_args.get("query").and_then(Value::as_str) {
                span.record("memory.query", query);
            }
            // Post-hook hit paths — what the agent actually saw.
            let hit_paths: Vec<&str> = post_hits
                .map(|a| {
                    a.iter()
                        .filter_map(|h| h.get("path").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            let hit_paths_json =
                serde_json::to_string(&hit_paths).unwrap_or_else(|_| "[]".to_string());
            span.record("memory.hit_paths", hit_paths_json.as_str());

            Ok(result_json)
        })
    }
}

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
