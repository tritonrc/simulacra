use simulacra_types::VirtualFs;

use crate::CommandResult;
use crate::http_proxy::{ShellHttpError, ShellHttpProxy};

const CURL_SUPPORTED_FLAGS: &str = "-X, --request, -H, --header, -d, --data, --data-raw, --json, -o, --output, \
     -s, --silent, -i, --include, -f, --fail, -v, --verbose, -L, --location, --connect-timeout";

pub(crate) fn builtin_curl(
    args: &[String],
    vfs: &dyn VirtualFs,
    http_proxy: Option<&dyn ShellHttpProxy>,
    cwd: &str,
) -> CommandResult {
    let proxy = match http_proxy {
        Some(p) => p,
        None => {
            return CommandResult::error(
                1,
                "curl: network commands require HTTP proxy (not available in this context)\n",
            );
        }
    };

    let mut method: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut body: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut silent = false;
    let mut include = false;
    let mut fail_on_error = false;
    let mut verbose = false;
    let mut timeout_ms: Option<u64> = None;
    let mut url: Option<String> = None;
    let mut body_implies_post = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-X" | "--request" => {
                i += 1;
                if i >= args.len() {
                    return CommandResult::error(1, "curl: option -X requires a value\n");
                }
                method = Some(args[i].clone());
            }
            "-H" | "--header" => {
                i += 1;
                if i >= args.len() {
                    return CommandResult::error(1, "curl: option -H requires a value\n");
                }
                if let Some((name, value)) = args[i].split_once(':') {
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
            }
            "-d" | "--data" | "--data-raw" => {
                i += 1;
                if i >= args.len() {
                    return CommandResult::error(1, "curl: option -d requires a value\n");
                }
                body = Some(args[i].clone());
                body_implies_post = true;
            }
            "--json" => {
                i += 1;
                if i >= args.len() {
                    return CommandResult::error(1, "curl: option --json requires a value\n");
                }
                body = Some(args[i].clone());
                body_implies_post = true;
                headers.push(("Content-Type".to_string(), "application/json".to_string()));
                headers.push(("Accept".to_string(), "application/json".to_string()));
            }
            "-o" | "--output" => {
                i += 1;
                if i >= args.len() {
                    return CommandResult::error(1, "curl: option -o requires a value\n");
                }
                output_file = Some(args[i].clone());
            }
            "-s" | "--silent" => {
                silent = true;
            }
            "-i" | "--include" => {
                include = true;
            }
            "-f" | "--fail" => {
                fail_on_error = true;
            }
            "-v" | "--verbose" => {
                verbose = true;
            }
            "-L" | "--location" => {}
            "--connect-timeout" => {
                i += 1;
                if i >= args.len() {
                    return CommandResult::error(
                        1,
                        "curl: option --connect-timeout requires a value\n",
                    );
                }
                if let Ok(secs) = args[i].parse::<f64>() {
                    timeout_ms = Some((secs * 1000.0) as u64);
                }
            }
            other => {
                if other.starts_with('-') {
                    return CommandResult::error(
                        1,
                        format!(
                            "curl: unsupported option '{}'. Supported: {}\n",
                            other, CURL_SUPPORTED_FLAGS,
                        ),
                    );
                }
                url = Some(other.to_string());
            }
        }
        i += 1;
    }

    let url = match url {
        Some(u) => u,
        None => return CommandResult::error(1, "curl: no URL specified\n"),
    };

    let method = method.unwrap_or_else(|| {
        if body_implies_post {
            "POST".to_string()
        } else {
            "GET".to_string()
        }
    });

    let body_bytes = body.as_deref().map(|b| b.as_bytes());
    let mut stderr_out = String::new();

    if verbose {
        let (host, path) = parse_url_parts(&url);
        stderr_out.push_str(&format!("> {} {} HTTP/1.1\n", method, path));
        stderr_out.push_str(&format!("> Host: {}\n", host));
        for (name, value) in &headers {
            stderr_out.push_str(&format!("> {}: {}\n", name, value));
        }
    }

    let response = match proxy.execute(&url, &method, &headers, body_bytes, timeout_ms) {
        Ok(resp) => resp,
        Err(e) => {
            let msg = match e {
                ShellHttpError::CapabilityDenied(msg) => {
                    format!("curl: capability denied: {msg}\n")
                }
                ShellHttpError::BudgetExhausted(msg) => {
                    format!("curl: budget exhausted: {msg}\n")
                }
                ShellHttpError::NetworkError(msg) => format!("curl: network error: {msg}\n"),
                ShellHttpError::Timeout => "curl: operation timed out\n".to_string(),
            };
            return CommandResult::error(1, msg);
        }
    };

    let status = response.status;
    let status_text = &response.status_text;
    let is_error = status >= 400;

    if verbose {
        stderr_out.push_str(&format!("< HTTP/1.1 {} {}\n", status, status_text));
        for (name, value) in &response.headers {
            stderr_out.push_str(&format!("< {}: {}\n", name, value));
        }
    }

    if fail_on_error && is_error {
        stderr_out.push_str(&format!(
            "curl: (22) The requested URL returned error: {} {}\n",
            status, status_text
        ));
        return CommandResult {
            stdout: String::new(),
            stderr: stderr_out,
            exit_code: 1,
        };
    }

    let body_str = String::from_utf8_lossy(&response.body).to_string();
    let mut stdout_out = String::new();

    if include {
        stdout_out.push_str(&format!("HTTP/1.1 {} {}\r\n", status, status_text));
        for (name, value) in &response.headers {
            stdout_out.push_str(&format!("{}: {}\r\n", name, value));
        }
        stdout_out.push_str("\r\n");
    }

    if let Some(ref path) = output_file {
        let path = resolve_path(path, cwd);
        match vfs.write(&path, &response.body) {
            Ok(()) => {
                if !silent {
                    let len = response.body.len();
                    stderr_out.push_str(&format!(
                        "  % Total    % Received\n  {}    {}  100%\n",
                        len, len
                    ));
                }
            }
            Err(e) => {
                stderr_out.push_str(&format!("curl: {}: {}\n", path, e));
                return CommandResult {
                    stdout: String::new(),
                    stderr: stderr_out,
                    exit_code: 1,
                };
            }
        }
    } else {
        stdout_out.push_str(&body_str);
    }

    CommandResult {
        stdout: stdout_out,
        stderr: stderr_out,
        exit_code: 0,
    }
}

