//! WasmTool: implements `simulacra_types::Tool` with per-invocation sandboxing and fuel metering.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use wasmtime::component::Component;
use wasmtime::{Engine, Store};
use wasmtime_wasi::{
    DirPerms, FilePerms, ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

use tracing::Instrument;

use crate::Tool as WasmToolBindings;
use crate::error::WasmError;

/// Host state for per-invocation WASI sandboxes.
struct WasmState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for WasmState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Filesystem mount for a WASI tool sandbox.
#[derive(Debug, Clone)]
pub struct WasiMount {
    /// Path on the host filesystem.
    pub host: String,
    /// Path inside the WASI guest.
    pub guest: String,
    /// Permission mode: `"ro"` or `"rw"`.
    pub perms: String,
}

/// WASI sandbox configuration for a tool.
#[derive(Debug, Clone, Default)]
pub struct WasiToolConfig {
    /// Preopened directory mounts.
    pub fs: Vec<WasiMount>,
    /// Environment variable names to pass through from the host.
    /// Each entry is a variable NAME (e.g., `"GIT_TOKEN"`).
    /// The value is read from the host process environment via `std::env::var`.
    pub env: Vec<String>,
}

/// Lazily-initialized OTel meter instruments for WASM tool calls.
struct WasmMeters {
    fuel_consumed: Histogram<f64>,
    fuel_exhausted: Counter<u64>,
}

impl WasmMeters {
    fn get() -> &'static Self {
        static METERS: OnceLock<WasmMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-wasm");
            WasmMeters {
                fuel_consumed: meter
                    .f64_histogram("simulacra.wasm.fuel.consumed")
                    .with_unit("units")
                    .build(),
                fuel_exhausted: meter
                    .u64_counter("simulacra.wasm.fuel.exhausted")
                    .with_description("Number of WASM tool calls that ran out of fuel")
                    .build(),
            }
        })
    }
}

/// A WASM-based tool that implements `simulacra_types::Tool`.
///
/// Each call creates a fresh WASI sandbox with the configured
/// filesystem mounts, environment variables, and fuel limit.
pub struct WasmTool {
    engine: Engine,
    component: Component,
    tool_def: simulacra_types::ToolDefinition,
    wasi_config: WasiToolConfig,
    fuel_limit: u64,
    last_fuel: AtomicU64,
    /// Shared agent-level fuel remaining. When set, `WasmTool` checks this
    /// before each call and subtracts consumed fuel afterwards. All tools
    /// from the same agent share the same `Arc<AtomicU64>`.
    agent_fuel_remaining: Option<Arc<AtomicU64>>,
}

impl WasmTool {
    /// Create a new WasmTool from a compiled component and tool definition.
    ///
    /// - `fuel_limit`: maximum fuel per call. 0 means unlimited (`u64::MAX`).
    pub fn new(
        engine: Engine,
        component: Component,
        tool_def: simulacra_types::ToolDefinition,
        wasi_config: WasiToolConfig,
        fuel_limit: u64,
    ) -> Self {
        Self {
            engine,
            component,
            tool_def,
            wasi_config,
            fuel_limit,
            last_fuel: AtomicU64::new(0),
            agent_fuel_remaining: None,
        }
    }

    /// Set the agent-level fuel budget shared across all WASM tools for an
    /// agent.
    ///
    /// The counter is interpreted as "fuel units remaining". Pass an Arc
    /// seeded with the agent's total budget. Callers that want
    /// "unlimited" must use the constructor default (do NOT call this
    /// with `Arc::new(AtomicU64::new(0))` — a zero-valued counter means
    /// **exhausted**, and every subsequent call will fail immediately).
    pub fn set_agent_fuel(&mut self, remaining: Arc<AtomicU64>) {
        self.agent_fuel_remaining = Some(remaining);
    }

    /// Fuel consumed by the most recent call.
    pub fn last_fuel_consumed(&self) -> u64 {
        self.last_fuel.load(Ordering::Relaxed)
    }

