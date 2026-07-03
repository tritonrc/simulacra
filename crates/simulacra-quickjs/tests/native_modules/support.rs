pub(super) use simulacra_quickjs::{FsProxy, JsRuntime, ModuleFetcher};
pub(super) use simulacra_types::VirtualFs;
pub(super) use simulacra_vfs::MemoryFs;
pub(super) use std::collections::HashMap;
pub(super) use std::sync::{Arc, Mutex};
pub(super) use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone)]
pub(super) struct CapturedSpan {
    pub(super) fields: HashMap<String, String>,
}

pub(super) struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan { fields });
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
}

pub(super) fn capture_spans<R>(operation: impl FnOnce() -> R) -> (R, Vec<CapturedSpan>) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
    });
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans.lock().unwrap().clone();
    (result, spans)
}

pub(super) fn span_operations(spans: &[CapturedSpan]) -> Vec<String> {
    let mut operations = spans
        .iter()
        .filter_map(|span| span.fields.get("simulacra.operation.name").cloned())
        .collect::<Vec<_>>();
    operations.sort();
    operations
}

pub(super) fn make_runtime() -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = make_runtime_with_vfs_proxy(Arc::clone(&vfs), None);
    (runtime, vfs)
}

pub(super) struct VfsBackedFsProxy {
    vfs: Arc<MemoryFs>,
}

impl VfsBackedFsProxy {
    pub(super) fn new(vfs: Arc<MemoryFs>) -> Self {
        Self { vfs }
    }
}

impl FsProxy for VfsBackedFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.vfs.read(path).map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
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

pub(super) fn make_runtime_with_vfs_proxy(
    vfs: Arc<MemoryFs>,
    fetcher: Option<Box<dyn ModuleFetcher>>,
) -> JsRuntime {
    let proxy: Arc<dyn FsProxy> = Arc::new(VfsBackedFsProxy::new(Arc::clone(&vfs)));
    JsRuntime::with_options(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        fetcher,
        Some(proxy),
    )
    .expect("failed to create runtime")
}

pub(super) fn make_runtime_with_env(env: HashMap<String, String>) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let runtime = JsRuntime::with_env(vfs_dyn, env).expect("failed to create runtime");
    (runtime, vfs)
}

pub(super) struct MockFetcher {
    responses: HashMap<String, Result<String, String>>,
}

impl MockFetcher {
    pub(super) fn new(responses: Vec<(&str, Result<&str, &str>)>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|(url, result)| {
                    (
                        url.to_string(),
                        result.map(str::to_string).map_err(str::to_string),
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

pub(super) fn make_runtime_with_fetcher(fetcher: MockFetcher) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = make_runtime_with_vfs_proxy(Arc::clone(&vfs), Some(Box::new(fetcher)));
    (runtime, vfs)
}
