use super::super::trace_support::{CapturedEvent, CapturedSpan, TraceCaptureLayer};
use super::*;

pub(crate) struct MockFsProxy {
    pub(crate) store: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl MockFsProxy {
    pub(crate) fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Seed a file into the mock store (for test setup).
    pub(crate) fn seed(&self, path: &str, data: &[u8]) {
        self.store
            .lock()
            .unwrap()
            .insert(path.to_string(), data.to_vec());
    }
}

impl FsProxy for MockFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let _span = tracing::info_span!(
            "sandbox_read_file",
            simulacra.operation.name = "sandbox_read_file",
            path = %path,
        )
        .entered();
        self.store
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| format!("file not found: {path}"))
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        let _span = tracing::info_span!(
            "sandbox_write_file",
            simulacra.operation.name = "sandbox_write_file",
            path = %path,
        )
        .entered();
        self.store
            .lock()
            .unwrap()
            .insert(path.to_string(), data.to_vec());
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let store = self.store.lock().unwrap();
        let prefix = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        let mut entries: Vec<String> = store
            .keys()
            .filter_map(|k| {
                k.strip_prefix(&prefix).and_then(|rest| {
                    let name = rest.split('/').next()?;
                    if name.is_empty() {
                        None
                    } else {
                        Some(name.to_string())
                    }
                })
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        entries.sort();
        Ok(entries)
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let store = self.store.lock().unwrap();
        if let Some(data) = store.get(path) {
            Ok((true, false, data.len() as u64))
        } else {
            // Check if it's a directory (any key starts with path/)
            let prefix = if path.ends_with('/') {
                path.to_string()
            } else {
                format!("{path}/")
            };
            if store.keys().any(|k| k.starts_with(&prefix)) {
                Ok((false, true, 0))
            } else {
                Err(format!("not found: {path}"))
            }
        }
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.store
            .lock()
            .unwrap()
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| format!("not found: {path}"))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let mut store = self.store.lock().unwrap();
        let data = store
            .remove(from)
            .ok_or_else(|| format!("not found: {from}"))?;
        store.insert(to.to_string(), data);
        Ok(())
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        let store = self.store.lock().unwrap();
        if store.contains_key(path) {
            return Ok(true);
        }
        let prefix = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        Ok(store.keys().any(|k| k.starts_with(&prefix)))
    }

    fn mkdir(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }
}

pub(crate) struct VfsBackedFsProxy {
    vfs: Arc<MemoryFs>,
}

impl VfsBackedFsProxy {
    pub(crate) fn new(vfs: Arc<MemoryFs>) -> Self {
        Self { vfs }
    }
}

impl FsProxy for VfsBackedFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let _span = tracing::info_span!(
            "vfs_read",
            simulacra.operation.name = "vfs_read",
            path = %path,
        )
        .entered();
        self.vfs.read(path).map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        let _span = tracing::info_span!(
            "vfs_write",
            simulacra.operation.name = "vfs_write",
            path = %path,
        )
        .entered();
        self.vfs.write(path, data).map_err(|e| e.to_string())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.vfs.list_dir(path).map_err(|e| e.to_string())
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let meta = self.vfs.metadata(path).map_err(|e| e.to_string())?;
        Ok((meta.is_file, meta.is_dir, meta.size))
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.vfs.remove(path).map_err(|e| e.to_string())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let data = self.vfs.read(from).map_err(|e| e.to_string())?;
        if let Some(parent) = std::path::Path::new(to).parent() {
            let parent = parent.to_string_lossy();
            if !parent.is_empty() && parent != "/" {
                let _ = self.vfs.mkdir(&parent);
            }
        }
        self.vfs.write(to, &data).map_err(|e| e.to_string())?;
        self.vfs.remove(from).map_err(|e| e.to_string())
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        Ok(self.vfs.exists(path))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.vfs.mkdir(path).map_err(|e| e.to_string())
    }
}

/// A mock fetch proxy that returns canned responses for allowed URLs.
pub(crate) struct MockFetchProxy {
    fixtures: HashMap<String, FetchFixture>,
    allowed_hosts: Vec<String>,
}