    /// Build a WASI context from the tool's config, gated by the caller's
    /// [`simulacra_types::CapabilityToken`].
    ///
    /// Security posture (per ARCHITECTURE.md "Capabilities at the Call
    /// Site"):
    ///
    /// - **Environment variables** — only explicit `KEY=VALUE` entries
    ///   from `wasi_config.env` are passed through. Bare `NAME` entries
    ///   are **not** looked up in the host process environment. The WASM
    ///   capability model does not yet have an env-var grant, and the
    ///   safest default is to leak zero host secrets.
    /// - **Preopened directories** — every mount's `host` path must pass
    ///   `check_path_read` (for `ro` mounts) or `check_path_write` (for
    ///   `rw` mounts) against the caller's capability. Mounts that are
    ///   not authorised are dropped with a tracing warning so the
    ///   operator can spot the mismatch. If a caller has no
    ///   `paths_read`/`paths_write` grant matching the mount, the tool
    ///   starts with no preopens at all — the WASM guest simply cannot
    ///   see the host filesystem.
    fn build_wasi_ctx(
        &self,
        capability: &simulacra_types::CapabilityToken,
    ) -> Result<WasiCtx, WasmError> {
        let mut builder = WasiCtxBuilder::new();

        // Set environment variables. Only explicit KEY=VALUE entries are
        // passed. Bare NAME entries are ignored because inheriting from
        // the host process env would leak secrets across the sandbox
        // boundary without capability mediation.
        for entry in &self.wasi_config.env {
            if let Some((k, v)) = entry.split_once('=') {
                builder.env(k, v);
            } else {
                tracing::warn!(
                    tool = %self.tool_def.name,
                    env_name = %entry,
                    "ignoring bare env-var entry without KEY=VALUE; host process env is not inherited"
                );
            }
        }

        // Preopened directories, filtered by capability.
        for mount in &self.wasi_config.fs {
            let cap_check = if mount.perms == "rw" {
                capability.check_path_write(&mount.host)
            } else {
                capability.check_path_read(&mount.host)
            };
            if let Err(denied) = cap_check {
                tracing::warn!(
                    tool = %self.tool_def.name,
                    host = %mount.host,
                    guest = %mount.guest,
                    perms = %mount.perms,
                    reason = %denied.reason,
                    "capability denies preopening mount; dropping from WASI sandbox"
                );
                continue;
            }
            let (dir_perms, file_perms) = if mount.perms == "rw" {
                (DirPerms::all(), FilePerms::all())
            } else {
                (DirPerms::READ, FilePerms::READ)
            };
            let guest_name = mount.guest.trim_start_matches('/');
            builder
                .preopened_dir(&mount.host, guest_name, dir_perms, file_perms)
                .map_err(|e| {
                    WasmError::InstantiationFailed(format!(
                        "failed to preopen '{}' -> '{}': {}",
                        mount.host, mount.guest, e
                    ))
                })?;
        }

        Ok(builder.build())
    }

