use std::path::Path;
use std::time::Instant;

use rquickjs::{Context, Runtime, Value};

use crate::HookModule;
use crate::error::HookError;
use crate::verdict::{Operation, Phase, Verdict};

/// A hook module backed by a JavaScript file evaluated via rquickjs.
///
/// Each invocation creates a fresh runtime and context, so no state
/// leaks between calls.
pub struct JsHookModule {
    hook_name: String,
    script: String,
    timeout_ms: u64,
}

impl JsHookModule {
    /// Create a JS hook by reading a file.
    pub fn from_file(
        name: impl Into<String>,
        path: impl AsRef<Path>,
        timeout_ms: u64,
    ) -> Result<Self, HookError> {
        let name = name.into();
        let script =
            std::fs::read_to_string(path.as_ref()).map_err(|e| HookError::ExecutionError {
                hook: name.clone(),
                message: format!("failed to read hook file: {e}"),
            })?;
        Ok(Self {
            hook_name: name,
            script,
            timeout_ms,
        })
    }

    /// Create a JS hook from an inline script string.
    pub fn from_source(
        name: impl Into<String>,
        script: impl Into<String>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            hook_name: name.into(),
            script: script.into(),
            timeout_ms,
        }
    }

    /// Parse a JS object verdict into our Verdict type.
    fn parse_verdict(ctx: &rquickjs::Ctx<'_>, value: Value<'_>) -> Result<Verdict, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "hook must return an object".to_string())?;

        // Use contains_key to check for property existence before getting values,
        // since rquickjs returns Ok(undefined) for missing keys.

        let has_continue = obj
            .contains_key("continue")
            .map_err(|e| format!("failed to check continue key: {e}"))?;
        let has_deny = obj
            .contains_key("deny")
            .map_err(|e| format!("failed to check deny key: {e}"))?;
        let has_kill = obj
            .contains_key("kill")
            .map_err(|e| format!("failed to check kill key: {e}"))?;

        // Check strongest verdict first: kill > deny > continue
        if has_kill {
            let val: Value<'_> = obj
                .get("kill")
                .map_err(|e| format!("failed to read kill value: {e}"))?;
            let reason = val
                .as_string()
                .ok_or_else(|| "kill value must be a string".to_string())?
                .to_string()
                .map_err(|e| format!("failed to read kill value: {e}"))?;
            return Ok(Verdict::Kill(reason));
        }

        if has_deny {
            let val: Value<'_> = obj
                .get("deny")
                .map_err(|e| format!("failed to read deny value: {e}"))?;
            let reason = val
                .as_string()
                .ok_or_else(|| "deny value must be a string".to_string())?
                .to_string()
                .map_err(|e| format!("failed to read deny value: {e}"))?;
            return Ok(Verdict::Deny(reason));
        }

        if has_continue {
            let val: Value<'_> = obj
                .get("continue")
                .map_err(|e| format!("failed to read continue value: {e}"))?;
            if val.is_null() || val.is_undefined() {
                return Ok(Verdict::continue_unchanged());
            }
            if let Some(s) = val.as_string() {
                let s = s
                    .to_string()
                    .map_err(|e| format!("failed to read continue value: {e}"))?;
                return Ok(Verdict::Continue(Some(s)));
            }
            return Err("continue value must be null or a string".to_string());
        }

        // Suppress unused variable warning — ctx is needed for lifetime scoping
        let _ = ctx;

        Err("hook return value must have a 'continue', 'deny', or 'kill' key".to_string())
    }
}

impl HookModule for JsHookModule {
    fn name(&self) -> &str {
        &self.hook_name
    }

    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        let timeout = std::time::Duration::from_millis(self.timeout_ms);
        let deadline = Instant::now() + timeout;

        let rt = Runtime::new().map_err(|e| HookError::ExecutionError {
            hook: self.hook_name.clone(),
            message: format!("failed to create JS runtime: {e}"),
        })?;

        // Set interrupt handler for timeout enforcement.
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));

        let ctx = Context::full(&rt).map_err(|e| HookError::ExecutionError {
            hook: self.hook_name.clone(),
            message: format!("failed to create JS context: {e}"),
        })?;

        let hook_name = self.hook_name.clone();
        let phase_str = phase.to_string();
        let operation_str = operation.to_string();
        let context_owned = context.to_string();
        let script = self.script.clone();
        let timeout_ms = self.timeout_ms;

        ctx.with(|ctx| {
            // Set arguments as global variables so we avoid string escaping issues.
            let globals = ctx.globals();
            let phase_js = rquickjs::String::from_str(ctx.clone(), &phase_str).map_err(|e| {
                HookError::ExecutionError {
                    hook: hook_name.clone(),
                    message: format!("failed to create phase string: {e}"),
                }
            })?;
            globals
                .set("__hook_phase", phase_js)
                .map_err(|e| HookError::ExecutionError {
                    hook: hook_name.clone(),
                    message: format!("failed to set phase global: {e}"),
                })?;
            let operation_js =
                rquickjs::String::from_str(ctx.clone(), &operation_str).map_err(|e| {
                    HookError::ExecutionError {
                        hook: hook_name.clone(),
                        message: format!("failed to create operation string: {e}"),
                    }
                })?;
            globals.set("__hook_operation", operation_js).map_err(|e| {
                HookError::ExecutionError {
                    hook: hook_name.clone(),
                    message: format!("failed to set operation global: {e}"),
                }
            })?;
            let context_js =
                rquickjs::String::from_str(ctx.clone(), &context_owned).map_err(|e| {
                    HookError::ExecutionError {
                        hook: hook_name.clone(),
                        message: format!("failed to create context string: {e}"),
                    }
                })?;
            globals
                .set("__hook_context", context_js)
                .map_err(|e| HookError::ExecutionError {
                    hook: hook_name.clone(),
                    message: format!("failed to set context global: {e}"),
                })?;

            // Wrap the user script in an IIFE and call invoke with the globals.
            let wrapped = format!(
                r#"var __module = (function() {{
    {script}
    return {{ invoke: invoke }};
}})();
__module.invoke(__hook_phase, __hook_operation, __hook_context);
"#,
            );

            let res: rquickjs::Result<Value<'_>> = ctx.eval(wrapped);
            match res {
                Ok(value) => {
                    Self::parse_verdict(&ctx, value).map_err(|msg| HookError::ExecutionError {
                        hook: hook_name.clone(),
                        message: msg,
                    })
                }
                Err(e) => {
                    // When the interrupt handler fires, rquickjs reports it
                    // as a generic "Exception generated by QuickJS". Check if
                    // we are past the deadline to distinguish timeout from
                    // real execution errors.
                    if Instant::now() >= deadline {
                        Err(HookError::Timeout {
                            hook: hook_name.clone(),
                            timeout_ms,
                        })
                    } else {
                        Err(HookError::ExecutionError {
                            hook: hook_name.clone(),
                            message: format!("{e}"),
                        })
                    }
                }
            }
        })
    }
}
