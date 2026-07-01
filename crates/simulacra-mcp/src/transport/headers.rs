pub(crate) fn apply_connection_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    if !headers.is_empty() {
        tracing::debug!(
            connection.headers = %redact_headers_for_log(headers),
            "applying connection headers to MCP request"
        );
    }

    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
}

/// Return a log-safe display string for connection headers, masking secret values.
pub fn redact_headers_for_log(headers: &[(String, String)]) -> String {
    let parts = headers
        .iter()
        .map(|(name, value)| {
            let lower_name = name.to_ascii_lowercase();
            let redact_value = matches!(
                lower_name.as_str(),
                "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
            ) || lower_name.starts_with("x-mcp-")
                || lower_name.starts_with("x-api")
                || lower_name.ends_with("-token")
                || lower_name.ends_with("-key")
                || lower_name.ends_with("-secret")
                || lower_name.ends_with("-auth");
            let display_value = if redact_value { "***" } else { value.as_str() };
            format!("{name}: {display_value}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!("[{parts}]")
}
