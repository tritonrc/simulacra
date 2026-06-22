use monty::{
    ExcType, ExtFunctionResult, FunctionCall, LimitedTracker, MontyException, MontyObject,
    MontyRun, NameLookupResult, OsCall, OsFunction, PrintWriter, RunProgress,
};

use crate::error::PythonError;
use crate::runtime::PythonOutput;

/// Trait for handling external operations during Python execution.
///
/// Implementations dispatch to AgentCell or test fakes.
/// All methods are synchronous because Monty's pause/resume is synchronous.
pub trait ExternalDispatcher {
    /// Read file contents as text.
    fn read_file(&self, path: &str) -> Result<String, String>;
    /// Write text to a file.
    fn write_file(&self, path: &str, content: &str) -> Result<(), String>;
    /// List directory entries.
    fn list_dir(&self, path: &str) -> Result<Vec<String>, String>;
    /// Check whether a path exists through mediated file/directory operations.
    fn path_exists(&self, path: &str) -> Result<bool, String> {
        match self.read_file(path) {
            Ok(_) => Ok(true),
            Err(read_err) if is_permission_error(&read_err) => Err(read_err),
            Err(_) => match self.list_dir(path) {
                Ok(_) => Ok(true),
                Err(list_err) if is_permission_error(&list_err) => Err(list_err),
                Err(_) => Ok(false),
            },
        }
    }
    /// Check whether a path is a file through the mediated read operation.
    fn path_is_file(&self, path: &str) -> Result<bool, String> {
        match self.read_file(path) {
            Ok(_) => Ok(true),
            Err(err) if is_permission_error(&err) => Err(err),
            Err(_) => Ok(false),
        }
    }
    /// Check whether a path is a directory through the mediated list operation.
    fn path_is_dir(&self, path: &str) -> Result<bool, String> {
        match self.list_dir(path) {
            Ok(_) => Ok(true),
            Err(err) if is_permission_error(&err) => Err(err),
            Err(_) => Ok(false),
        }
    }
    /// HTTP GET request.
    fn http_get(&self, url: &str) -> Result<String, String>;
    /// HTTP POST request.
    fn http_post(&self, url: &str, body: &str) -> Result<String, String>;
    /// Read environment variable.
    fn env_get(&self, name: &str) -> Result<Option<String>, String>;
}

/// Known external function names that we register via NameLookup.
const EXTERNAL_FUNCTIONS: &[&str] = &[
    "read_file",
    "write_file",
    "list_dir",
    "http_get",
    "http_post",
    "env",
];

/// Execute Python code with external function dispatch.
///
/// Uses MontyRun::start() for pause/resume execution. Handles:
/// - RunProgress::OsCall -- Monty's native OS operations (Path.read_text, os.getenv, etc.)
/// - RunProgress::FunctionCall -- user-registered external functions (http_get, http_post)
/// - RunProgress::NameLookup -- resolve external function names
/// - RunProgress::Complete -- execution finished
pub fn execute_with_dispatch(
    code: &str,
    tracker: LimitedTracker,
    dispatcher: &dyn ExternalDispatcher,
) -> Result<PythonOutput, PythonError> {
    let runner = MontyRun::new(code.to_owned(), "<py_exec>", vec![])
        .map_err(|e| PythonError::ParseError(crate::error::format_exception(&e)))?;

    let mut stdout = String::new();
    let mut print = PrintWriter::Collect(&mut stdout);

    let mut progress = runner
        .start(vec![], tracker, print.reborrow())
        .map_err(PythonError::from)?;

    loop {
        match progress {
            RunProgress::Complete(result) => {
                return Ok(PythonOutput {
                    stdout,
                    result: Some(result),
                });
            }

            RunProgress::OsCall(call) => {
                let result = handle_os_call(&call, dispatcher);
                progress = call
                    .resume(result, print.reborrow())
                    .map_err(PythonError::from)?;
            }

            RunProgress::FunctionCall(call) => {
                let result = handle_function_call(&call, dispatcher);
                progress = call
                    .resume(result, print.reborrow())
                    .map_err(PythonError::from)?;
            }

            RunProgress::NameLookup(lookup) => {
                let name = lookup.name.clone();
                if EXTERNAL_FUNCTIONS.contains(&name.as_str()) {
                    // Return a Function sentinel so Monty can call it later.
                    // When the function is actually invoked, Monty yields FunctionCall.
                    progress = lookup
                        .resume(
                            NameLookupResult::Value(MontyObject::Function {
                                name,
                                docstring: None,
                            }),
                            print.reborrow(),
                        )
                        .map_err(PythonError::from)?;
                } else {
                    // Let Monty handle the name lookup normally (will raise NameError
                    // if undefined).
                    progress = lookup
                        .resume(NameLookupResult::Undefined, print.reborrow())
                        .map_err(PythonError::from)?;
                }
            }

            RunProgress::ResolveFutures(_) => {
                return Err(PythonError::ExecutionError(
                    "unexpected async futures in synchronous execution".into(),
                ));
            }
        }
    }
}

