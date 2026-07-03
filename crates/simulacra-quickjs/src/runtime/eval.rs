use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use rquickjs::context::EvalOptions;
use rquickjs::promise::MaybePromise;
use rquickjs::{CatchResultExt, Promise, Value};
use tracing::Instrument;

use super::JsRuntime;
use crate::module_loading::{RemoteUrlSet, extract_module_loading_error, wrap_module_for_result};
use crate::{JsError, JsHostApiProfile, JsOutput, install_workflow_api_restrictions};

impl JsRuntime {
    /// Evaluate `code` asynchronously and return captured output.
    ///
    /// Each call creates a fresh QuickJS runtime/context. Host configuration and
    /// remote source caches remain on the `JsRuntime` wrapper.
    pub async fn eval_async(&self, code: &str) -> Result<JsOutput, JsError> {
        let code = code.to_string();

        let span = tracing::info_span!(
            "js_execute",
            simulacra.operation.name = "js_execute",
            simulacra.js.module = "<eval>",
        );

        async move {
            tokio::time::timeout(self.timeout, self.eval_async_inner(&code))
                .await
                .map_err(|_| JsError::Execution("JavaScript evaluation timed out".into()))?
        }
        .instrument(span)
        .await
    }

    async fn eval_async_inner(&self, code: &str) -> Result<JsOutput, JsError> {
        let allowed_remote_urls: RemoteUrlSet = Rc::new(RefCell::new(HashSet::new()));
        let fetched_remote_urls: RemoteUrlSet = Rc::new(RefCell::new(HashSet::new()));
        if self.host_api.module_loader {
            self.prefetch_remote_static_imports(code, &allowed_remote_urls, &fetched_remote_urls)
                .await?;
        }

        let deadline = Instant::now() + self.timeout;
        let (rt, ctx) = self
            .fresh_async_engine(allowed_remote_urls, fetched_remote_urls)
            .await?;
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
            .await;

        let is_module = code.contains("import ") || code.contains("export ");
        ctx.async_with(async |ctx| {
            let (stdout_buf, exit_code_cell) = self.register_globals(&ctx)?;
            if self.host_api == JsHostApiProfile::workflow() {
                install_workflow_api_restrictions(&ctx)
                    .map_err(|e| JsError::Runtime(e.to_string()))?;
            }
            if self.host_api.simulacra_modules {
                Self::register_native_modules_async(&ctx).await?;
            }
            if is_module {
                self.eval_as_module_async(&ctx, code, &stdout_buf, &exit_code_cell)
                    .await
            } else {
                self.eval_as_script_async(&ctx, code, &stdout_buf, &exit_code_cell)
                    .await
            }
        })
        .await
    }