impl FetchProxy for MockFetchProxy {
    fn fetch(
        &self,
        url: &str,
        method: &str,
        _headers: &[(String, String)],
        _body: Option<&[u8]>,
        _timeout_ms: Option<u64>,
    ) -> Result<FetchResponse, FetchError> {
        // Emit a span mimicking what AgentCellFetchProxy would emit via fetch_http_inner
        tracing::callsite::rebuild_interest_cache();
        let span = tracing::info_span!(
            "sandbox_http_fetch",
            simulacra.operation.name = "sandbox_http_fetch",
            simulacra.http.url = %url,
            simulacra.http.method = %method,
            simulacra.http.status = tracing::field::Empty,
        );
        let _guard = span.enter();

        // Check if the URL's host is allowed
        let host = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");

        if !self.allowed_hosts.iter().any(|h| h == host) {
            return Err(FetchError::CapabilityDenied(format!(
                "network access to {host} is not allowed"
            )));
        }

        if let Some(fixture) = self.fixtures.get(url) {
            tracing::Span::current().record("simulacra.http.status", fixture.status);
            Ok(FetchResponse {
                status: fixture.status,
                status_text: String::new(),
                headers: fixture
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: fixture.body.as_bytes().to_vec(),
                url: url.to_string(),
                redirected: false,
            })
        } else {
            Err(FetchError::NetworkError(format!(
                "MockFetchProxy: no fixture for '{url}'"
            )))
        }
    }
}

pub(crate) fn make_runtime() -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    (runtime, vfs)
}

pub(crate) fn make_runtime_with_vfs_proxy(vfs: Arc<MemoryFs>) -> JsRuntime {
    let proxy: Arc<dyn FsProxy> = Arc::new(VfsBackedFsProxy::new(Arc::clone(&vfs)));
    JsRuntime::with_options(
        vfs.clone() as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        None,
        Some(proxy),
    )
    .expect("failed to create runtime")
}

/// A mock fetcher for testing remote module imports.
pub(crate) struct MockFetcher {
    responses: HashMap<String, Result<String, String>>,
}

impl MockFetcher {
    pub(crate) fn new(responses: Vec<(&str, Result<&str, &str>)>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|(url, result)| {
                    (
                        url.to_string(),
                        result.map(|s| s.to_string()).map_err(|s| s.to_string()),
                    )
                })
                .collect(),
        }
    }
}

impl ModuleFetcher for MockFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self.responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(format!("MockFetcher: no response configured for '{url}'")))
    }
}

pub(crate) fn make_runtime_with_fetcher(fetcher: MockFetcher) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_fetcher(vfs.clone() as Arc<dyn VirtualFs>, Box::new(fetcher))
        .expect("failed to create runtime");
    (runtime, vfs)
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct FetchFixture {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
}

impl FetchFixture {
    pub(crate) fn text(status: u16, body: &'static str) -> Self {
        Self {
            status,
            headers: vec![("content-type", "text/plain")],
            body,
        }
    }

    pub(crate) fn json(status: u16, body: &'static str) -> Self {
        Self {
            status,
            headers: vec![("content-type", "application/json")],
            body,
        }
    }
}

pub(crate) fn make_runtime_with_fetch_fixtures(
    allowed_hosts: &[&str],
    fixtures: Vec<(&str, FetchFixture)>,
) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let fetch_proxy = Arc::new(MockFetchProxy {
        fixtures: fixtures
            .into_iter()
            .map(|(url, fix)| (url.to_string(), fix))
            .collect(),
        allowed_hosts: allowed_hosts.iter().map(|h| h.to_string()).collect(),
    });
    let runtime = JsRuntime::with_all_options(
        vfs.clone() as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        None,
        None,
        Some(fetch_proxy as Arc<dyn simulacra_fetch::FetchProxy>),
    )
    .expect("failed to create runtime");
    (runtime, vfs)
}

pub(crate) fn capture_trace<T>(
    f: impl FnOnce() -> T,
) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    static GLOBAL_TRACING: OnceLock<()> = OnceLock::new();
    GLOBAL_TRACING.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        tracing::callsite::rebuild_interest_cache();
    });

    static CAPTURE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = CAPTURE_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("capture mutex should not be poisoned");

    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let layer = TraceCaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    };
    let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
    let result = tracing::subscriber::with_default(subscriber, || {
        // Rebuild interest cache so that callsites registered on other threads
        // (where no subscriber was active) are re-evaluated against this subscriber.
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

pub(crate) fn field_matches(fields: &HashMap<String, String>, key: &str, expected: &str) -> bool {
    fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

pub(crate) fn find_span<'a>(spans: &'a [CapturedSpan], operation: &str) -> &'a CapturedSpan {
    spans
        .iter()
        .find(|span| field_matches(&span.fields, "simulacra.operation.name", operation))
        .unwrap_or_else(|| panic!("expected {operation} span, got {spans:#?}"))
}

pub(crate) fn event_text(event: &CapturedEvent) -> String {
    event
        .fields
        .values()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn execution_message(error: JsError) -> String {
    match error {
        JsError::Execution(message) => message,
        other => panic!("expected execution error, got {other:?}"),
    }
}

pub(crate) fn assert_contains_all(message: &str, expected: &[&str]) {
    for needle in expected {
        assert!(
            message.contains(needle),
            "expected {message:?} to contain {needle:?}"
        );
    }
}
