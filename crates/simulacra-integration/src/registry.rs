//! IntegrationRegistry — resolves env vars, manages credentials, background refresh.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use tracing::info;

use crate::metrics::IntegrationMeters;
use crate::types::*;

/// Registry of live integration credentials.
pub struct IntegrationRegistry {
    integrations: HashMap<String, Arc<IntegrationCredential>>,
    refresh_handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    meters: IntegrationMeters,
}

/// Resolve an environment variable value, returning an error if not set.
fn resolve_env(var_name: &str) -> Result<String, IntegrationError> {
    std::env::var(var_name).map_err(|_| IntegrationError::MissingEnvVar(var_name.to_string()))
}

/// Perform a single OAuth2 token refresh against the given token_url.
/// Returns (access_token, expires_at) on success.
async fn do_token_refresh(
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<(String, Instant), IntegrationError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| IntegrationError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        // Do not include the response body — it may contain sensitive fields
        // (e.g. error_description with credentials) from the OAuth2 server.
        return Err(IntegrationError::Http(format!(
            "token refresh failed with status {status}"
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| IntegrationError::Http(e.to_string()))?;

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| IntegrationError::Http("missing access_token in response".to_string()))?
        .to_string();

    let expires_in = body
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);

    let expires_at = Instant::now() + Duration::from_secs(expires_in);

    Ok((access_token, expires_at))
}

impl IntegrationRegistry {
    /// Create a registry from config, resolving all env var references.
    pub fn from_config(
        integrations: &HashMap<String, IntegrationConfig>,
    ) -> Result<Self, IntegrationError> {
        let mut resolved = HashMap::new();

        for (name, config) in integrations {
            let access_token = match &config.auth {
                AuthMethod::ApiKey { key, .. } => resolve_env(key)?,
                AuthMethod::OAuth2 {
                    client_id,
                    client_secret,
                    refresh_token,
                    ..
                } => {
                    // Resolve all env vars to verify they exist
                    let _id = resolve_env(client_id)?;
                    let _secret = resolve_env(client_secret)?;
                    if let Some(rt) = refresh_token {
                        let _rt_val = resolve_env(rt)?;
                    }
                    // For OAuth2, initial token will be obtained via refresh.
                    // For now, store empty — real token exchange requires HTTP.
                    String::new()
                }
            };

            let credential = Arc::new(IntegrationCredential {
                name: name.clone(),
                config: config.clone(),
                access_token: RwLock::new(access_token),
                expires_at: RwLock::new(None),
                degraded: AtomicBool::new(false),
                refresh_failures: AtomicU32::new(0),
                connectivity_ok: AtomicBool::new(true),
            });

            resolved.insert(name.clone(), credential);
        }

        let names: Vec<&str> = resolved.keys().map(|s| s.as_str()).collect();
        info!(
            integration_count = resolved.len(),
            integrations = ?names,
            "integration registry initialized"
        );

        // Collect all credentials for the observable gauge callback.
        let all_creds: Arc<Vec<Arc<IntegrationCredential>>> =
            Arc::new(resolved.values().cloned().collect());
        let meters = IntegrationMeters::new(all_creds);

        Ok(Self {
            integrations: resolved,
            refresh_handles: Mutex::new(Vec::new()),
            meters,
        })
    }

