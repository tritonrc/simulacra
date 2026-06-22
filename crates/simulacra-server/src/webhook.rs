//! Webhook handler with HMAC-SHA256 validation and payload templating.

use std::sync::Arc;

use hmac::{Hmac, Mac};
use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{error, info, warn};

use opentelemetry::KeyValue;

use crate::engine::SimulacraEngine;
use crate::metrics::ServerMeters;
use crate::task::{TaskHandle, TaskManager};
use crate::tenant::TenantResolver;

type HmacSha256 = Hmac<Sha256>;

/// Configuration for a single webhook endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Unique name for this webhook (used in logs and spans).
    pub name: String,
    /// URL path at which this webhook is mounted (e.g. "/hooks/new-customer").
    pub path: String,
    /// Tenant namespace that tasks created by this webhook belong to.
    pub tenant: String,
    /// Task description template (supports `{{payload.field}}` substitution).
    pub task_template: String,
    /// Default agent type for tasks created by this webhook.
    pub agent_type: String,
    /// Name of the environment variable holding the HMAC secret.
    /// The secret is NEVER stored in config — only the env var name.
    pub secret: String,
}

/// Handles incoming webhook requests.
pub struct WebhookHandler {
    config: WebhookConfig,
}

/// Errors that can occur during webhook processing.
#[derive(Debug, Error)]
pub enum WebhookError {
    #[error("missing X-Simulacra-Signature header")]
    MissingSignature,

    #[error("invalid HMAC signature")]
    InvalidSignature,

    #[error("invalid JSON body: {0}")]
    InvalidBody(String),

    #[error("tenant '{0}' not found")]
    TenantNotFound(String),

    #[error("secret env var '{0}' not set")]
    SecretNotFound(String),

    #[error("task creation failed: {0}")]
    TaskCreationFailed(String),
}

impl WebhookHandler {
    pub fn new(config: WebhookConfig) -> Self {
        Self { config }
    }

    /// Validate the HMAC-SHA256 signature of the raw body.
    ///
    /// Header format: `X-Simulacra-Signature: sha256=<hex_digest>`
    ///
    /// Uses constant-time comparison (via `hmac::Mac::verify_slice`) to prevent
    /// timing attacks.
    pub fn validate_signature(
        &self,
        raw_body: &[u8],
        signature_header: Option<&str>,
    ) -> Result<(), WebhookError> {
        let header = signature_header.ok_or(WebhookError::MissingSignature)?;

        let hex_signature = header
            .strip_prefix("sha256=")
            .ok_or(WebhookError::InvalidSignature)?;

        // Read the secret from the environment variable (never from config directly).
        let secret = std::env::var(&self.config.secret).map_err(|_| {
            warn!(
                webhook_name = %self.config.name,
                env_var = %self.config.secret,
                "webhook secret env var not set"
            );
            WebhookError::SecretNotFound(self.config.secret.clone())
        })?;

        // Compute HMAC-SHA256 of the raw body.
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|_| WebhookError::InvalidSignature)?;
        mac.update(raw_body);

        // Decode the provided hex signature.
        let provided_bytes =
            hex::decode(hex_signature).map_err(|_| WebhookError::InvalidSignature)?;

