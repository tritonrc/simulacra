//! Failures after response headers: the idle timeout that bounds a read,
//! Anthropic's in-stream `error` event, and a body read that gives out.

use std::time::Duration;

use simulacra_types::ProviderError;

pub(super) use crate::transport::READ_IDLE_TIMEOUT;

pub(super) fn idle_timeout_client(read_timeout: Duration) -> reqwest::Client {
    crate::transport::provider_http_client(read_timeout)
}

/// An `error` event ends the stream without a `message_stop`. Accepting it as
/// the end of the reply would pass a truncated or empty reply as a success.
pub(super) fn from_event(event: &serde_json::Value) -> ProviderError {
    let error = event.get("error");
    let field = |name: &str| {
        error
            .and_then(|e| e.get(name))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let (kind, message) = (field("type"), field("message"));
    match kind.as_str() {
        "overloaded_error" => ProviderError::Overloaded(message),
        "rate_limit_error" => ProviderError::RateLimit {
            retry_after_ms: None,
        },
        "invalid_request_error" | "request_too_large" => ProviderError::BadRequest(message),
        "authentication_error" | "permission_error" => ProviderError::AuthError(message),
        _ => ProviderError::ServerError(format!("stream error {kind}: {message}")),
    }
}

/// A response that stops sending past the idle timeout is a transient
/// failure worth a retry. `part` names what was being read.
pub(super) fn read_error(part: &'static str) -> impl Fn(reqwest::Error) -> ProviderError {
    move |err| {
        if err.is_timeout() {
            return ProviderError::Transport(format!(
                "the provider stopped sending the {part}; retry."
            ));
        }
        ProviderError::Other(format!("failed to read {part}: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::pin::Pin;
    use std::sync::mpsc;
    use std::time::Duration;

    use rust_decimal::Decimal;
    use simulacra_types::{
        Message, ProviderError, ProviderStreamEvent, ProviderStreamSink, ResourceBudget, Role,
        StreamingProvider,
    };

    use crate::anthropic::AnthropicProvider;
    use crate::anthropic::client::{HttpClient, HttpResponse, HttpStreamSink, ReqwestClient};

    struct Sse(&'static str);

    impl HttpClient for Sse {
        fn post(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            _body: &[u8],
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>
        {
            let body = self.0.as_bytes().to_vec();
            Box::pin(async move {
                Ok(HttpResponse {
                    status: 200,
                    headers: HashMap::from([(
                        "content-type".to_owned(),
                        "text/event-stream".to_owned(),
                    )]),
                    body,
                })
            })
        }
    }

    struct NoSink;
    impl ProviderStreamSink for NoSink {
        fn emit(&self, _event: ProviderStreamEvent) {}
    }

    async fn stream(sse: &'static str) -> Result<simulacra_types::ProviderResponse, ProviderError> {
        let provider =
            AnthropicProvider::with_http_client("key", "claude-test", Box::new(Sse(sse)));
        let messages = vec![Message {
            role: Role::User,
            content: "hi".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }];
        let mut budget = ResourceBudget::new(0, 10, Decimal::ZERO, 0);
        provider
            .chat_stream(&messages, &[], &mut budget, &NoSink)
            .await
    }

    const STARTED: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
    );

    #[tokio::test]
    async fn an_overloaded_error_mid_stream_is_a_retryable_overload() {
        const SSE: &str = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10}}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        match stream(SSE).await {
            Err(ProviderError::Overloaded(message)) => assert_eq!(message, "Overloaded"),
            other => panic!("expected an overload error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_api_error_after_partial_text_does_not_pass_the_partial_text_as_a_reply() {
        const SSE: &str = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Internal\"}}\n\n",
        );
        let body: &'static str = Box::leak(format!("{STARTED}{SSE}").into_boxed_str());
        let error = stream(body)
            .await
            .expect_err("an in-stream error is an error");
        assert!(
            matches!(&error, ProviderError::ServerError(m) if m.contains("api_error") && m.contains("Internal")),
            "{error:?}"
        );
        assert!(error.is_retryable());
    }

    #[test]
    fn error_event_types_map_to_their_provider_errors() {
        let event = |kind: &str| serde_json::json!({"type": "error", "error": {"type": kind, "message": "m"}});
        assert!(matches!(
            super::from_event(&event("rate_limit_error")),
            ProviderError::RateLimit { .. }
        ));
        assert!(matches!(
            super::from_event(&event("invalid_request_error")),
            ProviderError::BadRequest(_)
        ));
        assert!(matches!(
            super::from_event(&event("authentication_error")),
            ProviderError::AuthError(_)
        ));
        assert!(matches!(
            super::from_event(&serde_json::json!({"type": "error"})),
            ProviderError::ServerError(_)
        ));
    }

    /// Serves headers and the start of a body, then stops sending until the
    /// test is done with it.
    fn stalling_server(
        head: &'static [u8],
    ) -> (String, mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let (release, released) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0_u8; 4096]);
            let _ = socket.write_all(head);
            let _ = released.recv();
        });
        (url, release, server)
    }

    fn assert_retryable_transport<T>(result: Result<T, ProviderError>) {
        match result {
            Err(error @ ProviderError::Transport(_)) => assert!(error.is_retryable()),
            Err(other) => panic!("expected a transport failure, got {other:?}"),
            Ok(_) => panic!("a stalled response must not complete"),
        }
    }

    #[tokio::test]
    async fn a_stream_that_stops_sending_after_headers_is_a_retryable_transport_failure() {
        let (url, release, server) = stalling_server(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
              transfer-encoding: chunked\r\n\r\n6\r\ndata: \r\n",
        );
        struct Sink;
        impl HttpStreamSink for Sink {}
        let client = ReqwestClient::with_read_timeout(Duration::from_millis(300));
        let result = client.post_stream(&url, &[], b"{}", &mut Sink).await;
        let _ = release.send(());
        server.join().unwrap();
        assert_retryable_transport(result);
    }

    #[tokio::test]
    async fn a_response_body_that_stops_sending_is_a_retryable_transport_failure() {
        let (url, release, server) = stalling_server(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
              content-length: 100\r\n\r\n{\"id\":",
        );
        let client = ReqwestClient::with_read_timeout(Duration::from_millis(300));
        let result = client.post(&url, &[], b"{}").await;
        let _ = release.send(());
        server.join().unwrap();
        assert_retryable_transport(result);
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Tool arguments carry file contents, so the parse-failure warning
    /// names their size, never the text.
    #[tokio::test]
    async fn malformed_tool_arguments_are_logged_by_length_not_content() {
        const SSE: &str = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"workspace_write\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"content\\\":\\\"SECRET_FILE_BODY\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        );
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let _ = stream(SSE).await;

        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("failed to parse"),
            "the warning fires: {text}"
        );
        assert!(text.contains("raw_args_len=28"), "{text}");
        assert!(!text.contains("SECRET_FILE_BODY"), "{text}");
    }
}
