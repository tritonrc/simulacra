use std::collections::HashMap;
use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use tracing::warn;

use crate::HookModule;
use crate::error::HookError;
use crate::verdict::{Operation, Phase, Verdict};

/// Lazily-initialized OTel meter instruments for the hook pipeline.
struct HookMeters {
    invocations: Counter<u64>,
    denials: Counter<u64>,
    timeouts: Counter<u64>,
}

impl HookMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<HookMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-hooks");
            HookMeters {
                invocations: meter
                    .u64_counter("simulacra.hooks.invocations")
                    .with_description("Total hook invocations")
                    .build(),
                denials: meter
                    .u64_counter("simulacra.hooks.denials")
                    .with_description("Total hook denials")
                    .build(),
                timeouts: meter
                    .u64_counter("simulacra.hooks.timeouts")
                    .with_description("Total hook timeouts")
                    .build(),
            }
        })
    }
}

/// An ordered chain of hooks for a single operation type.
pub struct HookChain {
    hooks: Vec<Arc<dyn HookModule>>,
}

impl HookChain {
    fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    fn add(&mut self, hook: Arc<dyn HookModule>) {
        self.hooks.push(hook);
    }
}

/// Routes operations to their hook chains and executes them.
pub struct HookPipeline {
    chains: HashMap<Operation, HookChain>,
}

impl HookPipeline {
    /// Create an empty pipeline with no hooks.
    pub fn new() -> Self {
        Self {
            chains: HashMap::new(),
        }
    }

    /// Add a hook to the chain for the given operation.
    pub fn add(&mut self, operation: Operation, hook: Arc<dyn HookModule>) {
        self.chains
            .entry(operation)
            .or_insert_with(HookChain::new)
            .add(hook);
    }

    /// Return hook names registered for the given operation, in execution order.
    pub fn hook_names(&self, operation: Operation) -> Vec<String> {
        self.chains
            .get(&operation)
            .map(|c| c.hooks.iter().map(|h| h.name().to_string()).collect())
            .unwrap_or_default()
    }

    /// Run before-phase hooks in config order (first-deny-wins).
    ///
    /// Returns the final verdict and the (possibly modified) context.
    /// Modifications chain: each hook sees the output of the previous hook.
    pub fn run_before(
        &self,
        operation: Operation,
        context: &str,
    ) -> Result<(Verdict, String), HookError> {
        let meters = HookMeters::get();
        let chain = match self.chains.get(&operation) {
            Some(c) => c,
            None => return Ok((Verdict::continue_unchanged(), context.to_string())),
        };

        let mut current_context = context.to_string();

        for hook in &chain.hooks {
            meters.invocations.add(
                1,
                &[
                    KeyValue::new("hook", hook.name().to_string()),
                    KeyValue::new("phase", "before"),
                    KeyValue::new("operation", operation.to_string()),
                ],
            );

            match hook.invoke(Phase::Before, operation, &current_context) {
                Ok(Verdict::Continue(None)) => {
                    // No modification, keep going
                }
                Ok(Verdict::Continue(Some(modified))) => {
                    current_context = modified;
                }
                Ok(Verdict::Deny(reason)) => {
                    meters.denials.add(
                        1,
                        &[
                            KeyValue::new("hook", hook.name().to_string()),
                            KeyValue::new("phase", "before"),
                            KeyValue::new("operation", operation.to_string()),
                        ],
                    );
                    return Ok((Verdict::Deny(reason), current_context));
                }
                Ok(Verdict::Kill(reason)) => {
                    return Err(HookError::Killed {
                        hook: hook.name().to_string(),
                        reason,
                    });
                }
                Err(HookError::Timeout { hook, timeout_ms }) => {
                    meters.timeouts.add(
                        1,
                        &[
                            KeyValue::new("hook", hook.clone()),
                            KeyValue::new("phase", "before"),
                            KeyValue::new("operation", operation.to_string()),
                        ],
                    );
                    meters.denials.add(
                        1,
                        &[
                            KeyValue::new("hook", hook.clone()),
                            KeyValue::new("phase", "before"),
                            KeyValue::new("operation", operation.to_string()),
                        ],
                    );
                    let reason = format!("hook timeout after {timeout_ms}ms (fail closed)");
                    return Ok((Verdict::Deny(reason), current_context));
                }
                Err(e) => return Err(e),
            }
        }

        Ok((Verdict::continue_unchanged(), current_context))
    }

    /// Run after-phase hooks in reverse order (onion model).
    ///
    /// Deny in after-phase is logged as a warning but treated as Continue.
    /// Kill in after-phase is still enforced.
    pub fn run_after(
        &self,
        operation: Operation,
        context: &str,
    ) -> Result<(Verdict, String), HookError> {
        let meters = HookMeters::get();
        let chain = match self.chains.get(&operation) {
            Some(c) => c,
            None => return Ok((Verdict::continue_unchanged(), context.to_string())),
        };

        let mut current_context = context.to_string();

        for hook in chain.hooks.iter().rev() {
            meters.invocations.add(
                1,
                &[
                    KeyValue::new("hook", hook.name().to_string()),
                    KeyValue::new("phase", "after"),
                    KeyValue::new("operation", operation.to_string()),
                ],
            );

            match hook.invoke(Phase::After, operation, &current_context) {
                Ok(Verdict::Continue(None)) => {
                    // No modification, keep going
                }
                Ok(Verdict::Continue(Some(modified))) => {
                    current_context = modified;
                }
                Ok(Verdict::Deny(reason)) => {
                    meters.denials.add(
                        1,
                        &[
                            KeyValue::new("hook", hook.name().to_string()),
                            KeyValue::new("phase", "after"),
                            KeyValue::new("operation", operation.to_string()),
                        ],
                    );
                    warn!(
                        hook = hook.name(),
                        reason = reason,
                        "Deny verdict in after-phase is not enforced, treating as Continue"
                    );
                    // Deny in after-phase is downgraded to Continue
                }
                Ok(Verdict::Kill(reason)) => {
                    return Err(HookError::Killed {
                        hook: hook.name().to_string(),
                        reason,
                    });
                }
                Err(HookError::Timeout { hook, timeout_ms }) => {
                    meters.timeouts.add(
                        1,
                        &[
                            KeyValue::new("hook", hook.clone()),
                            KeyValue::new("phase", "after"),
                            KeyValue::new("operation", operation.to_string()),
                        ],
                    );
                    // After-phase timeout: log warning but treat as Continue (fail open)
                    // since the operation already executed.
                    warn!(
                        hook = hook,
                        timeout_ms = timeout_ms,
                        "Timeout in after-phase, treating as Continue"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        Ok((Verdict::continue_unchanged(), current_context))
    }
}

impl Default for HookPipeline {
    fn default() -> Self {
        Self::new()
    }
}