    pub(super) async fn eval_workflow_module_inner<F>(
        &self,
        source: &str,
        setup: F,
    ) -> Result<String, JsError>
    where
        F: for<'js> FnOnce(&rquickjs::Ctx<'js>) -> Result<(), JsError> + Send + 'static,
    {
        let source = source.to_string();
        tokio::time::timeout(self.timeout, async move {
            let allowed_remote_urls: RemoteUrlSet = Rc::new(RefCell::new(HashSet::new()));
            let fetched_remote_urls: RemoteUrlSet = Rc::new(RefCell::new(HashSet::new()));
            let deadline = Instant::now() + self.timeout;
            let (rt, ctx) = self
                .fresh_async_engine(allowed_remote_urls, fetched_remote_urls)
                .await?;
            rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
                .await;

            let mut setup = Some(setup);
            ctx.async_with(async move |ctx| {
                install_workflow_api_restrictions(&ctx)
                    .map_err(|e| JsError::Runtime(e.to_string()))?;
                let setup = setup
                    .take()
                    .ok_or_else(|| JsError::Runtime("workflow setup already consumed".into()))?;
                setup(&ctx)?;

                let mut opts = EvalOptions::default();
                opts.global = false;
                opts.promise = true;
                let promise: Promise<'_> = ctx
                    .eval_with_options(source, opts)
                    .catch(&ctx)
                    .map_err(|caught| JsError::Execution(format!("{caught}")))?;
                let _: Value<'_> = promise
                    .into_future()
                    .await
                    .catch(&ctx)
                    .map_err(|caught| JsError::Execution(format!("{caught}")))?;
                let result: Value<'_> = ctx
                    .eval("globalThis.__simulacraWorkflowResult__")
                    .catch(&ctx)
                    .map_err(|caught| JsError::Execution(format!("{caught}")))?;
                rquickjs_serde::from_value(result)
                    .map_err(|error| JsError::Execution(error.to_string()))
            })
            .await
        })
        .await
        .map_err(|_| JsError::Execution("JavaScript evaluation timed out".into()))?
    }

    async fn eval_as_script_async(
        &self,
        ctx: &rquickjs::Ctx<'_>,
        code: &str,
        stdout_buf: &Rc<RefCell<String>>,
        exit_code_cell: &Rc<RefCell<Option<i32>>>,
    ) -> Result<JsOutput, JsError> {
        let res: rquickjs::Result<rquickjs::Value<'_>> = ctx.eval(code.to_string());
        match res.catch(ctx) {
            Ok(val) => {
                let resolved = match MaybePromise::from_value(val)
                    .into_future::<Value<'_>>()
                    .await
                    .catch(ctx)
                {
                    Ok(value) => value,
                    Err(caught) => return Self::handle_error(caught, stdout_buf, exit_code_cell),
                };

                let exit_code = exit_code_cell.borrow_mut().take();
                Ok(JsOutput {
                    stdout: stdout_buf.borrow().clone(),
                    result: Self::extract_result(&resolved),
                    exit_code,
                })
            }
            Err(caught) => Self::handle_error(caught, stdout_buf, exit_code_cell),
        }
    }

    async fn eval_as_module_async(
        &self,
        ctx: &rquickjs::Ctx<'_>,
        code: &str,
        stdout_buf: &Rc<RefCell<String>>,
        exit_code_cell: &Rc<RefCell<Option<i32>>>,
    ) -> Result<JsOutput, JsError> {
        let wrapped = wrap_module_for_result(code);

        let mut opts = EvalOptions::default();
        opts.global = false;
        opts.promise = true;
        let res: rquickjs::Result<Promise<'_>> = ctx.eval_with_options(wrapped, opts);

        match res.catch(ctx) {
            Ok(promise) => match promise.into_future::<Value<'_>>().await.catch(ctx) {
                Ok(_) => {
                    let exit_code = exit_code_cell.borrow_mut().take();
                    let result_val: rquickjs::Result<rquickjs::Value<'_>> =
                        ctx.eval("globalThis.__simulacraResult__");
                    let result_val = match result_val.catch(ctx) {
                        Ok(value) => value,
                        Err(caught) => {
                            return Self::handle_error(caught, stdout_buf, exit_code_cell);
                        }
                    };
                    let resolved = match MaybePromise::from_value(result_val)
                        .into_future::<Value<'_>>()
                        .await
                        .catch(ctx)
                    {
                        Ok(value) => value,
                        Err(caught) => {
                            return Self::handle_error(caught, stdout_buf, exit_code_cell);
                        }
                    };
                    Ok(JsOutput {
                        stdout: stdout_buf.borrow().clone(),
                        result: Self::extract_result(&resolved),
                        exit_code,
                    })
                }
                Err(caught) => Self::handle_error(caught, stdout_buf, exit_code_cell),
            },
            Err(caught) => Self::handle_error(caught, stdout_buf, exit_code_cell),
        }
    }

    fn extract_result(val: &rquickjs::Value<'_>) -> Option<String> {
        if val.is_undefined() || val.is_null() {
            None
        } else if let Some(s) = val.as_string() {
            Some(s.to_string().unwrap_or_else(|_| format!("{val:?}")))
        } else if let Some(n) = val.as_int() {
            Some(n.to_string())
        } else if let Some(n) = val.as_float() {
            Some(n.to_string())
        } else if let Some(b) = val.as_bool() {
            Some(b.to_string())
        } else {
            Some(format!("{val:?}"))
        }
    }

    fn handle_error(
        caught: rquickjs::CaughtError<'_>,
        stdout_buf: &Rc<RefCell<String>>,
        exit_code_cell: &Rc<RefCell<Option<i32>>>,
    ) -> Result<JsOutput, JsError> {
        let exit_code = exit_code_cell.borrow_mut().take();
        if exit_code.is_some() {
            return Ok(JsOutput {
                stdout: stdout_buf.borrow().clone(),
                result: None,
                exit_code,
            });
        }

        let msg = format!("{caught}");
        let msg = extract_module_loading_error(&msg).unwrap_or(msg);
        tracing::error!(exception.message = %msg, "uncaught JS exception");
        Err(JsError::Execution(msg))
    }
}