fn parse_url_parts(url: &str) -> (String, String) {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    if let Some(slash_pos) = without_scheme.find('/') {
        let host = &without_scheme[..slash_pos];
        let path = &without_scheme[slash_pos..];
        (host.to_string(), path.to_string())
    } else {
        (without_scheme.to_string(), "/".to_string())
    }
}

const WGET_SUPPORTED_FLAGS: &str = "-O, --output-document, -q, --quiet, --header, --post-data, --method, --timeout, --no-check-certificate";

pub(crate) fn builtin_wget(
    args: &[String],
    vfs: &dyn VirtualFs,
    http_proxy: Option<&dyn ShellHttpProxy>,
    cwd: &str,
) -> CommandResult {
    let proxy = match http_proxy {
        Some(p) => p,
        None => {
            return CommandResult::error(
                1,
                "wget: network commands require HTTP proxy (not available in this context)\n",
            );
        }
    };

    let mut method: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut body: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut quiet = false;
    let mut timeout_ms: Option<u64> = None;
    let mut url: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if let Some(val) = arg.strip_prefix("--output-document=") {
            output_file = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--header=") {
            if let Some((name, value)) = val.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--post-data=") {
            body = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--method=") {
            method = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--timeout=") {
            if let Ok(secs) = val.parse::<f64>() {
                timeout_ms = Some((secs * 1000.0) as u64);
            }
            i += 1;
            continue;
        }

        match arg.as_str() {
            "-O" => {
                i += 1;
                if i >= args.len() {
                    return CommandResult::error(1, "wget: option -O requires a value\n");
                }
                output_file = Some(args[i].clone());
            }
            "-q" | "--quiet" => {
                quiet = true;
            }
            "--no-check-certificate" => {}
            other => {
                if other.starts_with('-') {
                    return CommandResult::error(
                        1,
                        format!(
                            "wget: unsupported option '{}'. Supported: {}\n",
                            other, WGET_SUPPORTED_FLAGS,
                        ),
                    );
                }
                url = Some(other.to_string());
            }
        }
        i += 1;
    }

    let url = match url {
        Some(u) => u,
        None => return CommandResult::error(1, "wget: no URL specified\n"),
    };

    let method = method.unwrap_or_else(|| {
        if body.is_some() {
            "POST".to_string()
        } else {
            "GET".to_string()
        }
    });

    let stdout_mode = output_file.as_deref() == Some("-");
    let save_filename = if stdout_mode {
        None
    } else if let Some(ref path) = output_file {
        Some(path.clone())
    } else {
        Some(derive_filename_from_url(&url))
    };

    let (host, _) = parse_url_parts(&url);
    let mut stderr_out = String::new();
    if !quiet {
        stderr_out.push_str(&format!("Resolving {}... connecting.\n", host));
    }

    let body_bytes = body.as_deref().map(|b| b.as_bytes());

    let response = match proxy.execute(&url, &method, &headers, body_bytes, timeout_ms) {
        Ok(resp) => resp,
        Err(e) => {
            let msg = match e {
                ShellHttpError::CapabilityDenied(msg) => {
                    format!("wget: capability denied: {msg}\n")
                }
                ShellHttpError::BudgetExhausted(msg) => {
                    format!("wget: budget exhausted: {msg}\n")
                }
                ShellHttpError::NetworkError(msg) => format!("wget: network error: {msg}\n"),
                ShellHttpError::Timeout => "wget: operation timed out\n".to_string(),
            };
            return CommandResult::error(1, msg);
        }
    };

    let body_len = response.body.len();
    let content_type = response
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("application/octet-stream");

    if !quiet {
        stderr_out.push_str(&format!(
            "HTTP request sent, awaiting response... {} {}\n",
            response.status, response.status_text
        ));
        stderr_out.push_str(&format!("Length: {} [{}]\n", body_len, content_type));
    }

    let mut stdout_out = String::new();

    if stdout_mode {
        stdout_out.push_str(&String::from_utf8_lossy(&response.body));
    } else if let Some(ref filename) = save_filename {
        let resolved_filename = resolve_path(filename, cwd);
        if !quiet {
            stderr_out.push_str(&format!("Saving to: '{}'\n", filename));
        }

        match vfs.write(&resolved_filename, &response.body) {
            Ok(()) => {
                if !quiet {
                    stderr_out.push_str(&format!("\n'{}' saved [{}]\n", filename, body_len));
                }
            }
            Err(e) => {
                stderr_out.push_str(&format!("wget: {}: {}\n", filename, e));
                return CommandResult {
                    stdout: String::new(),
                    stderr: stderr_out,
                    exit_code: 1,
                };
            }
        }
    }

    CommandResult {
        stdout: stdout_out,
        stderr: stderr_out,
        exit_code: 0,
    }
}

fn derive_filename_from_url(url: &str) -> String {
    let (_, path) = parse_url_parts(url);
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "index.html".to_string();
    }
    match trimmed.rsplit('/').next() {
        Some(segment) if !segment.is_empty() => segment.to_string(),
        _ => "index.html".to_string(),
    }
}

fn resolve_path(path: &str, cwd: &str) -> String {
    crate::executor::resolve_against_cwd(path, cwd)
}
