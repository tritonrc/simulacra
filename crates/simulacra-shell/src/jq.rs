use serde_json::Value;
use simulacra_types::VirtualFs;

use crate::CommandResult;
use crate::builtins::{resolve_path, shell_read_file};

pub(crate) fn builtin_jq(
    args: &[String],
    stdin: &str,
    vfs: &dyn VirtualFs,
    cwd: &str,
) -> CommandResult {
    let query = match JqQuery::parse(args) {
        Ok(query) => query,
        Err(message) => return CommandResult::error(2, message),
    };
    let filter = match Filter::parse(&query.filter) {
        Ok(filter) => filter,
        Err(message) => return CommandResult::error(3, message),
    };

    let inputs = match read_inputs(&query.files, stdin, vfs, cwd) {
        Ok(inputs) => inputs,
        Err(message) => return CommandResult::error(1, message),
    };

    let mut out = String::new();
    for input in inputs {
        let value: Value = match serde_json::from_str(&input) {
            Ok(value) => value,
            Err(err) => {
                return CommandResult::error(4, format!("jq: invalid JSON input: {err}\n"));
            }
        };

        let values = match filter.apply(&value) {
            Ok(values) => values,
            Err(message) => return CommandResult::error(5, message),
        };
        for value in values {
            render_value(&value, query.raw_output, &mut out);
        }
    }

    CommandResult::success(out)
}

#[derive(Debug, PartialEq, Eq)]
struct JqQuery {
    raw_output: bool,
    filter: String,
    files: Vec<String>,
}

impl JqQuery {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut raw_output = false;
        let mut filter = None;
        let mut files = Vec::new();
        let mut parse_flags = true;

        for arg in args {
            if parse_flags && arg == "--" {
                parse_flags = false;
                continue;
            }
            if parse_flags && matches!(arg.as_str(), "-r" | "--raw-output") {
                raw_output = true;
                continue;
            }
            if parse_flags && arg.starts_with('-') {
                return Err(format!("jq: unsupported option: {arg}\n"));
            }
            if filter.is_none() {
                filter = Some(arg.clone());
            } else {
                files.push(arg.clone());
            }
        }

        let Some(filter) = filter else {
            return Err("jq: missing filter\n".to_string());
        };

        Ok(Self {
            raw_output,
            filter,
            files,
        })
    }
}

fn read_inputs(
    files: &[String],
    stdin: &str,
    vfs: &dyn VirtualFs,
    cwd: &str,
) -> Result<Vec<String>, String> {
    if files.is_empty() {
        return Ok(vec![stdin.to_string()]);
    }

    let mut inputs = Vec::with_capacity(files.len());
    for file in files {
        let path = resolve_path(file, cwd);
        let bytes = shell_read_file(vfs, &path).map_err(|err| format!("jq: {file}: {err}\n"))?;
        let input = String::from_utf8(bytes)
            .map_err(|err| format!("jq: {file}: invalid UTF-8 input: {err}\n"))?;
        inputs.push(input);
    }
    Ok(inputs)
}

#[derive(Debug, PartialEq, Eq)]
struct Filter {
    steps: Vec<FilterStep>,
}

#[derive(Debug, PartialEq, Eq)]
enum FilterStep {
    Identity,
    FieldPath(Vec<String>),
    KeysIter,
}

impl Filter {
    fn parse(raw: &str) -> Result<Self, String> {
        let mut steps = Vec::new();
        for part in raw.split('|') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("jq: unsupported filter: {raw}\n"));
            }
            let step = match part {
                "." => FilterStep::Identity,
                "keys[]" => FilterStep::KeysIter,
                _ if part.starts_with('.') => FilterStep::FieldPath(parse_field_path(part, raw)?),
                _ => return Err(format!("jq: unsupported filter: {raw}\n")),
            };
            steps.push(step);
        }
        Ok(Self { steps })
    }

    fn apply(&self, input: &Value) -> Result<Vec<Value>, String> {
        let mut values = vec![input.clone()];
        for step in &self.steps {
            let mut next = Vec::new();
            for value in &values {
                match step {
                    FilterStep::Identity => next.push(value.clone()),
                    FilterStep::FieldPath(path) => next.push(select_path(value, path)),
                    FilterStep::KeysIter => next.extend(keys(value)?),
                }
            }
            values = next;
        }
        Ok(values)
    }
}

fn parse_field_path(part: &str, raw: &str) -> Result<Vec<String>, String> {
    if part == "." {
        return Ok(Vec::new());
    }
    let Some(path) = part.strip_prefix('.') else {
        return Err(format!("jq: unsupported filter: {raw}\n"));
    };
    if path.is_empty() || path.contains("..") {
        return Err(format!("jq: unsupported filter: {raw}\n"));
    }

    let mut fields = Vec::new();
    for field in path.split('.') {
        // Keep this fidelity subset to common bare jq identifiers.
        if field.is_empty()
            || !field
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(format!("jq: unsupported filter: {raw}\n"));
        }
        fields.push(field.to_string());
    }
    Ok(fields)
}

fn select_path(value: &Value, path: &[String]) -> Value {
    let mut current = value;
    for field in path {
        let Some(next) = current.get(field) else {
            return Value::Null;
        };
        current = next;
    }
    current.clone()
}

fn keys(value: &Value) -> Result<Vec<Value>, String> {
    match value {
        // serde_json's default Map ordering is sorted, matching jq `keys[]`.
        Value::Object(map) => Ok(map.keys().cloned().map(Value::String).collect()),
        Value::Array(items) => Ok((0..items.len())
            .map(|index| Value::Number(index.into()))
            .collect()),
        _ => Err("jq: keys[] requires an object or array\n".to_string()),
    }
}

fn render_value(value: &Value, raw_output: bool, out: &mut String) {
    if raw_output && let Value::String(value) = value {
        out.push_str(value);
        out.push('\n');
        return;
    }

    let rendered = if raw_output {
        value.to_string()
    } else {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    };
    out.push_str(&rendered);
    out.push('\n');
}
