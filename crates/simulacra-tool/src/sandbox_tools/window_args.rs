//! Argument parsing for `file_read`'s byte window.

use serde_json::Value;
use simulacra_types::ToolError;

/// A byte-window bound: absent or null means unset; anything else must be an
/// integer of at least `minimum`.
pub(super) fn optional_window(
    args: &Value,
    key: &str,
    minimum: u64,
) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(n) if n >= minimum => Ok(Some(n)),
            _ => Err(ToolError::InvalidArguments(format!(
                "{key} must be an integer of at least {minimum}"
            ))),
        },
    }
}
