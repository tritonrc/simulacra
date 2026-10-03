//! Per-instance configuration for the OpenAI-compatible provider.
//!
//! `OpenAiConfig` lets a caller point the provider at any OpenAI-compatible
//! gateway (OpenRouter, LiteLLM, vLLM, Azure-style deployments) without
//! touching the process environment. `OpenAiProvider::new` still reads
//! `OPENAI_BASE_URL`/`OPENAI_API_BASE` for back-compat; `with_config` does not.

/// How the API key is attached to outgoing requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AuthStyle {
    /// `authorization: Bearer <key>` — OpenAI, OpenRouter, most gateways.
    #[default]
    Bearer,
    /// The raw key under a caller-named header, e.g. `api-key` for Azure.
    Header(String),
}

/// Which request field caps generation length.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputCapField {
    /// Current chat-completions field name (OpenAI, most gateways).
    #[default]
    MaxCompletionTokens,
    /// Older field name some OpenAI-compatible servers still expect.
    MaxTokens,
}

impl OutputCapField {
    pub(super) fn wire_name(self) -> &'static str {
        match self {
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxTokens => "max_tokens",
        }
    }
}

/// Per-instance OpenAI-compatible provider configuration.
#[derive(Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub auth_style: AuthStyle,
    pub extra_headers: Vec<(String, String)>,
    pub output_cap_field: OutputCapField,
}

impl OpenAiConfig {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into().trim().to_owned(),
            model: model.into(),
            auth_style: AuthStyle::default(),
            extra_headers: Vec::new(),
            output_cap_field: OutputCapField::default(),
        }
    }

    pub fn with_auth_style(mut self, auth_style: AuthStyle) -> Self {
        self.auth_style = auth_style;
        self
    }

    pub fn with_extra_headers(mut self, extra_headers: Vec<(String, String)>) -> Self {
        self.extra_headers = extra_headers;
        self
    }

    pub fn with_output_cap_field(mut self, output_cap_field: OutputCapField) -> Self {
        self.output_cap_field = output_cap_field;
        self
    }

    /// Headers that carry the credential plus any caller-supplied static
    /// headers. Never logged — callers must not `{:?}` the result.
    pub(super) fn auth_headers(&self) -> Vec<(String, String)> {
        let mut headers = match &self.auth_style {
            AuthStyle::Bearer => vec![(
                "authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            )],
            AuthStyle::Header(name) => vec![(name.clone(), self.api_key.clone())],
        };
        headers.extend(self.extra_headers.iter().cloned());
        headers
    }
}

const MAX_UPSTREAM_TEXT_CHARS: usize = 512;

impl OpenAiConfig {
    /// An upstream error body can echo what it was sent (a gateway's "invalid
    /// key: ..." message), so every configured secret is masked and the text
    /// is bounded before it travels on in a `ProviderError`.
    pub(super) fn scrub_upstream_text(&self, text: &str) -> String {
        let mut scrubbed = text.to_owned();
        let secrets = std::iter::once(self.api_key.as_str())
            .chain(self.extra_headers.iter().map(|(_, value)| value.as_str()))
            .filter(|secret| !secret.is_empty());
        // Longest first, so a secret containing another is masked whole.
        let mut secrets: Vec<&str> = secrets.collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        for secret in secrets {
            scrubbed = scrubbed.replace(secret, "<redacted>");
        }
        match scrubbed.char_indices().nth(MAX_UPSTREAM_TEXT_CHARS) {
            Some((cut, _)) => format!("{}...", &scrubbed[..cut]),
            None => scrubbed,
        }
    }
}

/// The base URL without userinfo or query, which can carry credentials.
pub(super) fn redacted_base_url(base_url: &str) -> String {
    match url::Url::parse(base_url) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.to_string()
        }
        Err(_) => "<unparseable base URL>".to_owned(),
    }
}

/// Redacts the credential and extra header values; only shapes/counts are
/// shown so a stray `{:?}` of the config can't leak a secret into logs.
impl std::fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &redacted_base_url(&self.base_url))
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("auth_style", &self.auth_style)
            .field(
                "extra_headers",
                &format!("<{} headers>", self.extra_headers.len()),
            )
            .field("output_cap_field", &self.output_cap_field)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_auth_style_is_bearer_with_no_extra_headers() {
        let config = OpenAiConfig::new("https://api.openai.com", "sk-test", "gpt-4o-mini");
        assert_eq!(
            config.auth_headers(),
            vec![("authorization".to_owned(), "Bearer sk-test".to_owned())]
        );
    }

    #[test]
    fn named_header_auth_style_sends_the_raw_key_under_that_header() {
        let config = OpenAiConfig::new("https://example.azure.com", "secret", "gpt-4o")
            .with_auth_style(AuthStyle::Header("api-key".to_owned()));
        assert_eq!(
            config.auth_headers(),
            vec![("api-key".to_owned(), "secret".to_owned())]
        );
    }

    #[test]
    fn extra_headers_are_appended_after_the_auth_header() {
        let config = OpenAiConfig::new("https://openrouter.ai/api/v1", "key", "model")
            .with_extra_headers(vec![("x-app".to_owned(), "simulacra".to_owned())]);
        assert_eq!(
            config.auth_headers(),
            vec![
                ("authorization".to_owned(), "Bearer key".to_owned()),
                ("x-app".to_owned(), "simulacra".to_owned()),
            ]
        );
    }

    #[test]
    fn debug_never_prints_the_api_key_or_header_values() {
        let config = OpenAiConfig::new("https://api.openai.com", "sk-super-secret", "gpt-4o-mini")
            .with_extra_headers(vec![("x-app".to_owned(), "do-not-print".to_owned())]);
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-super-secret"));
        assert!(!debug.contains("do-not-print"));
    }

    #[test]
    fn output_cap_field_wire_names_match_the_api() {
        assert_eq!(
            OutputCapField::MaxCompletionTokens.wire_name(),
            "max_completion_tokens"
        );
        assert_eq!(OutputCapField::MaxTokens.wire_name(), "max_tokens");
        assert_eq!(
            OutputCapField::default(),
            OutputCapField::MaxCompletionTokens
        );
    }
}