        // Constant-time verification using hmac's built-in verify_slice.
        mac.verify_slice(&provided_bytes).map_err(|_| {
            warn!(webhook_name = %self.config.name, "HMAC signature mismatch");
            WebhookError::InvalidSignature
        })
    }

    /// Process a webhook request: validate, template, create task.
    ///
    /// Without an engine the webhook only creates a task record (no agent
    /// actually runs). This is the legacy path — tests that care only about
    /// HMAC/templating use it. Production callers should prefer
    /// [`process_with_engine`], which actually spawns an agent worker.
    pub fn process(
        &self,
        raw_body: &[u8],
        signature_header: Option<&str>,
        task_manager: &TaskManager,
        resolver: &TenantResolver,
    ) -> Result<TaskHandle, WebhookError> {
        self.process_inner(raw_body, signature_header, task_manager, resolver, None)
    }

    /// Process a webhook request and spawn an actual agent via the engine.
    ///
    /// Unlike [`process`], the task is dispatched through
    /// `SimulacraEngine::spawn_task`, which constructs the VFS, capability token,
    /// and tool registry, then runs the agent loop. This is the production
    /// path — a webhook without an engine creates a task record but never
    /// executes anything.
    pub async fn process_with_engine(
        &self,
        raw_body: &[u8],
        signature_header: Option<&str>,
        engine: &SimulacraEngine,
        task_manager: &TaskManager,
        resolver: &TenantResolver,
    ) -> Result<TaskHandle, WebhookError> {
        self.process_inner_async(raw_body, signature_header, engine, task_manager, resolver)
            .await
    }

    /// Shared synchronous pre-flight: HMAC check, JSON parse, templating.
    /// Returns the (tenant_config, description, metadata) triple ready for
    /// task creation.
    fn validate_and_build<'t>(
        &self,
        raw_body: &[u8],
        signature_header: Option<&str>,
        resolver: &'t TenantResolver,
    ) -> Result<(&'t crate::tenant::TenantConfig, String, serde_json::Value), WebhookError> {
        // 1. Validate HMAC signature.
        let sig_result = self.validate_signature(raw_body, signature_header);
        if sig_result.is_err() {
            ServerMeters::get().webhook_requests.add(
                1,
                &[
                    KeyValue::new("webhook_name", self.config.name.clone()),
                    KeyValue::new("tenant", self.config.tenant.clone()),
                    KeyValue::new("status", "auth_failure"),
                ],
            );
        }
        sig_result?;

        // 2. Parse JSON body.
        let payload: serde_json::Value = serde_json::from_slice(raw_body).map_err(|e| {
            ServerMeters::get().webhook_requests.add(
                1,
                &[
                    KeyValue::new("webhook_name", self.config.name.clone()),
                    KeyValue::new("tenant", self.config.tenant.clone()),
                    KeyValue::new("status", "parse_error"),
                ],
            );
            WebhookError::InvalidBody(e.to_string())
        })?;

        // 3. Template substitution to build the task description.
        let task_description = apply_payload_template(&self.config.task_template, &payload);

        // 4. Look up tenant config.
        let tenant = resolver.get(&self.config.tenant).ok_or_else(|| {
            error!(
                webhook_name = %self.config.name,
                tenant = %self.config.tenant,
                "webhook tenant not found"
            );
            WebhookError::TenantNotFound(self.config.tenant.clone())
        })?;

        // 5. Build task metadata (full payload + source info for audit trail).
        let payload_hash = hex::encode(sha2::Sha256::digest(raw_body));
        let metadata = serde_json::json!({
            "source": "webhook",
            "webhook_name": self.config.name,
            "payload": payload,
            "payload_hash": payload_hash,
        });

        Ok((tenant, task_description, metadata))
    }

    fn process_inner(
        &self,
        raw_body: &[u8],
        signature_header: Option<&str>,
        task_manager: &TaskManager,
        resolver: &TenantResolver,
        _engine: Option<Arc<SimulacraEngine>>,
    ) -> Result<TaskHandle, WebhookError> {
        let _span = tracing::info_span!(
            "simulacra_webhook_received",
            "simulacra.trigger.webhook_name" = self.config.name.as_str(),
            "simulacra.trigger.tenant" = self.config.tenant.as_str(),
        )
        .entered();

        let (tenant, task_description, metadata) =
            self.validate_and_build(raw_body, signature_header, resolver)?;

        // No engine — create a task record through TaskManager. Note that
        // this path is for tests and bring-up only: the task reaches the
        // Running state but no agent actually executes. Production servers
        // must route webhooks through `process_with_engine`.
        let handle = task_manager
            .create_task(
                tenant,
                task_description,
                Some(self.config.agent_type.clone()),
                metadata,
                None,
            )
            .map_err(|e| WebhookError::TaskCreationFailed(e.to_string()))?;

        info!(
            webhook_name = %self.config.name,
            tenant = %self.config.tenant,
            task_id = %handle.task_id,
            "webhook task created (record-only path)"
        );
        ServerMeters::get().webhook_requests.add(
            1,
            &[
                KeyValue::new("webhook_name", self.config.name.clone()),
                KeyValue::new("tenant", self.config.tenant.clone()),
                KeyValue::new("status", "success"),
            ],
        );

        Ok(handle)
    }

    async fn process_inner_async(
        &self,
        raw_body: &[u8],
        signature_header: Option<&str>,
        engine: &SimulacraEngine,
        task_manager: &TaskManager,
        resolver: &TenantResolver,
    ) -> Result<TaskHandle, WebhookError> {
        use tracing::Instrument;

        let span = tracing::info_span!(
            "simulacra_webhook_received",
            "simulacra.trigger.webhook_name" = self.config.name.as_str(),
            "simulacra.trigger.tenant" = self.config.tenant.as_str(),
        );

        self.process_inner_async_instrumented(
            raw_body,
            signature_header,
            engine,
            task_manager,
            resolver,
        )
        .instrument(span)
        .await
    }

    async fn process_inner_async_instrumented(
        &self,
        raw_body: &[u8],
        signature_header: Option<&str>,
        engine: &SimulacraEngine,
        task_manager: &TaskManager,
        resolver: &TenantResolver,
    ) -> Result<TaskHandle, WebhookError> {
        let (tenant, task_description, metadata) =
            self.validate_and_build(raw_body, signature_header, resolver)?;

        // Engine path — actually spawn an agent. This constructs the VFS,
        // capability token, and tool registry and runs the agent loop on the
        // worker pool.
        let handle = engine
            .spawn_task(
                task_manager,
                &task_description,
                tenant,
                Some(&self.config.agent_type),
                metadata,
                None,
                None,
            )
            .await
            .map_err(|e| WebhookError::TaskCreationFailed(e.to_string()))?;

        info!(
            webhook_name = %self.config.name,
            tenant = %self.config.tenant,
            task_id = %handle.task_id,
            "webhook task spawned via engine"
        );
        ServerMeters::get().webhook_requests.add(
            1,
            &[
                KeyValue::new("webhook_name", self.config.name.clone()),
                KeyValue::new("tenant", self.config.tenant.clone()),
                KeyValue::new("status", "success"),
            ],
        );

        Ok(handle)
    }
}