    /// Start background OAuth2 token refresh for all OAuth2 integrations that
    /// have a refresh_token configured.
    ///
    /// This method performs the initial token exchange synchronously (awaited)
    /// so that tokens are available immediately before returning. Background
    /// tasks are then spawned to refresh tokens before they expire.
    pub async fn start_background_refresh(&self) {
        for (name, cred) in &self.integrations {
            let (token_url, client_id_env, client_secret_env, refresh_token_env) =
                match &cred.config.auth {
                    AuthMethod::OAuth2 {
                        token_url,
                        client_id,
                        client_secret,
                        refresh_token: Some(rt),
                        ..
                    } => (
                        token_url.clone(),
                        client_id.clone(),
                        client_secret.clone(),
                        rt.clone(),
                    ),
                    _ => continue, // ApiKey or OAuth2 without refresh_token — skip
                };

            // Resolve env vars (already validated in from_config, so these should succeed).
            let client_id = match std::env::var(&client_id_env) {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(
                        integration = %name,
                        env_var = %client_id_env,
                        "could not resolve client_id env var for OAuth2 refresh"
                    );
                    continue;
                }
            };
            let client_secret = match std::env::var(&client_secret_env) {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(
                        integration = %name,
                        env_var = %client_secret_env,
                        "could not resolve client_secret env var for OAuth2 refresh"
                    );
                    continue;
                }
            };
            let refresh_token = match std::env::var(&refresh_token_env) {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(
                        integration = %name,
                        env_var = %refresh_token_env,
                        "could not resolve refresh_token env var for OAuth2 refresh"
                    );
                    continue;
                }
            };

            // Initial token exchange.
            let span = tracing::info_span!(
                "simulacra_integration_token_refresh",
                "simulacra.integration.name" = name.as_str()
            );
            let _enter = span.enter();

            match do_token_refresh(&token_url, &client_id, &client_secret, &refresh_token).await {
                Ok((token, expires_at)) => {
                    *cred.access_token.write().unwrap() = token;
                    *cred.expires_at.write().unwrap() = Some(expires_at);
                    cred.clear_degraded();
                    tracing::info!(
                        integration = %name,
                        "simulacra.integration.result" = "success",
                        "OAuth2 token refreshed successfully"
                    );
                }
                Err(e) => {
                    // Count initial failure toward the 3-strike degraded threshold so that
                    // initial(1) + retry1(1) + retry2(1) = 3 triggers degraded, matching the
                    // spec intent ("3 consecutive failures").
                    let failures = cred.increment_failures();
                    if failures >= 3 {
                        cred.mark_degraded();
                        tracing::error!(
                            integration = %name,
                            consecutive_failures = failures,
                            "integration marked degraded after consecutive refresh failures"
                        );
                    }
                    tracing::warn!(
                        integration = %name,
                        error = %e,
                        attempt = failures,
                        "simulacra.integration.result" = "failure",
                        "OAuth2 initial token exchange failed"
                    );
                }
            }
            drop(_enter);

            // Spawn background refresh task.
            let cred_clone = Arc::clone(cred);
            let name_clone = name.clone();
            let meters_refresh_failures = self.meters.refresh_failures.clone();

            let handle = tokio::spawn(async move {
                run_refresh_loop(
                    name_clone,
                    cred_clone,
                    token_url,
                    client_id,
                    client_secret,
                    refresh_token,
                    meters_refresh_failures,
                )
                .await;
            });

            self.refresh_handles
                .lock()
                .expect("refresh_handles mutex should not be poisoned")
                .push(handle);
        }
    }

    /// Probe connectivity for all integrations.
    /// Returns a map of integration name -> result. Failed connectivity
    /// is logged as warning but does not prevent startup.
    pub async fn test_connectivity(&self) -> HashMap<String, Result<(), IntegrationError>> {
        let mut results = HashMap::new();
        for (name, cred) in &self.integrations {
            // Simple connectivity check: try to reach base_url with HEAD request.
            let base_url = &cred.config.base_url;
            let result = match reqwest::Client::new()
                .head(base_url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
            {
                Ok(_) => {
                    cred.connectivity_ok.store(true, Ordering::Relaxed);
                    Ok(())
                }
                Err(e) => {
                    cred.connectivity_ok.store(false, Ordering::Relaxed);
                    tracing::warn!(
                        integration = %name,
                        error = %e,
                        "integration connectivity check failed"
                    );
                    Err(IntegrationError::Http(e.to_string()))
                }
            };
            results.insert(name.clone(), result);
        }
        results
    }

    /// Get the current access token for an integration.
    pub async fn access_token(&self, name: &str) -> Result<String, IntegrationError> {
        let cred = self
            .integrations
            .get(name)
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;

        if cred.is_degraded() {
            return Err(IntegrationError::TokenRefreshFailed(name.to_string()));
        }

        let token = cred.access_token.read().unwrap().clone();
        if token.is_empty() {
            return Err(IntegrationError::TokenRefreshFailed(name.to_string()));
        }
        Ok(token)
    }

    /// List all integration names.
    pub fn names(&self) -> Vec<String> {
        self.integrations.keys().cloned().collect()
    }

    /// Get non-secret metadata for an integration.
    pub fn metadata(&self, name: &str) -> Option<IntegrationMetadata> {
        let cred = self.integrations.get(name)?;
        let scopes = match &cred.config.auth {
            AuthMethod::OAuth2 { scopes, .. } => scopes.clone(),
            AuthMethod::ApiKey { .. } => vec![],
        };
        let status = if cred.is_degraded() {
            "degraded".to_string()
        } else if cred.connectivity_ok.load(Ordering::Relaxed) {
            "ok".to_string()
        } else {
            "unreachable".to_string()
        };

        Some(IntegrationMetadata {
            base_url: cred.config.base_url.clone(),
            scopes,
            rate_limit_rps: cred.config.rate_limit_rps,
            status,
            description: cred.config.description.clone(),
        })
    }

    /// Synchronously inject auth headers for an outbound URL, if it matches a
    /// configured integration that the tenant is granted access to.
    ///
    /// Returns `None` if no integration matches. This is the hot-path method
    /// called from the sync `FetchProxy::fetch` — it reads the token from a
    /// `RwLock` (no async, no network I/O).
    pub fn inject_headers_sync(
        &self,
        url: &str,
        tenant_integrations: &[String],
    ) -> Result<Option<Vec<(String, String)>>, IntegrationError> {
        use crate::injector::{parse_api_key_headers, url_matches_base};

        for (name, cred) in &self.integrations {
            if !url_matches_base(&cred.config.base_url, url) {
                continue;
            }
            if !tenant_integrations.iter().any(|t| t == name) {
                continue;
            }
            if cred.is_degraded() {
                return Err(IntegrationError::Degraded(format!(
                    "integration '{name}' is degraded"
                )));
            }
            let token = cred.access_token.read().unwrap().clone();
            if token.is_empty() {
                return Err(IntegrationError::TokenRefreshFailed(format!(
                    "integration '{name}' has no token"
                )));
            }
            tracing::debug!(
                integration = %name,
                url_host = url.split('/').nth(2).unwrap_or("unknown"),
                "credential injection"
            );
            let headers = match &cred.config.auth {
                AuthMethod::ApiKey { placement, .. } => parse_api_key_headers(placement, &token),
                AuthMethod::OAuth2 { .. } => {
                    vec![("Authorization".to_string(), format!("Bearer {token}"))]
                }
            };
            self.meters
                .credential_injections
                .add(1, &[KeyValue::new("integration", name.clone())]);
            return Ok(Some(headers));
        }
        Ok(None)
    }

    /// Cancel all background refresh tasks and wait for them to stop.
    ///
    /// Must be called before the tokio runtime is dropped to ensure background
    /// tasks are fully stopped (not just signalled).
    pub async fn shutdown(&self) {
        // Drain handles from the mutex so we own them — can't await while holding lock.
        let handles = {
            let mut lock = self
                .refresh_handles
                .lock()
                .expect("refresh_handles mutex should not be poisoned");
            std::mem::take(&mut *lock)
        };
        for handle in handles {
            handle.abort();
            // Await to confirm the task has actually stopped. AbortError is expected;
            // anything else (panic) is a bug — log it.
            match handle.await {
                Ok(()) => {}                     // Task finished normally before abort landed.
                Err(e) if e.is_cancelled() => {} // Expected: abort landed.
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "integration background refresh task panicked during shutdown"
                    );
                }
            }
        }
    }

    /// Get all integrations (for internal use by injector).
    pub(crate) fn all_integrations(&self) -> &HashMap<String, Arc<IntegrationCredential>> {
        &self.integrations
    }

    /// Get OTel instruments (for internal use by injector).
    pub(crate) fn meters(&self) -> &IntegrationMeters {
        &self.meters
    }
}

