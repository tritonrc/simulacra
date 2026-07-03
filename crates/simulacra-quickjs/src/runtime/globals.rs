use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use rquickjs::{Function, Object};

use super::JsRuntime;
use crate::formatting::format_js_value;
use crate::{JsError, fs_proxy_required_error, globals};

impl JsRuntime {
    /// Register all host globals (`console`, `fs`, `process`) and return
    /// shared cells for stdout capture and exit code interception.
    #[allow(clippy::type_complexity)]
    pub(super) fn register_globals(
        &self,
        ctx: &rquickjs::Ctx<'_>,
    ) -> Result<(Rc<RefCell<String>>, Rc<RefCell<Option<i32>>>), JsError> {
        let stdout_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let exit_code_cell: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));

        let globals_obj = ctx.globals();

        if self.host_api.console {
            let console = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;
            let buf = Rc::clone(&stdout_buf);
            let log_fn = Function::new(
                ctx.clone(),
                move |args: rquickjs::function::Rest<rquickjs::Value<'_>>| {
                    let parts: Vec<String> = args
                        .0
                        .iter()
                        .map(|v| format_js_value(v, 0, &mut HashSet::new()))
                        .collect();
                    let line = parts.join(" ");
                    buf.borrow_mut().push_str(&line);
                    buf.borrow_mut().push('\n');
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?;

            console
                .set("log", log_fn)
                .map_err(|e| JsError::Runtime(e.to_string()))?;
            globals_obj
                .set("console", console)
                .map_err(|e| JsError::Runtime(e.to_string()))?;
        }

        if self.host_api.fs {
            self.register_fs_global(ctx, &globals_obj)?;
        }

        if self.host_api.process {
            let process = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;
            let env_obj = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;
            for (key, value) in &self.env {
                env_obj
                    .set(key.as_str(), value.as_str())
                    .map_err(|e| JsError::Runtime(e.to_string()))?;
            }
            process
                .set("env", env_obj)
                .map_err(|e| JsError::Runtime(e.to_string()))?;

            let cwd_fn = Function::new(ctx.clone(), || -> String { "/workspace".to_string() })
                .map_err(|e| JsError::Runtime(e.to_string()))?;
            process
                .set("cwd", cwd_fn)
                .map_err(|e| JsError::Runtime(e.to_string()))?;

            let exit_code_writer = Rc::clone(&exit_code_cell);
            let exit_fn = Function::new(
                ctx.clone(),
                move |code: rquickjs::function::Opt<i32>| -> rquickjs::Result<()> {
                    *exit_code_writer.borrow_mut() = Some(code.0.unwrap_or(0));
                    Err(rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        "__SIMULACRA_PROCESS_EXIT__",
                    ))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?;
            process
                .set("exit", exit_fn)
                .map_err(|e| JsError::Runtime(e.to_string()))?;

            globals_obj
                .set("process", process)
                .map_err(|e| JsError::Runtime(e.to_string()))?;
        }

        if self.host_api.fetch
            && let Some(ref fetch_proxy) = self.fetch_proxy
        {
            simulacra_fetch::register_globals(ctx, Arc::clone(fetch_proxy))
                .map_err(|e| JsError::Runtime(e.to_string()))?;
        }

        if self.host_api.web_globals {
            globals::register_web_globals(ctx, &stdout_buf, self.runtime_start)?;
        }

        Ok((stdout_buf, exit_code_cell))
    }

    fn register_fs_global<'js>(
        &self,
        ctx: &rquickjs::Ctx<'js>,
        globals_obj: &Object<'js>,
    ) -> Result<(), JsError> {
        let fs = Object::new(ctx.clone()).map_err(|e| JsError::Runtime(e.to_string()))?;

        let read_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String| -> rquickjs::Result<String> {
                    let data = proxy.read_file(&path).map_err(|e| {
                        rquickjs::Error::new_from_js_message("string", "string", &e)
                    })?;
                    String::from_utf8(data).map_err(|e| {
                        rquickjs::Error::new_from_js_message("string", "string", &e.to_string())
                    })
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<String> { Err(fs_proxy_required_error()) },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("readFileSync", read_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let write_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String, data: String| -> rquickjs::Result<()> {
                    proxy
                        .write_file(&path, data.as_bytes())
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String, _data: String| -> rquickjs::Result<()> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("writeFileSync", write_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let exists_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<bool> {
                proxy
                    .exists(&path)
                    .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<bool> { Err(fs_proxy_required_error()) },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("existsSync", exists_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let mkdir_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<()> {
                proxy
                    .mkdir(&path)
                    .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(ctx.clone(), move |_path: String| -> rquickjs::Result<()> {
                Err(fs_proxy_required_error())
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("mkdirSync", mkdir_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let readdir_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String| -> rquickjs::Result<Vec<String>> {
                    proxy
                        .list_dir(&path)
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<Vec<String>> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("readdirSync", readdir_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let stat_helper_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String| -> rquickjs::Result<Vec<String>> {
                    let (is_file, is_dir, size) = proxy.stat(&path).map_err(|e| {
                        rquickjs::Error::new_from_js_message("string", "string", &e)
                    })?;
                    Ok(vec![
                        is_file.to_string(),
                        is_dir.to_string(),
                        size.to_string(),
                    ])
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String| -> rquickjs::Result<Vec<String>> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        ctx.globals()
            .set("__simulacra_fs_stat", stat_helper_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;
        ctx.eval::<(), _>(
            r#"globalThis.__simulacra_fs_statSync = function(path) {
                const parts = __simulacra_fs_stat(path);
                return {
                    isFile: parts[0] === 'true',
                    isDirectory: parts[1] === 'true',
                    size: Number(parts[2])
                };
            };"#,
        )
        .map_err(|e| JsError::Runtime(format!("statSync wrapper: {e}")))?;
        let stat_fn: Function<'_> = ctx
            .globals()
            .get("__simulacra_fs_statSync")
            .map_err(|e| JsError::Runtime(e.to_string()))?;
        fs.set("statSync", stat_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let unlink_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<()> {
                proxy
                    .remove(&path)
                    .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(ctx.clone(), move |_path: String| -> rquickjs::Result<()> {
                Err(fs_proxy_required_error())
            })
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("unlinkSync", unlink_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let rename_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |old_path: String, new_path: String| -> rquickjs::Result<()> {
                    proxy
                        .rename(&old_path, &new_path)
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_old_path: String, _new_path: String| -> rquickjs::Result<()> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("renameSync", rename_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        let append_fn = if let Some(ref proxy) = self.fs_proxy {
            let proxy = Arc::clone(proxy);
            Function::new(
                ctx.clone(),
                move |path: String, data: String| -> rquickjs::Result<()> {
                    proxy
                        .append_file(&path, data.as_bytes())
                        .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &e))
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        } else {
            Function::new(
                ctx.clone(),
                move |_path: String, _data: String| -> rquickjs::Result<()> {
                    Err(fs_proxy_required_error())
                },
            )
            .map_err(|e| JsError::Runtime(e.to_string()))?
        };
        fs.set("appendFileSync", append_fn)
            .map_err(|e| JsError::Runtime(e.to_string()))?;

        globals_obj
            .set("fs", fs)
            .map_err(|e| JsError::Runtime(e.to_string()))?;
        Ok(())
    }
}