/// Apply Mustache-style payload template substitution.
///
/// Replaces `{{payload.field}}` and `{{payload.nested.field}}` with values
/// from the JSON payload. Missing fields produce `<missing: payload.field_name>`.
pub fn apply_payload_template(template: &str, payload: &serde_json::Value) -> String {
    // Match `{{payload.path.to.field}}`
    let re = Regex::new(r"\{\{payload\.([^}]+)\}\}").expect("valid regex");
    let mut result = template.to_string();

    // Collect matches first to avoid borrow issues.
    let substitutions: Vec<(String, String)> = re
        .captures_iter(template)
        .map(|cap| {
            let full_match = cap[0].to_string();
            let path = &cap[1];
            let value = resolve_dot_path(payload, path)
                .unwrap_or_else(|| format!("<missing: payload.{path}>"));
            (full_match, value)
        })
        .collect();

    for (pattern, replacement) in substitutions {
        result = result.replace(&pattern, &replacement);
    }
    result
}

/// Resolve a dot-path like "contact.email" in a JSON value.
fn resolve_dot_path(value: &serde_json::Value, path: &str) -> Option<String> {
    let mut current = value;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    match current {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Compute HMAC-SHA256 signature string for a body.
///
/// Returns the full `sha256=<hex>` header value.
/// Used for generating test signatures and for `simulacra-cli` webhook testing.
pub fn compute_hmac_signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    let result = mac.finalize();
    format!("sha256={}", hex::encode(result.into_bytes()))
}
