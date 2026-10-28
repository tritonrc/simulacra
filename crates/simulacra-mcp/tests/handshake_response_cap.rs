//! The response cap that guards tool calls also guards the handshake reads.

use serde_json::json;
use simulacra_mcp::McpManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CAP: usize = 2048;

#[derive(Clone, Copy)]
enum Wire {
    Json,
    Sse,
}

/// Headers ended and the whole `Content-Length` body has arrived.
fn request_complete(request: &[u8]) -> bool {
    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    request.len() >= end + 4 + length
}

/// Serves a valid handshake, padding the `oversize` method's reply past the cap.
async fn serve(wire: Wire, oversize: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request_complete(&request) {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let method = ["initialize", "notifications/initialized", "tools/list"]
                    .into_iter()
                    .find(|m| request.contains(&format!("\"method\":\"{m}\"")))
                    .unwrap_or("");
                let pad = if method == oversize {
                    "x".repeat(CAP * 2)
                } else {
                    String::new()
                };
                let result = match method {
                    "initialize" => json!({"protocolVersion": "2025-03-26", "capabilities": {},
                        "serverInfo": {"name": "t", "version": "1", "pad": pad}}),
                    "tools/list" => json!({"tools": [{"name": "t", "description": pad,
                        "inputSchema": {"type": "object", "properties": {}}}]}),
                    _ => json!({}),
                };
                let body = json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string();
                let (content_type, body) = match wire {
                    Wire::Json => ("application/json", body),
                    Wire::Sse => (
                        "text/event-stream",
                        format!("event: message\ndata: {body}\n\n"),
                    ),
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nMcp-Session-Id: s\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            });
        }
    });
    url
}

async fn tool_count(url: &str, cap: usize) -> usize {
    let mut manager = McpManager::new();
    manager.set_max_response_bytes(Some(cap));
    manager.connect(url, None).await.unwrap();
    manager.list_tools().await.len()
}

#[tokio::test]
async fn oversized_handshake_responses_are_refused_for_json_and_sse() {
    for wire in [Wire::Json, Wire::Sse] {
        for oversize in ["initialize", "tools/list"] {
            let url = serve(wire, oversize).await;
            assert_eq!(tool_count(&url, CAP).await, 0, "{oversize} over the cap");
            assert_eq!(
                tool_count(&url, CAP * 8).await,
                1,
                "{oversize} under a larger cap is accepted"
            );
        }
    }
}
