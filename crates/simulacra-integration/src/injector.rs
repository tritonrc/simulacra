//! CredentialInjector — match URL to integration, inject auth headers.

use async_trait::async_trait;
use opentelemetry::KeyValue;

use crate::registry::IntegrationRegistry;
use crate::types::IntegrationError;

/// Injects credentials into outbound HTTP requests based on URL matching.
#[async_trait]
pub trait CredentialInjector: Send + Sync + 'static {
    /// Given a URL and the tenant's granted integrations, return auth headers to inject.
    /// Returns `None` if no integration matches the URL.
    async fn inject_credentials(
        &self,
        url: &str,
        tenant_integrations: &[String],
    ) -> Result<Option<Vec<(String, String)>>, IntegrationError>;
}

#[async_trait]
impl CredentialInjector for IntegrationRegistry {
    async fn inject_credentials(
        &self,
        url: &str,
        tenant_integrations: &[String],
    ) -> Result<Option<Vec<(String, String)>>, IntegrationError> {
        // Find the integration whose base_url is a prefix of the requested URL
        // and that the tenant has been granted access to.
        // URL matching is safe: after matching the prefix, we verify the next
        // character is '/', '?', '#', or end-of-string to prevent injection
        // into attacker-controlled domains (e.g., base_url "https://api.example.com"
        // must not match "https://api.example.com.evil.com").
        for (name, cred) in self.all_integrations() {
            if !url_matches_base(&cred.config.base_url, url) {
                continue;
            }
            // Check tenant scope
            if !tenant_integrations.contains(name) {
                continue;
            }

            tracing::debug!(
                integration = %name,
                url_host = url.split('/').nth(2).unwrap_or("unknown"),
                "credential injection"
            );

            // Get current token
            let token = self.access_token(name).await?;

            let headers = match &cred.config.auth {
                crate::types::AuthMethod::ApiKey { placement, .. } => {
                    parse_api_key_headers(placement, &token)
                }
                crate::types::AuthMethod::OAuth2 { .. } => {
                    vec![("Authorization".to_string(), format!("Bearer {token}"))]
                }
            };

            self.meters()
                .credential_injections
                .add(1, &[KeyValue::new("integration", name.clone())]);

            return Ok(Some(headers));
        }

        // No matching integration found
        Ok(None)
    }
}

/// Safe URL prefix matching for credential injection.
///
/// Returns true only if `url` starts with `base_url` AND the character
/// immediately after the base_url prefix is one of: '/', '?', '#', or
/// end-of-string. This prevents matching attacker-controlled domains
/// like `https://api.example.com.evil.com` against a base_url of
/// `https://api.example.com`.
pub(crate) fn url_matches_base(base_url: &str, url: &str) -> bool {
    if !url.starts_with(base_url) {
        return false;
    }
    // If url is exactly the base_url, it's a match.
    if url.len() == base_url.len() {
        return true;
    }
    // The base_url may end with '/'. If so, starts_with is sufficient.
    if base_url.ends_with('/') {
        return true;
    }
    // Otherwise, the next char after base_url must be a path/query/fragment boundary.
    matches!(
        url.as_bytes().get(base_url.len()),
        Some(b'/') | Some(b'?') | Some(b'#')
    )
}

/// Parse API key placement string and return appropriate headers.
/// "header" -> Authorization: Bearer <key>
/// "header:X-Api-Key" -> X-Api-Key: <key>
pub(crate) fn parse_api_key_headers(placement: &str, key: &str) -> Vec<(String, String)> {
    if let Some(header_name) = placement.strip_prefix("header:") {
        vec![(header_name.to_string(), key.to_string())]
    } else {
        // Default: Authorization: Bearer <key>
        vec![("Authorization".to_string(), format!("Bearer {key}"))]
    }
}
