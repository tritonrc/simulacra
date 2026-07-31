//! Classification of `reqwest` failures into `ProviderError::Transport`.
//!
//! Only transient failures raised while sending a request belong here:
//! connect, request, and timeout failures before response headers arrive.
//! Local request construction and redirect-policy failures are deterministic
//! configuration errors and remain non-retryable.

use simulacra_types::ProviderError;

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