    /// Atomically reserve fuel from the agent-level budget and return the
    /// amount actually reserved.
    ///
    /// The returned value is `min(module_fuel, agent_remaining)` AND is
    /// subtracted from the agent counter in a single CAS step so two
    /// concurrent callers cannot race-double-spend the budget. Any unused
    /// reservation is refunded in [`refund_agent_fuel`] once the call
    /// reports its actual consumption.
    ///
    /// Callers pass `None` (not `Some(Arc::new(AtomicU64::new(0)))`) for
    /// "unlimited agent budget". If the Arc is set, `0` means exhausted.
    fn reserve_agent_fuel(&self, module_fuel: u64) -> Result<u64, WasmError> {
        match &self.agent_fuel_remaining {
            None => Ok(module_fuel),
            Some(remaining) => {
                loop {
                    let current = remaining.load(Ordering::Acquire);
                    if current == 0 {
                        return Err(WasmError::FuelExhausted { consumed: 0 });
                    }
                    let take = module_fuel.min(current);
                    let new = current - take;
                    match remaining.compare_exchange(
                        current,
                        new,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Ok(take),
                        Err(_) => {
                            // Another writer raced us; retry with the new snapshot.
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// Refund the unused portion of a previously reserved agent fuel chunk.
    fn refund_agent_fuel(&self, reserved: u64, consumed: u64) {
        if let Some(ref remaining) = self.agent_fuel_remaining {
            let refund = reserved.saturating_sub(consumed);
            if refund > 0 {
                remaining.fetch_add(refund, Ordering::AcqRel);
            }
        }
    }

    /// Execute the tool synchronously (called from async context via spawn_blocking).
    fn call_sync(
        &self,
        arguments: serde_json::Value,
        capability: &simulacra_types::CapabilityToken,
    ) -> Result<serde_json::Value, WasmError> {
        let wasi_ctx = self.build_wasi_ctx(capability)?;
        let state = WasmState {
            ctx: wasi_ctx,
            table: ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);

        let module_fuel = if self.fuel_limit == 0 {
            u64::MAX
        } else {
            self.fuel_limit
        };
        // Atomically carve our slice off the agent-level budget (if any).
        let reserved = self.reserve_agent_fuel(module_fuel)?;
        store
            .set_fuel(reserved)
            .map_err(|e| WasmError::InstantiationFailed(e.to_string()))?;

        let mut linker = wasmtime::component::Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| WasmError::InstantiationFailed(e.to_string()))?;

        let bindings = WasmToolBindings::instantiate(&mut store, &self.component, &linker)
            .map_err(|e| WasmError::InstantiationFailed(e.to_string()))?;

        let args_json = serde_json::to_string(&arguments)
            .map_err(|e| WasmError::ToolError(format!("failed to serialize arguments: {}", e)))?;

        let result = bindings.call_call_tool(&mut store, &self.tool_def.name, &args_json);

        // Calculate fuel consumed.
        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let consumed = reserved.saturating_sub(remaining_fuel);
        self.last_fuel.store(consumed, Ordering::Relaxed);

        // Refund the unused portion of the reservation so another call on
        // the same agent can use it.
        self.refund_agent_fuel(reserved, consumed);

        // Record OTel metrics.
        let meters = WasmMeters::get();
        let attrs = &[KeyValue::new("tool", self.tool_def.name.clone())];
        meters.fuel_consumed.record(consumed as f64, attrs);

        match result {
            Ok(Ok(json_str)) => {
                let value: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
                    WasmError::ToolError(format!("invalid JSON response from tool: {}", e))
                })?;
                Ok(value)
            }
            Ok(Err(wit_err)) => {
                let msg = match wit_err {
                    crate::simulacra::tools::types::ToolError::InvalidArguments(s) => {
                        format!("invalid arguments: {}", s)
                    }
                    crate::simulacra::tools::types::ToolError::ExecutionFailed(s) => {
                        format!("execution failed: {}", s)
                    }
                };
                Err(WasmError::ToolError(msg))
            }
            Err(e) => {
                // Check for fuel exhaustion via downcast to wasmtime::Trap.
                let is_out_of_fuel = e
                    .downcast_ref::<wasmtime::Trap>()
                    .is_some_and(|t| *t == wasmtime::Trap::OutOfFuel);
                if is_out_of_fuel {
                    tracing::warn!(
                        tool = %self.tool_def.name,
                        fuel_consumed = consumed,
                        "WASM tool call exhausted fuel budget"
                    );
                    meters.fuel_exhausted.add(1, attrs);
                    Err(WasmError::FuelExhausted { consumed })
                } else {
                    tracing::error!(
                        tool = %self.tool_def.name,
                        error = %e,
                        "WASM trap during tool call"
                    );
                    Err(WasmError::Trap(e.to_string()))
                }
            }
        }
    }
}

impl simulacra_types::Tool for WasmTool {
    fn definition(&self) -> simulacra_types::ToolDefinition {
        self.tool_def.clone()
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        capability: &simulacra_types::CapabilityToken,
    ) -> Pin<
        Box<dyn Future<Output = Result<serde_json::Value, simulacra_types::ToolError>> + Send + '_>,
    > {
        let span = tracing::info_span!(
            "simulacra_wasm_tool_call",
            simulacra.wasm.tool = %self.tool_def.name,
        );

        // Clone the capability up-front so it can move into the blocking
        // closure alongside the rest of the owned state. `CapabilityToken`
        // is cheap to clone and this keeps the future `'static`.
        let capability = capability.clone();

        Box::pin(
            async move {
                // WASM execution with WASI filesystem preopens calls `block_on`
                // internally, which panics inside a tokio runtime. Move execution
                // to a blocking thread to avoid the nested-runtime conflict.
                //
                // We need to move owned data into the closure since spawn_blocking
                // requires 'static. Engine and Component are Clone.
                let engine = self.engine.clone();
                let component = self.component.clone();
                let tool_def = self.tool_def.clone();
                let wasi_config = self.wasi_config.clone();
                let fuel_limit = self.fuel_limit;
                let agent_fuel = self.agent_fuel_remaining.clone();

                let result = tokio::task::spawn_blocking(move || {
                    let mut tool =
                        WasmTool::new(engine, component, tool_def, wasi_config, fuel_limit);
                    if let Some(arc) = agent_fuel {
                        tool.set_agent_fuel(arc);
                    }
                    let result = tool.call_sync(arguments, &capability);
                    (result, tool.last_fuel_consumed())
                })
                .await;

                match result {
                    Ok((call_result, consumed)) => {
                        // Store fuel consumed from the blocking task back into self.
                        self.last_fuel.store(consumed, Ordering::Relaxed);

                        tracing::info!(simulacra.wasm.fuel_consumed = consumed);

                        call_result.map_err(|e| match e {
                            WasmError::FuelExhausted { consumed } => {
                                simulacra_types::ToolError::ExecutionFailed(format!(
                                    "fuel exhausted after {} units",
                                    consumed
                                ))
                            }
                            WasmError::ToolError(msg) if msg.starts_with("invalid arguments:") => {
                                simulacra_types::ToolError::InvalidArguments(
                                    msg.strip_prefix("invalid arguments: ")
                                        .unwrap_or(&msg)
                                        .to_string(),
                                )
                            }
                            other => simulacra_types::ToolError::ExecutionFailed(other.to_string()),
                        })
                    }
                    Err(join_err) => Err(simulacra_types::ToolError::ExecutionFailed(format!(
                        "spawn_blocking failed: {}",
                        join_err
                    ))),
                }
            }
            .instrument(span),
        )
    }
}
