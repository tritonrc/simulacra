mod eval;
mod globals;
mod modules;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::Ctx;
use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;

use crate::host_api::{DEFAULT_TIMEOUT, WORKFLOW_TIMEOUT};
use crate::{FsProxy, JsError, JsHostApiProfile, JsOutput, ModuleFetcher};

/// A minimal QuickJS sandbox with mediated host functions.
#[derive(Clone)]
pub struct JsRuntime {
    /// Maximum wall-clock time allowed for a single JS evaluation.
    timeout: Duration,
    /// Host-controlled environment variables exposed via `process.env`.
    env: HashMap<String, String>,
    /// Optional fetcher for remote ESM module source.
    module_fetcher: Option<Arc<dyn ModuleFetcher>>,
    /// Remote module source cache shared by fresh eval contexts owned by this
    /// wrapper. JS module instances are not shared across eval calls.
    module_source_cache: Arc<Mutex<HashMap<String, String>>>,
    /// Optional proxy for fs operations (capability checking).
    fs_proxy: Option<Arc<dyn FsProxy>>,
    /// Optional proxy for HTTP fetch operations (capability checking).
    fetch_proxy: Option<Arc<dyn simulacra_fetch::FetchProxy>>,
    /// Timestamp when the runtime was created, for `performance.now()`.
    runtime_start: Instant,
    /// Host API surface to install into fresh eval contexts.
    host_api: JsHostApiProfile,
}

impl JsRuntime {
    /// Create a new runtime with the default timeout and no env vars.
    ///
    /// Host functions (`console`, `fs`, `process`) are registered lazily on
    /// each [`eval`](Self::eval) call so the output buffer is fresh.
    pub fn new(vfs: Arc<dyn VirtualFs>) -> Result<Self, JsError> {
        Self::with_timeout(vfs, DEFAULT_TIMEOUT)
    }

    /// Create a new runtime with a custom execution timeout.
    pub fn with_timeout(vfs: Arc<dyn VirtualFs>, timeout: Duration) -> Result<Self, JsError> {
        Self::build(vfs, timeout, None, None, None, JsHostApiProfile::full())
    }

    /// Create a new runtime with a remote module fetcher.
    pub fn with_fetcher(
        vfs: Arc<dyn VirtualFs>,
        fetcher: Box<dyn ModuleFetcher>,
    ) -> Result<Self, JsError> {
        Self::build(
            vfs,
            DEFAULT_TIMEOUT,
            Some(fetcher),
            None,
            None,
            JsHostApiProfile::full(),
        )
    }

    /// Create a new runtime with a custom timeout and a remote module fetcher.
    pub fn with_timeout_and_fetcher(
        vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Box<dyn ModuleFetcher>,
    ) -> Result<Self, JsError> {
        Self::build(
            vfs,
            timeout,
            Some(fetcher),
            None,
            None,
            JsHostApiProfile::full(),
        )
    }

    /// Create a new runtime with a custom timeout, optional fetcher, and optional fs proxy.
    pub fn with_options(
        vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Option<Box<dyn ModuleFetcher>>,
        fs_proxy: Option<Arc<dyn FsProxy>>,
    ) -> Result<Self, JsError> {
        Self::build(
            vfs,
            timeout,
            fetcher,
            fs_proxy,
            None,
            JsHostApiProfile::full(),
        )
    }

    /// Create a new runtime with all optional components.
    pub fn with_all_options(
        vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Option<Box<dyn ModuleFetcher>>,
        fs_proxy: Option<Arc<dyn FsProxy>>,
        fetch_proxy: Option<Arc<dyn simulacra_fetch::FetchProxy>>,
    ) -> Result<Self, JsError> {
        Self::build(
            vfs,
            timeout,
            fetcher,
            fs_proxy,
            fetch_proxy,
            JsHostApiProfile::full(),
        )
    }

    /// Create a new runtime with an explicit host API profile.
    pub fn with_host_api_profile(
        vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Option<Box<dyn ModuleFetcher>>,
        fs_proxy: Option<Arc<dyn FsProxy>>,
        fetch_proxy: Option<Arc<dyn simulacra_fetch::FetchProxy>>,
        host_api: JsHostApiProfile,
    ) -> Result<Self, JsError> {
        Self::build(vfs, timeout, fetcher, fs_proxy, fetch_proxy, host_api)
    }

    fn build(
        _vfs: Arc<dyn VirtualFs>,
        timeout: Duration,
        fetcher: Option<Box<dyn ModuleFetcher>>,
        fs_proxy: Option<Arc<dyn FsProxy>>,
        fetch_proxy: Option<Arc<dyn simulacra_fetch::FetchProxy>>,
        host_api: JsHostApiProfile,
    ) -> Result<Self, JsError> {
        Ok(Self {
            timeout,
            env: HashMap::new(),
            module_fetcher: fetcher.map(|f| Arc::from(f) as Arc<dyn ModuleFetcher>),
            module_source_cache: Arc::new(Mutex::new(HashMap::new())),
            fs_proxy,
            fetch_proxy,
            runtime_start: Instant::now(),
            host_api,
        })
    }

    /// Create a new runtime with host-controlled environment variables.
    pub fn with_env(
        vfs: Arc<dyn VirtualFs>,
        env: HashMap<String, String>,
    ) -> Result<Self, JsError> {
        let mut runtime = Self::with_timeout(vfs, DEFAULT_TIMEOUT)?;
        runtime.env = env;
        Ok(runtime)
    }

    /// Evaluate `code` and return captured output.
    ///
    /// If the code contains `import` statements, it is automatically
    /// evaluated as an ESM module. Otherwise it runs as a plain script.
    pub fn eval(&self, code: &str) -> Result<JsOutput, JsError> {
        let runtime = self.clone();
        let code = code.to_string();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let parent_span = tracing::Span::current();
        std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let _parent_guard = parent_span.enter();
                let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(|e| {
                        JsError::Runtime(format!("failed to create JS async runtime: {e}"))
                    })?;
                let result = tokio_runtime.block_on(runtime.eval_async(&code));
                tokio_runtime.shutdown_timeout(Duration::from_millis(0));
                result
            })
        })
        .join()
        .map_err(|_| JsError::Runtime("JS eval thread panicked".into()))?
    }

    /// Evaluate a restricted workflow ESM module using the shared async
    /// QuickJS substrate.
    pub async fn eval_workflow_module_with_setup<F>(
        source: &str,
        setup: F,
    ) -> Result<String, JsError>
    where
        F: for<'js> FnOnce(&Ctx<'js>) -> Result<(), JsError> + Send + 'static,
    {
        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let runtime = Self::with_host_api_profile(
            vfs,
            WORKFLOW_TIMEOUT,
            None,
            None,
            None,
            JsHostApiProfile::workflow(),
        )?;
        runtime.eval_workflow_module_inner(source, setup).await
    }
}
