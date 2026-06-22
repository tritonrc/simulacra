//! Core HTTP types for the simulacra-http crate.

/// An HTTP request to be executed by an [`HttpClient`](crate::HttpClient).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// The URL to request.
    pub url: String,
    /// The HTTP method (GET, POST, PUT, PATCH, DELETE, HEAD).
    pub method: String,
    /// Request headers as name-value pairs.
    pub headers: Vec<(String, String)>,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// Per-request timeout in milliseconds. Overrides the client default when set.
    pub timeout_ms: Option<u64>,
    /// Maximum number of redirects to follow. Overrides the client default when set.
    pub max_redirects: Option<u32>,
}

/// An HTTP response returned by an [`HttpClient`](crate::HttpClient).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code (e.g. 200, 404).
    pub status: u16,
    /// HTTP status text (e.g. "OK", "Not Found").
    pub status_text: String,
    /// Response headers as name-value pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
    /// The final URL after any redirects.
    pub url: String,
    /// Whether the request was redirected (final URL differs from original).
    pub redirected: bool,
}

/// Errors that can occur during HTTP request execution.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// A network-level error (DNS failure, connection refused, etc.).
    #[error("network error: {0}")]
    Network(String),

    /// The request timed out.
    #[error("request timed out")]
    Timeout,

    /// Too many redirects were followed.
    #[error("too many redirects")]
    TooManyRedirects,

    /// The provided URL is invalid.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// Response body exceeded the maximum allowed size.
    #[error("response too large: {0} bytes")]
    ResponseTooLarge(u64),
}
