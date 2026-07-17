use crate::error::McpError;
use crate::transport::sse::parse_sse_endpoint;
use crate::transport::state::TransportMode;

use super::McpManager;

impl McpManager {
    /// Dispatch the handshake for a server based on its configured transport.
    ///
    /// `configured_transport == Some("sse")` → SSE handshake (no auto-detect).
    /// `configured_transport == Some("http")` → streamable HTTP only (no fallback).
    /// `configured_transport == None` → auto-detect: try streamable HTTP first,
    ///   fall back to legacy SSE on 404/405.
    pub(crate) async fn handshake_server(&mut self, key: &str) {
        let conn = match self.connections.get(key) {
            Some(c) => c,
            None => return,
        };

        let configured = conn.configured_transport.as_deref();

        // WARNING 4: Only accept whitelisted transport strings. Unknown
        // values should never reach here because `normalize_transport`
        // rejects them at registration, but refuse to guess here just in
        // case a caller constructed an McpConnection by other means.
        match configured {
            None | Some("") => {
                self.auto_detect_handshake(key).await;
            }
            Some("sse") => {
                // Forced legacy SSE — skip auto-detect.
                self.perform_sse_handshake(key).await;
            }
            Some("http") => {
                self.forced_http_handshake(key).await;
            }
            Some(other) => {
                tracing::warn!(
                    server = %key,
                    transport = %other,
                    "MCP handshake aborted: unknown configured transport (expected \"sse\" or \"http\")"
                );
            }
        }
    }

