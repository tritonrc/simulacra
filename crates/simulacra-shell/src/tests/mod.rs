//! Red tests for `specs/S002-shell.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;
use tracing_subscriber::layer::SubscriberExt;

use crate::http_proxy::{ShellHttpError, ShellHttpProxy, ShellHttpResponse};
use crate::{CommandResult, ShellExecutor};

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
    parent: Option<String>,
}

struct SpanCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for SpanCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);

        let parent = attrs
            .parent()
            .and_then(|parent_id| ctx.span(parent_id))
            .map(|span| span.name().to_string())
            .or_else(|| {
                if attrs.is_contextual() {
                    ctx.current_span()
                        .id()
                        .and_then(|parent_id| ctx.span(parent_id))
                        .map(|span| span.name().to_string())
                } else {
                    None
                }
            });

        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
            parent,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span_ref) = ctx.span(id) {
            let span_name = span_ref.name().to_string();
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

fn run_shell(vfs: &dyn VirtualFs, env: HashMap<String, String>, input: &str) -> CommandResult {
    let mut shell = ShellExecutor::new(vfs, env, None);
    shell.run(input)
}

// Use a global subscriber to avoid callsite interest caching issues.
// When tracing spans are first hit without any subscriber, the callsite is
// cached as Interest::never and `with_default` cannot override it. A global
// subscriber ensures callsites are always registered as active.
static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();
static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn capture_store() -> Arc<Mutex<Vec<CapturedSpan>>> {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("capture store should only be initialized once");

        let subscriber =
            tracing_subscriber::registry::Registry::default().with(SpanCaptureLayer { spans });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
    });

    Arc::clone(
        CAPTURED_SPANS
            .get()
            .expect("capture store should be installed"),
    )
}

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn capture_spans<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>) {
    let _guard = test_guard();
    let spans = capture_store();
    spans.lock().unwrap().clear();
    let result = f();
    let captured = spans.lock().unwrap().clone();
    (result, captured)
}

fn field_matches(span: &CapturedSpan, key: &str, expected: &str) -> bool {
    span.fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn shell_command_spans(spans: &[CapturedSpan]) -> Vec<&CapturedSpan> {
    spans
        .iter()
        .filter(|span| field_matches(span, "simulacra.operation.name", "shell_command"))
        .collect()
}

// ---------------------------------------------------------------------------
// Mock HTTP proxy and curl tests
// ---------------------------------------------------------------------------

/// A mock HTTP proxy that returns a preconfigured response or error.
struct MockShellHttpProxy {
    response: Mutex<Option<Result<ShellHttpResponse, MockHttpError>>>,
    /// Captures the last request for assertion.
    last_request: Mutex<Option<CapturedRequest>>,
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
}

/// We cannot store `ShellHttpError` directly because it does not implement Clone.
/// Instead we store a variant tag and re-create the error on demand.
#[derive(Debug)]
enum MockHttpError {
    CapabilityDenied(String),
    BudgetExhausted(String),
    NetworkError(String),
    Timeout,
}

impl MockShellHttpProxy {
    fn with_response(status: u16, status_text: &str, body: &str) -> Self {
        Self {
            response: Mutex::new(Some(Ok(ShellHttpResponse {
                status,
                status_text: status_text.to_string(),
                headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                body: body.as_bytes().to_vec(),
                url: String::new(),
            }))),
            last_request: Mutex::new(None),
        }
    }

    fn with_response_headers(
        status: u16,
        status_text: &str,
        headers: Vec<(String, String)>,
        body: &str,
    ) -> Self {
        Self {
            response: Mutex::new(Some(Ok(ShellHttpResponse {
                status,
                status_text: status_text.to_string(),
                headers,
                body: body.as_bytes().to_vec(),
                url: String::new(),
            }))),
            last_request: Mutex::new(None),
        }
    }

    fn with_error(err: MockHttpError) -> Self {
        Self {
            response: Mutex::new(Some(Err(err))),
            last_request: Mutex::new(None),
        }
    }

    fn last_request(&self) -> CapturedRequest {
        self.last_request
            .lock()
            .unwrap()
            .clone()
            .expect("no request was captured")
    }
}

impl ShellHttpProxy for MockShellHttpProxy {
    fn execute(
        &self,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<ShellHttpResponse, ShellHttpError> {
        *self.last_request.lock().unwrap() = Some(CapturedRequest {
            url: url.to_string(),
            method: method.to_string(),
            headers: headers.to_vec(),
            body: body.map(|b| b.to_vec()),
            timeout_ms,
        });

        match self.response.lock().unwrap().take() {
            Some(Ok(mut resp)) => {
                resp.url = url.to_string();
                Ok(resp)
            }
            Some(Err(e)) => match e {
                MockHttpError::CapabilityDenied(msg) => Err(ShellHttpError::CapabilityDenied(msg)),
                MockHttpError::BudgetExhausted(msg) => Err(ShellHttpError::BudgetExhausted(msg)),
                MockHttpError::NetworkError(msg) => Err(ShellHttpError::NetworkError(msg)),
                MockHttpError::Timeout => Err(ShellHttpError::Timeout),
            },
            None => panic!("MockShellHttpProxy: response already consumed"),
        }
    }
}

fn run_shell_with_http(
    vfs: &dyn VirtualFs,
    env: HashMap<String, String>,
    http_proxy: &dyn ShellHttpProxy,
    input: &str,
) -> CommandResult {
    let mut shell = ShellExecutor::new(vfs, env, Some(http_proxy));
    shell.run(input)
}

mod builtin_commands;
mod connectors;
mod core;
mod curl_basic;
mod curl_errors;
mod http_isolation;
mod parser;
mod shell_state;
mod wget;
