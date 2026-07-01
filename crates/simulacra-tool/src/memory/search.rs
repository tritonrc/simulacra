//! `semantic_search` tool implementation.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use simulacra_hooks::{HookError, HookPipeline, Operation, Verdict};
use simulacra_memory::{
    Embedder, HitIdCache, RecentWritesBuffer, SearchHit, ToolSearchHit, VectorIndex,
};
use simulacra_types::{
    CapabilityToken, HitId, MEMORY_SNIPPET_CHARS, MemoryCapability, MemoryPath, TenantId, Tool,
    ToolDefinition, ToolError,
};

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