/// Handle Monty's native OS operations.
fn handle_os_call(
    call: &OsCall<LimitedTracker>,
    dispatcher: &dyn ExternalDispatcher,
) -> ExtFunctionResult {
    match call.function {
        OsFunction::ReadText => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.read_file(&p) {
                    Ok(content) => ExtFunctionResult::Return(MontyObject::String(content)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("read_text requires a path argument".into()),
                )),
            }
        }
        OsFunction::WriteText => {
            let path = extract_string_arg(&call.args, 0);
            let content = extract_string_arg(&call.args, 1);
            match (path, content) {
                (Some(p), Some(c)) => match dispatcher.write_file(&p, &c) {
                    Ok(()) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                _ => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("write_text requires path and content arguments".into()),
                )),
            }
        }
        OsFunction::Iterdir => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.list_dir(&p) {
                    Ok(entries) => {
                        let list = entries.into_iter().map(MontyObject::String).collect();
                        ExtFunctionResult::Return(MontyObject::List(list))
                    }
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("iterdir requires a path argument".into()),
                )),
            }
        }
        OsFunction::Getenv => {
            let name = extract_string_arg(&call.args, 0);
            match name {
                Some(n) => match dispatcher.env_get(&n) {
                    Ok(Some(val)) => ExtFunctionResult::Return(MontyObject::String(val)),
                    Ok(None) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("getenv requires a name argument".into()),
                )),
            }
        }
        OsFunction::GetEnviron => {
            // Block access to full environment
            ExtFunctionResult::Error(MontyException::new(
                ExcType::OSError,
                Some("access to os.environ is not permitted in sandbox".into()),
            ))
        }
        OsFunction::Exists => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.path_exists(&p) {
                    Ok(exists) => ExtFunctionResult::Return(MontyObject::Bool(exists)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("exists requires a path argument".into()),
                )),
            }
        }
        OsFunction::IsFile => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.path_is_file(&p) {
                    Ok(is_file) => ExtFunctionResult::Return(MontyObject::Bool(is_file)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("is_file requires a path argument".into()),
                )),
            }
        }
        OsFunction::IsDir => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.path_is_dir(&p) {
                    Ok(is_dir) => ExtFunctionResult::Return(MontyObject::Bool(is_dir)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("is_dir requires a path argument".into()),
                )),
            }
        }
        OsFunction::DateToday => {
            // Return today's date using Monty's MontyDate type
            let now = time_now();
            ExtFunctionResult::Return(MontyObject::Date(monty::MontyDate {
                year: now.0,
                month: now.1,
                day: now.2,
            }))
        }
        OsFunction::DateTimeNow => {
            let (year, month, day, hour, minute, second, microsecond) = datetime_now();
            ExtFunctionResult::Return(MontyObject::DateTime(monty::MontyDateTime {
                year,
                month,
                day,
                hour,
                minute,
                second,
                microsecond,
                offset_seconds: None,
                timezone_name: None,
            }))
        }
        _ => {
            // Unsupported OS operation
            ExtFunctionResult::Error(MontyException::new(
                ExcType::OSError,
                Some(format!(
                    "OS operation not supported in sandbox: {}",
                    call.function
                )),
            ))
        }
    }
}

