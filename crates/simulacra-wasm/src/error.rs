#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("module load failed: {0}")]
    ModuleLoadFailed(String),
    #[error("instantiation failed: {0}")]
    InstantiationFailed(String),
    #[error("fuel exhausted after {consumed} units")]
    FuelExhausted { consumed: u64 },
    #[error("tool error: {0}")]
    ToolError(String),
    #[error("wasm trap: {0}")]
    Trap(String),
}
