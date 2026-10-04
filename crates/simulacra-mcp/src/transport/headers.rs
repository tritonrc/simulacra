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

/// Return a log-safe display string for connection headers: the names only.
/// Which header carries a credential is the operator's choice, so no value is
/// ever judged safe to print.
pub fn redact_headers_for_log(headers: &[(String, String)]) -> String {
    let names = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    format!("[{names}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_header_value_is_never_logged_whatever_the_header_is_called() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let headers = vec![
            ("DD_API_KEY".to_owned(), "SENTINEL-api".to_owned()),
            ("DD_APPLICATION_KEY".to_owned(), "SENTINEL-app".to_owned()),
            (
                "Authorization".to_owned(),
                "Bearer SENTINEL-bearer".to_owned(),
            ),
            ("X-Anything".to_owned(), "SENTINEL-other".to_owned()),
        ];

        tracing::subscriber::with_default(subscriber, || {
            let request = reqwest::Client::new().get("http://localhost/");
            let _ = apply_connection_headers(request, &headers);
        });

        let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("DD_API_KEY"), "positive control: {logged}");
        assert!(!logged.contains("SENTINEL"), "a value was logged: {logged}");
    }
}