/// Get the current date as (year, month, day).
fn time_now() -> (i32, u8, u8) {
    let (y, m, d, _, _, _, _) = datetime_now();
    (y, m, d)
}

/// Get the current date and time as (year, month, day, hour, minute, second, microsecond).
fn datetime_now() -> (i32, u8, u8, u8, u8, u8, u32) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let micros = dur.subsec_micros();

    // Time-of-day components
    let day_secs = (total_secs % 86400) as u32;
    let hour = (day_secs / 3600) as u8;
    let minute = ((day_secs % 3600) / 60) as u8;
    let second = (day_secs % 60) as u8;

    // Date from unix timestamp (Howard Hinnant's algorithm)
    let days = (total_secs / 86400) as i64;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m as u8, d as u8, hour, minute, second, micros)
}

/// Handle user-registered external function calls.
fn handle_function_call(
    call: &FunctionCall<LimitedTracker>,
    dispatcher: &dyn ExternalDispatcher,
) -> ExtFunctionResult {
    match call.function_name.as_str() {
        "read_file" => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.read_file(&p) {
                    Ok(content) => ExtFunctionResult::Return(MontyObject::String(content)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("read_file(path) requires a string argument".into()),
                )),
            }
        }
        "write_file" => {
            let path = extract_string_arg(&call.args, 0);
            let content = extract_string_arg(&call.args, 1);
            match (path, content) {
                (Some(p), Some(c)) => match dispatcher.write_file(&p, &c) {
                    Ok(()) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                _ => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("write_file(path, content) requires two string arguments".into()),
                )),
            }
        }
        "list_dir" => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.list_dir(&p) {
                    Ok(entries) => {
                        let list = entries.into_iter().map(MontyObject::String).collect();
                        ExtFunctionResult::Return(MontyObject::List(list))
                    }
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("list_dir(path) requires a string argument".into()),
                )),
            }
        }
        "http_get" => {
            let url = extract_string_arg(&call.args, 0);
            match url {
                Some(u) => match dispatcher.http_get(&u) {
                    Ok(body) => ExtFunctionResult::Return(MontyObject::String(body)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("http_get(url) requires a string argument".into()),
                )),
            }
        }
        "http_post" => {
            let url = extract_string_arg(&call.args, 0);
            let body = extract_string_arg(&call.args, 1);
            match (url, body) {
                (Some(u), Some(b)) => match dispatcher.http_post(&u, &b) {
                    Ok(resp) => ExtFunctionResult::Return(MontyObject::String(resp)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                _ => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("http_post(url, body) requires two string arguments".into()),
                )),
            }
        }
        "env" => {
            let name = extract_string_arg(&call.args, 0);
            match name {
                Some(n) => match dispatcher.env_get(&n) {
                    Ok(Some(val)) => ExtFunctionResult::Return(MontyObject::String(val)),
                    Ok(None) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("env(name) requires a string argument".into()),
                )),
            }
        }
        _ => ExtFunctionResult::NotFound(call.function_name.clone()),
    }
}

/// Extract a string argument from MontyObject args at the given index.
fn extract_string_arg(args: &[MontyObject], index: usize) -> Option<String> {
    args.get(index).and_then(|obj| match obj {
        MontyObject::String(s) => Some(s.clone()),
        MontyObject::Path(s) => Some(s.clone()),
        _ => None,
    })
}

/// Convert an error message to the appropriate Python exception.
///
/// NOTE: The S028 spec calls for `PermissionError` on capability denials, but
/// Monty does not yet expose `PermissionError` as a distinct `ExcType`. We use
/// `OSError` as the closest available superclass (in CPython, `PermissionError`
/// is a subclass of `OSError`). Revisit if Monty adds `ExcType::PermissionError`.
fn permission_or_runtime_error(msg: &str) -> ExtFunctionResult {
    let lower = msg.to_lowercase();
    let exc_type = if is_permission_error(&lower) {
        ExcType::OSError
    } else {
        ExcType::RuntimeError
    };
    ExtFunctionResult::Error(MontyException::new(exc_type, Some(msg.to_string())))
}

fn is_permission_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("capability denied") || lower.contains("permission")
}
