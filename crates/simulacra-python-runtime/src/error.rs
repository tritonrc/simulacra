use monty::MontyException;

/// Errors from Python execution.
#[derive(Debug, thiserror::Error)]
pub enum PythonError {
    #[error("parse error: {0}")]
    ParseError(String),

    #[error("execution error: {0}")]
    ExecutionError(String),

    #[error("resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    #[error("external function error: {0}")]
    ExternalFunctionError(String),
}

impl From<MontyException> for PythonError {
    fn from(exc: MontyException) -> Self {
        let msg = format_exception(&exc);
        match exc.exc_type() {
            monty::ExcType::MemoryError => Self::ResourceLimitExceeded(msg),
            monty::ExcType::TimeoutError => Self::ResourceLimitExceeded(msg),
            monty::ExcType::RecursionError => Self::ResourceLimitExceeded(msg),
            _ => Self::ExecutionError(msg),
        }
    }
}

/// Format a MontyException into a human-readable string with traceback.
pub fn format_exception(exc: &MontyException) -> String {
    let mut parts = Vec::new();
    for frame in exc.traceback() {
        parts.push(format!(
            "  File \"{}\", line {}",
            frame.filename, frame.start.line
        ));
    }
    let exc_type = exc.exc_type();
    match exc.message() {
        Some(msg) => parts.push(format!("{exc_type}: {msg}")),
        None => parts.push(format!("{exc_type}")),
    }
    parts.join("\n")
}
