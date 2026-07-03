pub(super) use std::collections::HashMap;
pub(super) use std::future::Future;
pub(super) use std::pin::Pin;
pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::{Duration, Instant};

pub(super) use simulacra_quickjs::{
    FsProxy, JsError, JsHostApiProfile, JsOutput, JsRuntime, ModuleFetcher,
};
pub(super) use simulacra_types::VirtualFs;
pub(super) use simulacra_vfs::MemoryFs;

#[allow(dead_code)]
pub(super) trait MissingEvalAsyncFallback {
    fn eval_async<'a>(
        &'a self,
        code: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<JsOutput, JsError>> + 'a>>;
}

impl MissingEvalAsyncFallback for JsRuntime {
    fn eval_async<'a>(
        &'a self,
        code: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<JsOutput, JsError>> + 'a>> {
        let _ = (self, code);
        Box::pin(async {
            panic!(
                "JsRuntime::eval_async is not implemented; this fallback should be shadowed by the inherent S053 API"
            )
        })
    }
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
        self.vfs.read(path).map_err(|error| error.to_string())
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        self.vfs
            .write(path, data)
            .map_err(|error| error.to_string())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.vfs.list_dir(path).map_err(|error| error.to_string())
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let metadata = self.vfs.metadata(path).map_err(|error| error.to_string())?;
        Ok((metadata.is_file, metadata.is_dir, metadata.size))
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.vfs.remove(path).map_err(|error| error.to_string())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let data = self.vfs.read(from).map_err(|error| error.to_string())?;
        self.vfs
            .write(to, &data)
            .map_err(|error| error.to_string())?;
        self.vfs.remove(from).map_err(|error| error.to_string())
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        Ok(self.vfs.exists(path))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.vfs.mkdir(path).map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
pub(super) struct RecordingFetcher {
    responses: Arc<HashMap<String, Result<String, String>>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingFetcher {
    pub(super) fn new(
        responses: Vec<(&str, Result<&str, &str>)>,
    ) -> (Self, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fetcher = Self {
            responses: Arc::new(
                responses
                    .into_iter()
                    .map(|(url, response)| {
                        (
                            url.to_string(),
                            response.map(str::to_string).map_err(str::to_string),
                        )
                    })
                    .collect(),
            ),
            calls: Arc::clone(&calls),
        };
        (fetcher, calls)
    }
}

impl ModuleFetcher for RecordingFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self.calls
            .lock()
            .expect("fetch calls lock should not be poisoned")
            .push(url.to_string());
        self.responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(format!("no test module fixture for {url}")))
    }
}

pub(super) struct PrefetchOrderFetcher {
    pub(super) vfs: Arc<MemoryFs>,
    pub(super) calls: Arc<Mutex<Vec<String>>>,
}

impl ModuleFetcher for PrefetchOrderFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self.calls
            .lock()
            .expect("fetch calls lock should not be poisoned")
            .push(url.to_string());
        match url {
            "https://modules.invalid/entry.js" => Ok(r#"
                import marker from "https://modules.invalid/marker.js";
                fs.writeFileSync("/workspace/entry-evaluated.txt", "yes");
                export default marker;
                "#
            .to_string()),
            "https://modules.invalid/marker.js" => {
                if self.vfs.exists("/workspace/entry-evaluated.txt") {
                    return Err(
                        "transitive remote import was fetched after parent module evaluation"
                            .to_string(),
                    );
                }
                Ok(r#"export default "marker-ready";"#.to_string())
            }
            other => Err(format!("unexpected module fetch: {other}")),
        }
    }
}

pub(super) struct SlowFetcher {
    pub(super) delay: Duration,
}

impl ModuleFetcher for SlowFetcher {
    fn fetch(&self, _url: &str) -> Result<String, String> {
        std::thread::sleep(self.delay);
        Ok(r#"export default "late";"#.to_string())
    }
}

pub(super) struct SlowThenFastFetcher {
    pub(super) calls: Arc<Mutex<usize>>,
}

impl ModuleFetcher for SlowThenFastFetcher {
    fn fetch(&self, _url: &str) -> Result<String, String> {
        let call_index = {
            let mut calls = self
                .calls
                .lock()
                .expect("calls lock should not be poisoned");
            let call_index = *calls;
            *calls += 1;
            call_index
        };
        if call_index == 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        Ok(format!(r#"export default "call-{call_index}";"#))
    }
}

pub(super) fn runtime(timeout: Duration) -> JsRuntime {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    JsRuntime::with_timeout(vfs, timeout).expect("runtime should be created")
}

pub(super) fn execution_message(error: JsError) -> String {
    match error {
        JsError::Execution(message) => message,
        other => panic!("expected JsError::Execution, got {other:?}"),
    }
}

pub(super) fn assert_timeout_message(message: &str) {
    let lower = message.to_lowercase();
    assert!(
        lower.contains("timeout") || lower.contains("timed out") || lower.contains("interrupt"),
        "expected timeout or interrupt error, got {message:?}"
    );
}