/// Background loop that refreshes the OAuth2 token for a single integration.
///
/// On each iteration:
/// 1. If the current token is empty (initial exchange failed) or within 5 minutes
///    of expiry, refresh immediately. Otherwise sleep until 5 minutes before expiry.
/// 2. Refresh with exponential backoff on failure (1s, 2s, 4s, 8s, max 60s).
/// 3. After 3 consecutive failures, mark the integration degraded.
/// 4. On success, clear degraded state and loop back to step 1.
async fn run_refresh_loop(
    name: String,
    cred: Arc<IntegrationCredential>,
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    refresh_failures_counter: opentelemetry::metrics::Counter<u64>,
) {
    loop {
        // Determine how long to sleep before the next refresh.
        let sleep_duration = {
            let token_empty = cred.access_token.read().unwrap().is_empty();
            if token_empty {
                // Initial exchange failed — retry immediately with no sleep.
                Duration::ZERO
            } else {
                let expires_at = *cred.expires_at.read().unwrap();
                match expires_at {
                    Some(expiry) => {
                        let now = Instant::now();
                        let five_minutes = Duration::from_secs(5 * 60);
                        if expiry <= now {
                            // Already expired — refresh immediately.
                            Duration::ZERO
                        } else if expiry > now + five_minutes {
                            // Long-lived token: wake up 5 min before expiry.
                            expiry - now - five_minutes
                        } else {
                            // Short-lived token (expiry within the 5-min window):
                            // sleep 90% of remaining lifetime so we refresh just before
                            // expiry rather than hammering the endpoint in a tight loop.
                            let remaining = expiry - now;
                            remaining * 9 / 10
                        }
                    }
                    None => {
                        // No expiry known — assume 60-minute token life, refresh at 55 minutes.
                        Duration::from_secs(55 * 60)
                    }
                }
            }
        };

        if !sleep_duration.is_zero() {
            tokio::time::sleep(sleep_duration).await;
        }

        // Attempt refresh with exponential backoff on failure.
        // Use cred.increment_failures() so the counter persists across the
        // initial exchange (in start_background_refresh) and these retries,
        // giving a true "N consecutive failures" count.
        let mut backoff = Duration::from_secs(1);

        loop {
            let span = tracing::info_span!(
                "simulacra_integration_token_refresh",
                "simulacra.integration.name" = name.as_str()
            );
            let _enter = span.enter();

            match do_token_refresh(&token_url, &client_id, &client_secret, &refresh_token).await {
                Ok((token, expires_at)) => {
                    *cred.access_token.write().unwrap() = token;
                    *cred.expires_at.write().unwrap() = Some(expires_at);
                    cred.clear_degraded(); // Also resets refresh_failures counter.
                    tracing::info!(
                        integration = %name,
                        "simulacra.integration.result" = "success",
                        "OAuth2 token refreshed successfully"
                    );
                    break; // Exit inner retry loop — go back to outer sleep loop.
                }
                Err(e) => {
                    let failures = cred.increment_failures();
                    refresh_failures_counter.add(1, &[KeyValue::new("integration", name.clone())]);
                    tracing::warn!(
                        integration = %name,
                        error = %e,
                        attempt = failures,
                        "simulacra.integration.result" = "failure",
                        "OAuth2 token refresh failed"
                    );

                    if failures >= 3 {
                        cred.mark_degraded();
                        tracing::error!(
                            integration = %name,
                            consecutive_failures = failures,
                            "integration marked degraded after consecutive refresh failures"
                        );
                    }

                    // Wait with exponential backoff before retrying (max 60s).
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }
}