    /// Forced streamable HTTP handshake — no fallback.
    async fn forced_http_handshake(&mut self, key: &str) {
        let (url, headers) = match self.connections.get(key) {
            Some(c) => (c.url.clone(), c.headers.clone()),
            None => return,
        };

        let span = tracing::info_span!(
            "simulacra_mcp_handshake",
            simulacra.mcp.transport_mode = "streamable_http",
            simulacra.mcp.protocol_version = "2025-03-26",
            simulacra.mcp.session_id = tracing::field::Empty,
        );

        match self.perform_streamable_http_handshake(&url, &headers).await {
            Ok((tools, session_id)) => {
                if let Some(ref sid) = session_id {
                    span.record("simulacra.mcp.session_id", sid.as_str());
                }
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = tools;
                    conn.handshake_done = true;
                    conn.was_connected = true;
                    conn.transport_mode = Some(TransportMode::StreamableHttp { session_id });
                }
            }
            Err(_error) => {
                let server_name = self
                    .connections
                    .get(key)
                    .map(|c| c.server_name.clone())
                    .unwrap_or_else(|| key.to_string());
                tracing::warn!(
                    server = %server_name,
                    error = "transport failure (details redacted)",
                    "MCP streamable HTTP handshake failure"
                );
                // Forced "http" failure is final — do NOT set handshake_done
                // so the connection remains unusable but retryable.
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = Vec::new();
                    // handshake_done stays false, was_connected stays false.
                }
            }
        }
    }

    /// Auto-detect: try streamable HTTP first, fall back to legacy SSE on 404/405.
    async fn auto_detect_handshake(&mut self, key: &str) {
        let (url, headers) = match self.connections.get(key) {
            Some(c) => (c.url.clone(), c.headers.clone()),
            None => return,
        };

        let span = tracing::info_span!(
            "simulacra_mcp_handshake",
            simulacra.mcp.transport_mode = tracing::field::Empty,
            simulacra.mcp.protocol_version = tracing::field::Empty,
            simulacra.mcp.session_id = tracing::field::Empty,
        );

        match self.perform_streamable_http_handshake(&url, &headers).await {
            Ok((tools, session_id)) => {
                span.record("simulacra.mcp.transport_mode", "streamable_http");
                span.record("simulacra.mcp.protocol_version", "2025-03-26");
                if let Some(ref sid) = session_id {
                    span.record("simulacra.mcp.session_id", sid.as_str());
                }
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = tools;
                    conn.handshake_done = true;
                    conn.was_connected = true;
                    conn.transport_mode = Some(TransportMode::StreamableHttp { session_id });
                }
            }
            Err(McpError::TransportError(ref msg))
                if msg.contains("404") || msg.contains("405") =>
            {
                let server_name = self
                    .connections
                    .get(key)
                    .map(|c| c.server_name.clone())
                    .unwrap_or_else(|| key.to_string());
                tracing::info!(
                    server = %server_name,
                    streamable_http_error = "404/405",
                    "Auto-detect: streamable HTTP returned 404/405, falling back to legacy SSE"
                );
                // Fall back to legacy SSE.
                self.perform_sse_handshake(key).await;
                // Record transport mode on span after SSE handshake.
                span.record("simulacra.mcp.transport_mode", "legacy_sse");
                span.record("simulacra.mcp.protocol_version", "2024-11-05");
            }
            Err(_error) => {
                // Non-fallback error (auth, 5xx, network) — do not try SSE.
                let server_name = self
                    .connections
                    .get(key)
                    .map(|c| c.server_name.clone())
                    .unwrap_or_else(|| key.to_string());
                tracing::warn!(
                    server = %server_name,
                    error = "transport failure (details redacted)",
                    "MCP connection failure (not eligible for SSE fallback)"
                );
                // Do NOT set handshake_done — connection stays retryable.
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = Vec::new();
                    // handshake_done stays false, was_connected stays false.
                }
            }
        }
    }

    /// Connect to an SSE endpoint, discover the POST endpoint from SSE events,
    /// perform the MCP handshake via the discovered endpoint, and keep the
    /// SSE connection alive in a background task.
    ///
    /// Only marks the connection as successful (`handshake_done = true`,
    /// `was_connected = true`) AFTER the handshake actually succeeds. If
    /// endpoint discovery or the HTTP handshake fails, the connection is
    /// left in a non-handshaked state so callers can retry or surface the
    /// failure.
    async fn perform_sse_handshake(&mut self, key: &str) {
        let conn = match self.connections.get(key) {
            Some(c) => c,
            None => return,
        };
        let sse_url = conn.url.clone();
        let parsed = match url::Url::parse(&sse_url) {
            Ok(p) => p,
            Err(_error) => {
                tracing::warn!(
                    server = %key,
                    transport = "sse",
                    stage = "validate_url",
                    error = "invalid URL (details redacted)",
                    "MCP SSE handshake failure: invalid URL"
                );
                return;
            }
        };

        // WARNING 3: Legacy SSE uses raw TcpStream for keepalive, which
        // cannot carry HTTPS traffic. Reject https:// URLs on legacy SSE
        // rather than silently sending plaintext HTTP through a TLS port.
        if parsed.scheme() == "https" {
            tracing::warn!(
                server = %key,
                transport = "sse",
                stage = "validate_transport",
                error = "unsupported HTTPS scheme (details redacted)",
                "MCP SSE handshake failure: legacy SSE transport does not support HTTPS — \
                 use streamable HTTP transport (transport = \"http\") instead"
            );
            return;
        }

        let host = parsed.host_str().unwrap_or("127.0.0.1").to_string();
        let port = parsed.port().unwrap_or(80);
        let path = parsed.path().to_string();
        let addr = format!("{host}:{port}");
        let base_url = format!("{}://{}:{}", parsed.scheme(), host, port);

        // Discover the POST endpoint via reqwest SSE request.
        let post_endpoint = {
            let client = reqwest::Client::builder().build().ok();

            let mut discovered_endpoint: Option<String> = None;

            if let Some(client) = client {
                let response = client
                    .get(&sse_url)
                    .header("Accept", "text/event-stream")
                    .send()
                    .await;

                if let Ok(mut response) = response {
                    let mut accumulated = String::new();
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                    while discovered_endpoint.is_none() {
                        let remaining =
                            deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match tokio::time::timeout(remaining, response.chunk()).await {
                            Ok(Ok(Some(chunk))) => {
                                accumulated.push_str(&String::from_utf8_lossy(&chunk));
                                discovered_endpoint = parse_sse_endpoint(&accumulated);
                            }
                            _ => break,
                        }
                    }
                }
            }

            discovered_endpoint.map(|ep| {
                if ep.starts_with("http://") || ep.starts_with("https://") {
                    ep
                } else {
                    format!("{base_url}{ep}")
                }
            })
        };

        // If endpoint discovery failed, abort. Do NOT mark the connection
        // as handshake_done — leave it in a retryable state.
        let endpoint = match post_endpoint {
            Some(ep) => ep,
            None => {
                tracing::warn!(
                    server = %key,
                    transport = "sse",
                    stage = "discover_endpoint",
                    error = "endpoint discovery failed (details redacted)",
                    "MCP SSE handshake failure: endpoint discovery produced no result"
                );
                return;
            }
        };

        // Perform the MCP handshake via the discovered POST endpoint BEFORE
        // opening any keepalive stream. If the handshake fails, there is
        // nothing worth keeping alive.
        let tools = match self.perform_http_handshake(&endpoint).await {
            Ok(t) => t,
            Err(_error) => {
                tracing::warn!(
                    server = %key,
                    transport = "sse",
                    stage = "initialize",
                    error = "transport failure (details redacted)",
                    "MCP SSE handshake failure"
                );
                return;
            }
        };

        // Open a raw TCP connection for the background SSE keepalive task.
        // Only reached after handshake success.
        let stream = {
            use tokio::io::AsyncWriteExt;

            match tokio::net::TcpStream::connect(&addr).await {
                Ok(mut s) => {
                    let request = format!(
                        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\n\r\n"
                    );
                    if s.write_all(request.as_bytes()).await.is_err() {
                        tracing::warn!(
                            server = %key,
                            transport = "sse",
                            stage = "open_keepalive",
                            error = "transport failure (details redacted)",
                            "MCP SSE handshake failure: could not write keepalive request"
                        );
                        return;
                    }
                    s
                }
                Err(_error) => {
                    tracing::warn!(
                        server = %key,
                        transport = "sse",
                        stage = "open_keepalive",
                        error = "transport failure (details redacted)",
                        "MCP SSE handshake failure: could not open keepalive TCP stream"
                    );
                    return;
                }
            }
        };

        // Keep the SSE connection alive in a background task.
        let sse_handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_n) => {
                        // Event received; keep reading to maintain persistence.
                    }
                    Err(_) => break,
                }
            }
        });

        if let Some(conn) = self.connections.get_mut(key) {
            conn.transport_mode = Some(TransportMode::LegacySse {
                post_endpoint: endpoint,
                sse_handle,
            });
            conn.tools = tools;
            conn.handshake_done = true;
            conn.was_connected = true;
        }
    }
}
