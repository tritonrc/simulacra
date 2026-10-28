use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rquickjs::function::Async;
use rquickjs::{Ctx, Function};

use crate::JsError;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Default cap on the JSON arguments of one `tools.x(...)` call.
const DEFAULT_HOST_ARG_CAP: usize = 64 * 1024;

/// Named async functions an embedder offers to scripts as `globalThis.tools`.
pub trait JsHost: Send + Sync {
    fn names(&self) -> Vec<String>;
    /// `Ok` is the JSON result; `Err` rejects the script's promise with that message.
    fn call(&self, name: String, args_json: String) -> BoxFuture<'static, Result<String, String>>;
}

/// Per-evaluation settings an embedder layers over [`JsRuntime`](crate::JsRuntime).
#[derive(Clone, Default)]
pub struct JsRuntimeOptions {
    pub host: Option<Arc<dyn JsHost>>,
    /// Replaces the runtime's wall-clock budget.
    pub timeout: Option<Duration>,
    /// QuickJS heap cap in bytes.
    pub memory_limit: Option<usize>,
    /// Cap on captured console bytes, shared by every console method.
    pub console_cap: Option<usize>,
    /// Cap on one `tools` call's JSON arguments. Default 64 KiB.
    pub host_arg_cap: Option<usize>,
}

impl JsRuntimeOptions {
    pub(super) fn host_arg_cap(&self) -> usize {
        self.host_arg_cap.unwrap_or(DEFAULT_HOST_ARG_CAP)
    }
}

/// One host function carries every call; its string result is `o` + JSON or
/// `e` + message, so a host error becomes a catchable `Error`. The raw function
/// is captured by the factory below and never reachable as a global. The length
/// check runs on the JS string, before any copy into Rust.
const TOOLS_FACTORY: &str = r#"(function (call, names, cap) {
    const tools = {};
    for (const name of names) {
        tools[name] = async function (args) {
            const json = JSON.stringify(args ?? {}) ?? "null";
            if (json.length > cap) {
                throw new Error(`tools.${name}: arguments exceed ${cap} bytes`);
            }
            const reply = await call(name, json);
            if (reply[0] === "e") throw new Error(reply.slice(1));
            return JSON.parse(reply.slice(1));
        };
    }
    const missing = (name) => async function () {
        throw new Error(`tools.${name} does not exist; available: ${names.join(", ")}`);
    };
    // `then` stays undefined so awaiting `tools` itself is not a thenable trap.
    return new Proxy(tools, {
        get: (target, key) =>
            typeof key !== "string" || key in target || key === "then"
                ? target[key]
                : missing(key),
    });
})"#;

pub(super) fn install_tools(
    ctx: &Ctx<'_>,
    host: Arc<dyn JsHost>,
    arg_cap: usize,
) -> Result<(), JsError> {
    let err = |e: rquickjs::Error| JsError::Runtime(e.to_string());
    let names = host.names();
    let call = Function::new(
        ctx.clone(),
        Async(move |name: String, args_json: String| {
            // UTF-16 length passed the JS check; this enforces the byte cap.
            let pending = (args_json.len() <= arg_cap).then(|| host.call(name, args_json));
            async move {
                match pending {
                    Some(call) => match call.await {
                        Ok(json) => format!("o{json}"),
                        Err(message) => format!("e{message}"),
                    },
                    None => format!("earguments exceed {arg_cap} bytes"),
                }
            }
        }),
    )
    .map_err(err)?;
    let factory: Function<'_> = ctx.eval(TOOLS_FACTORY).map_err(err)?;
    let tools: rquickjs::Value<'_> = factory.call((call, names, arg_cap)).map_err(err)?;
    ctx.globals().set("tools", tools).map_err(err)
}
