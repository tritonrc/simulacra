//! Classification of `reqwest` failures into `ProviderError::Transport`.
//!
//! Only transient failures raised while sending a request belong here:
//! connect, request, and timeout failures before response headers arrive.
//! Local request construction and redirect-policy failures are deterministic
//! configuration errors and remain non-retryable.

use std::time::Duration;

use simulacra_types::ProviderError;

/// No total timeout, so a long stream can run to the end; a stream that goes
/// this long without a byte is dead.
pub(crate) const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// The client every provider sends through. Redirects are refused: reqwest
/// strips only `Authorization` on a cross-host redirect, so a key carried in
/// any other header (`x-api-key`, `api-key`) would follow it to the new host.
/// A 3xx comes back as an ordinary error response instead.
pub(crate) fn provider_http_client(read_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .read_timeout(read_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("the HTTP client builds, as reqwest::Client::new() assumes")
}

/// Stage of the HTTP exchange that failed. Used to tell the caller how far the
/// request got before the connection gave out.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TransportStage {
    /// Sending the request, connecting, or waiting for response headers.
    SendRequest,
}

impl TransportStage {
    fn description(self) -> &'static str {
        match self {
            Self::SendRequest => "sending the HTTP request to the provider",
        }
    }
}

/// Classify a `reqwest` send failure without rendering its URL or source
/// chain, either of which may contain configured endpoint secrets.
pub(crate) fn transport_error(stage: TransportStage, err: &reqwest::Error) -> ProviderError {
    let detail = if err.is_builder() {
        return ProviderError::Other(format!(
            "{} could not be constructed; verify the provider endpoint and credential \
             configuration.",
            stage.description()
        ));
    } else if err.is_redirect() {
        return ProviderError::Other(format!(
            "{} was rejected by the HTTP redirect policy; verify the provider endpoint \
             configuration.",
            stage.description()
        ));
    } else if err.is_timeout() {
        format!(
            "{} timed out before response headers arrived; check network connectivity and retry.",
            stage.description()
        )
    } else if err.is_connect() {
        format!(
            "{} could not connect before response headers arrived; check network connectivity \
             and retry.",
            stage.description()
        )
    } else if err.is_request() {
        format!(
            "{} failed before response headers arrived; the provider may or may not have \
             processed the request, so check network connectivity and retry.",
            stage.description()
        )
    } else {
        return ProviderError::Other(format!(
            "{} failed for a non-transient HTTP reason; verify the provider endpoint \
             configuration.",
            stage.description()
        ));
    };

    tracing::warn!(
        error_type = "transport",
        stage = "send_request",
        message = detail.as_str(),
        "provider error: retryable transport failure"
    );
    ProviderError::Transport(detail)
}
