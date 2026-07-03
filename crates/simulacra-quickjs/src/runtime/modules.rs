use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rquickjs::module::Module;
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Value};

use super::JsRuntime;
use crate::module_loading::{
    PrefetchedRemoteModules, RemoteUrlSet, SimulacraLoader, SimulacraResolver,
    is_remote_module_url, resolve_module_specifier, static_import_specifiers,
};
use crate::native_modules::{ConsoleModule, FsModule, ProcessModule};
use crate::{JsError, crypto_module, path_module};

impl JsRuntime {
    pub(super) async fn prefetch_remote_static_imports(
        &self,
        root_source: &str,
        allowed_remote_urls: &RemoteUrlSet,
        fetched_remote_urls: &RemoteUrlSet,
    ) -> Result<(), JsError> {
        let runtime = self.clone();
        let source = root_source.to_string();
        let span = tracing::Span::current();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let handle = tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let _guard = span.enter();
                runtime.prefetch_remote_static_imports_owned(&source)
            })
        });
        let prefetched = tokio::time::timeout(self.timeout, handle)
            .await
            .map_err(|_| JsError::Execution("JavaScript evaluation timed out".into()))?
            .map_err(|error| JsError::Runtime(format!("module prefetch task failed: {error}")))??;

        if !prefetched.sources.is_empty() {
            self.module_source_cache
                .lock()
                .map_err(|e| JsError::Runtime(format!("module source cache mutex poisoned: {e}")))?
                .extend(prefetched.sources);
        }
        allowed_remote_urls.borrow_mut().extend(prefetched.allowed);
        fetched_remote_urls.borrow_mut().extend(prefetched.fetched);
        Ok(())
    }

    fn prefetch_remote_static_imports_owned(
        &self,
        root_source: &str,
    ) -> Result<PrefetchedRemoteModules, JsError> {
        let mut stack = vec![("<eval>".to_string(), root_source.to_string())];
        let mut visited = HashSet::new();
        let mut allowed_remote_urls = HashSet::new();
        let mut fetched_remote_urls = HashSet::new();
        let mut new_sources: HashMap<String, String> = HashMap::new();

        while let Some((base, source)) = stack.pop() {
            for specifier in static_import_specifiers(&source) {
                if specifier.starts_with("simulacra:") {
                    continue;
                }
                let resolved = resolve_module_specifier(&base, &specifier).map_err(|message| {
                    tracing::error!(
                        specifier = %specifier,
                        reason = %message,
                        "module resolution failed"
                    );
                    JsError::Execution(message)
                })?;
                if !is_remote_module_url(&resolved) {
                    if resolved.starts_with('/')
                        && visited.insert(resolved.clone())
                        && let Some(proxy) = self.fs_proxy.as_ref()
                    {
                        let data = proxy.read_file(&resolved).map_err(|e| {
                            JsError::Execution(format!(
                                "Failed to load VFS module '{resolved}' during prefetch: {e}"
                            ))
                        })?;
                        let source = String::from_utf8(data).map_err(|e| {
                            JsError::Execution(format!(
                                "VFS module '{resolved}' is not valid UTF-8: {e}"
                            ))
                        })?;
                        stack.push((resolved, source));
                    }
                    continue;
                }

                allowed_remote_urls.insert(resolved.clone());
                if !visited.insert(resolved.clone()) {
                    continue;
                }

                let cached = if let Some(source) = new_sources.get(&resolved) {
                    Some(source.clone())
                } else {
                    self.module_source_cache
                        .lock()
                        .map_err(|e| {
                            JsError::Runtime(format!("module source cache mutex poisoned: {e}"))
                        })?
                        .get(&resolved)
                        .cloned()
                };

                let remote_source = if let Some(source) = cached {
                    source
                } else {
                    let fetcher = self.module_fetcher.as_ref().ok_or_else(|| {
                        JsError::Execution(format!(
                            "No module fetcher configured for remote module: '{resolved}'"
                        ))
                    })?;

                    let _span = tracing::info_span!(
                        "module_fetch",
                        simulacra.operation.name = "module_fetch",
                        simulacra.module.url = %resolved,
                    )
                    .entered();

                    let source = fetcher.fetch(&resolved).map_err(JsError::Execution)?;
                    new_sources.insert(resolved.clone(), source.clone());
                    fetched_remote_urls.insert(resolved.clone());
                    tracing::info!(simulacra.module.fetches = 1u64, "remote module fetched");
                    source
                };

                stack.push((resolved, remote_source));
            }
        }

        Ok(PrefetchedRemoteModules {
            allowed: allowed_remote_urls,
            fetched: fetched_remote_urls,
            sources: new_sources,
        })
    }

    pub(super) async fn fresh_async_engine(
        &self,
        allowed_remote_urls: RemoteUrlSet,
        fetched_remote_urls: RemoteUrlSet,
    ) -> Result<(AsyncRuntime, AsyncContext), JsError> {
        let rt = AsyncRuntime::new().map_err(|e| JsError::Runtime(e.to_string()))?;

        if self.host_api.module_loader || self.host_api.simulacra_modules {
            rt.set_loader(
                SimulacraResolver,
                SimulacraLoader {
                    fs_proxy: self.fs_proxy.clone(),
                    source_cache: Arc::clone(&self.module_source_cache),
                    allowed_remote_urls,
                    fetched_remote_urls,
                },
            )
            .await;
        }

        let ctx = AsyncContext::full(&rt)
            .await
            .map_err(|e| JsError::Runtime(e.to_string()))?;
        Ok((rt, ctx))
    }

    pub(super) async fn register_native_modules_async(
        ctx: &rquickjs::Ctx<'_>,
    ) -> Result<(), JsError> {
        let (_module, promise) =
            Module::evaluate_def::<FsModule, _>(ctx.clone(), "simulacra:fs")
                .map_err(|e| JsError::Runtime(format!("failed to register simulacra:fs: {e}")))?;
        let _: Value<'_> = promise.into_future().await.catch(ctx).map_err(|caught| {
            JsError::Runtime(format!("failed to evaluate simulacra:fs: {caught}"))
        })?;

        let (_module, promise) =
            Module::evaluate_def::<ConsoleModule, _>(ctx.clone(), "simulacra:console").map_err(
                |e| JsError::Runtime(format!("failed to register simulacra:console: {e}")),
            )?;
        let _: Value<'_> = promise.into_future().await.catch(ctx).map_err(|caught| {
            JsError::Runtime(format!("failed to evaluate simulacra:console: {caught}"))
        })?;

        let (_module, promise) =
            Module::evaluate_def::<ProcessModule, _>(ctx.clone(), "simulacra:process").map_err(
                |e| JsError::Runtime(format!("failed to register simulacra:process: {e}")),
            )?;
        let _: Value<'_> = promise.into_future().await.catch(ctx).map_err(|caught| {
            JsError::Runtime(format!("failed to evaluate simulacra:process: {caught}"))
        })?;

        let (_module, promise) =
            Module::evaluate_def::<path_module::PathModule, _>(ctx.clone(), "simulacra:path")
                .map_err(|e| JsError::Runtime(format!("failed to register simulacra:path: {e}")))?;
        let _: Value<'_> = promise.into_future().await.catch(ctx).map_err(|caught| {
            JsError::Runtime(format!("failed to evaluate simulacra:path: {caught}"))
        })?;
        tracing::debug!("simulacra:path module loaded");

        let (_module, promise) =
            Module::evaluate_def::<crypto_module::CryptoModule, _>(ctx.clone(), "simulacra:crypto")
                .map_err(|e| {
                    JsError::Runtime(format!("failed to register simulacra:crypto: {e}"))
                })?;
        let _: Value<'_> = promise.into_future().await.catch(ctx).map_err(|caught| {
            JsError::Runtime(format!("failed to evaluate simulacra:crypto: {caught}"))
        })?;
        tracing::debug!("simulacra:crypto module loaded");

        Ok(())
    }
}
