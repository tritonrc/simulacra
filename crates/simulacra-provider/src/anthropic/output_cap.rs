//! Per-request output token cap for Anthropic requests.

use simulacra_types::ResourceBudget;

pub(super) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

/// `max_tokens` for one request: the provider's cap, lowered to what remains
/// of a non-zero run budget. A run budget of 0 is unlimited.
pub(super) fn request_max_tokens(budget: &ResourceBudget, cap: u32) -> u32 {
    if budget.max_tokens == 0 {
        return cap;
    }
    let remaining = budget.max_tokens.saturating_sub(budget.used_tokens);
    (remaining.min(u64::from(cap)) as u32).max(1)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use rust_decimal::Decimal;
    use simulacra_types::{
        Message, Provider, ProviderError, ProviderStreamEvent, ProviderStreamSink, ResourceBudget,
        Role, StreamingProvider,
    };

    use super::{DEFAULT_MAX_OUTPUT_TOKENS, request_max_tokens};
    use crate::anthropic::AnthropicProvider;
    use crate::anthropic::client::{HttpClient, HttpResponse};

    fn budget(max_tokens: u64, used_tokens: u64) -> ResourceBudget {
        let mut budget = ResourceBudget::new(max_tokens, 10, Decimal::ZERO, 0);
        budget.used_tokens = used_tokens;
        budget
    }

    #[test]
    fn unlimited_run_budget_sends_the_cap() {
        assert_eq!(request_max_tokens(&budget(0, 5_000_000), 16_000), 16_000);
        assert_eq!(
            request_max_tokens(&budget(0, 0), DEFAULT_MAX_OUTPUT_TOKENS),
            8192
        );
    }

    #[test]
    fn run_budget_lowers_the_cap_to_what_remains() {
        assert_eq!(request_max_tokens(&budget(100_000, 0), 16_000), 16_000);
        assert_eq!(request_max_tokens(&budget(100_000, 95_000), 16_000), 5_000);
        assert_eq!(request_max_tokens(&budget(100, 100), 16_000), 1);
    }

    struct Capturing(Arc<Mutex<Vec<serde_json::Value>>>);

    impl HttpClient for Capturing {
        fn post(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            body: &[u8],
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>
        {
            self.0
                .lock()
                .unwrap()
                .push(serde_json::from_slice(body).unwrap());
            Box::pin(async {
                Ok(HttpResponse {
                    status: 400,
                    headers: HashMap::new(),
                    body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"x"}}"#
                        .to_vec(),
                })
            })
        }
    }

    struct NoSink;
    impl ProviderStreamSink for NoSink {
        fn emit(&self, _event: ProviderStreamEvent) {}
    }

    fn capped_provider(
        cap: Option<u32>,
    ) -> (AnthropicProvider, Arc<Mutex<Vec<serde_json::Value>>>) {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let provider = AnthropicProvider::with_http_client(
            "key",
            "claude-test",
            Box::new(Capturing(Arc::clone(&bodies))),
        );
        let provider = match cap {
            Some(cap) => provider.with_max_output_tokens(cap),
            None => provider,
        };
        (provider, bodies)
    }

    fn user() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: "hi".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }]
    }

    #[tokio::test]
    async fn configured_cap_reaches_the_non_streaming_request() {
        let (provider, bodies) = capped_provider(Some(16_000));
        let _ = provider.chat(&user(), &[], &mut budget(0, 0)).await;
        assert_eq!(bodies.lock().unwrap()[0]["max_tokens"], 16_000);
    }

    #[tokio::test]
    async fn configured_cap_reaches_the_streaming_request() {
        let (provider, bodies) = capped_provider(Some(16_000));
        let _ = provider
            .chat_stream(&user(), &[], &mut budget(0, 0), &NoSink)
            .await;
        assert_eq!(bodies.lock().unwrap()[0]["max_tokens"], 16_000);
    }

    #[tokio::test]
    async fn unconfigured_provider_keeps_the_default_cap() {
        let (provider, bodies) = capped_provider(None);
        let _ = provider.chat(&user(), &[], &mut budget(0, 0)).await;
        assert_eq!(bodies.lock().unwrap()[0]["max_tokens"], 8192);
    }
}
