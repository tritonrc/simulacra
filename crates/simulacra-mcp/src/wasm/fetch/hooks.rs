use super::{FetchError, FetchRequest, FetchResponse};

/// Run `simulacra_hooks::HookPipeline::run_before` for an outbound fetch.
///
/// Serializes the [`FetchRequest`] to JSON (the universal hook context shape),
/// invokes the pipeline against `Operation::HttpRequest`, and reconstitutes
/// a possibly-redacted [`FetchRequest`] from `Verdict::Continue(Some(_))`.
/// Denial maps to `FetchError::HookDenied`; serialization failures map to
/// `FetchError::Transport` so callers see a typed error surface.
pub(crate) fn run_hook_phase_before(
    pipeline: &simulacra_hooks::HookPipeline,
    server: &str,
    fetch_span: &tracing::Span,
    request: FetchRequest,
) -> Result<FetchRequest, FetchError> {
    let request_json = serde_json::to_string(&request)
        .map_err(|e| FetchError::Transport(format!("hook serialize: {e}")))?;
    let (verdict, modified) = pipeline
        .run_before(simulacra_hooks::Operation::HttpRequest, &request_json)
        .map_err(|e| FetchError::HookDenied(e.to_string()))?;
    match verdict {
        simulacra_hooks::Verdict::Deny(reason) => {
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "hook-denied",
                "simulacra:http/fetch hook denial (Phase::Before)"
            );
            tracing::warn!(
                server = server,
                reason = %reason,
                "simulacra:http/fetch hook denial in Phase::Before"
            );
            fetch_span.record("http.response.status_code", 0_u64);
            Err(FetchError::HookDenied(reason))
        }
        simulacra_hooks::Verdict::Kill(reason) => {
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "hook-killed",
                "simulacra:http/fetch hook kill (Phase::Before)"
            );
            tracing::warn!(
                server = server,
                reason = %reason,
                "simulacra:http/fetch hook kill in Phase::Before"
            );
            fetch_span.record("http.response.status_code", 0_u64);
            Err(FetchError::HookDenied(format!("kill: {reason}")))
        }
        simulacra_hooks::Verdict::Continue(_) => {
            if modified == request_json {
                Ok(request)
            } else {
                serde_json::from_str(&modified)
                    .map_err(|e| FetchError::Transport(format!("hook deserialize: {e}")))
            }
        }
    }
}

/// Run `simulacra_hooks::HookPipeline::run_after`. Mirror of
/// [`run_hook_phase_before`] for the response side. After-phase denials are
/// downgraded to Continue inside the pipeline itself; a Kill from any hook
/// surfaces as [`FetchError::HookDenied`] to keep the FetchError surface
/// uniform.
pub(crate) fn run_hook_phase_after(
    pipeline: &simulacra_hooks::HookPipeline,
    server: &str,
    request: &FetchRequest,
    response: FetchResponse,
) -> Result<FetchResponse, FetchError> {
    let response_json = serde_json::to_string(&response)
        .map_err(|e| FetchError::Transport(format!("hook serialize: {e}")))?;
    let (verdict, modified) = pipeline
        .run_after(simulacra_hooks::Operation::HttpRequest, &response_json)
        .map_err(|e| FetchError::HookDenied(e.to_string()))?;
    match verdict {
        simulacra_hooks::Verdict::Kill(reason) => {
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "hook-killed",
                "simulacra:http/fetch hook kill (Phase::After)"
            );
            tracing::warn!(
                server = server,
                reason = %reason,
                method = %request.method,
                "simulacra:http/fetch hook kill in Phase::After"
            );
            Err(FetchError::HookDenied(format!("kill: {reason}")))
        }
        simulacra_hooks::Verdict::Deny(reason) => {
            // After-phase Deny is downgraded to Continue inside the
            // pipeline, but we surface it as a WARN for observability.
            tracing::warn!(
                server = server,
                reason = %reason,
                method = %request.method,
                "simulacra:http/fetch hook denial in Phase::After (downgraded to continue)"
            );
            if modified == response_json {
                Ok(response)
            } else {
                serde_json::from_str(&modified)
                    .map_err(|e| FetchError::Transport(format!("hook deserialize: {e}")))
            }
        }
        simulacra_hooks::Verdict::Continue(_) => {
            if modified == response_json {
                Ok(response)
            } else {
                serde_json::from_str(&modified)
                    .map_err(|e| FetchError::Transport(format!("hook deserialize: {e}")))
            }
        }
    }
}
