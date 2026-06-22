//! Built-in shell commands.

use simulacra_types::VirtualFs;

use crate::CommandResult;
use crate::http_proxy::ShellHttpProxy;

mod http;

use http::{builtin_curl, builtin_wget};

/// Execute a builtin command if the program name matches.
/// Returns `None` if the command is not a builtin.
///
/// `cwd` is the shell's current working directory; path-bearing builtins
/// resolve relative-path arguments against it.
pub(crate) fn try_builtin(
    program: &str,
    args: &[String],
    stdin: &str,
    vfs: &dyn VirtualFs,
    http_proxy: Option<&dyn ShellHttpProxy>,
    cwd: &str,
) -> Option<CommandResult> {
    match program {
        "echo" => Some(builtin_echo(args)),
        "cat" => Some(builtin_cat(args, stdin, vfs, cwd)),
        "ls" => Some(builtin_ls(args, vfs, cwd)),
        "mkdir" => Some(builtin_mkdir(args, vfs, cwd)),
        "grep" => Some(builtin_grep(args, stdin, vfs, cwd)),
        "true" => Some(CommandResult::success("")),
        "false" => Some(CommandResult::error(1, "")),
        "cp" => Some(builtin_cp(args, vfs, cwd)),
        "mv" => Some(builtin_mv(args, vfs, cwd)),
        "rm" => Some(builtin_rm(args, vfs, cwd)),
        "head" => Some(builtin_head(args, stdin, vfs, cwd)),
        "tail" => Some(builtin_tail(args, stdin, vfs, cwd)),
        "sed" => Some(builtin_sed(args, stdin)),
        "wc" => Some(builtin_wc(args, stdin, vfs, cwd)),
        "find" => Some(builtin_find(args, vfs, cwd)),
        "sort" => Some(builtin_sort(args, stdin)),
        "uniq" => Some(builtin_uniq(stdin)),
        "cut" => Some(builtin_cut(args, stdin)),
        "tr" => Some(builtin_tr(args, stdin)),
        "tee" => Some(builtin_tee(args, stdin, vfs, cwd)),
        "curl" => Some(builtin_curl(args, vfs, http_proxy, cwd)),
        "wget" => Some(builtin_wget(args, vfs, http_proxy, cwd)),
        "touch" => Some(builtin_touch(args, vfs, cwd)),
        "test" => Some(builtin_test(args, vfs, cwd)),
        "[" => Some(builtin_bracket_test(args, vfs, cwd)),
        "printf" => Some(builtin_printf(args)),
        "basename" => Some(builtin_basename(args)),
        "dirname" => Some(builtin_dirname(args)),
        _ => None,
    }
}

// ── Phase 1 builtins ─────────────────────────────────────────────────

fn builtin_echo(args: &[String]) -> CommandResult {
    let line = args.join(" ");
    CommandResult::success(format!("{line}\n"))
}

fn builtin_cat(args: &[String], stdin: &str, vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    // If no file args, echo stdin
    if args.is_empty() {
        return CommandResult::success(stdin.to_string());
    }

    let mut out = String::new();
    for path in args {
        let resolved = resolve_path(path, cwd);
        match vfs.read(&resolved) {
            Ok(data) => match String::from_utf8(data) {
                Ok(s) => out.push_str(&s),
                Err(e) => {
                    return CommandResult::error(1, format!("cat: {path}: {e}\n"));
                }
            },
            Err(e) => {
                return CommandResult::error(1, format!("cat: {path}: {e}\n"));
            }
        }
    }
    CommandResult::success(out)
}

fn builtin_ls(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    // Skip flag args (anything starting with `-`); pick the first non-flag
    // as the path. POSIX flags like `-l`, `-a`, `-la`, `-al`, `-h`, `-1`
    // are accepted as no-ops — the listing is always the directory entries.
    // The original bug: `ls -la /tmp` parsed `-la` as a path → "ls: not found: /-la".
    let path_arg: Option<&str> = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str());

    let path = match path_arg {
        Some(p) => crate::executor::resolve_against_cwd(p, cwd),
        None => cwd.to_string(),
    };

    match vfs.list_dir(&path) {
        Ok(mut entries) => {
            entries.sort();
            let out = if entries.is_empty() {
                String::new()
            } else {
                format!("{}\n", entries.join("\n"))
            };
            CommandResult::success(out)
        }
        Err(e) => CommandResult::error(1, format!("ls: {e}\n")),
    }
}

fn builtin_mkdir(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let recursive = args.iter().any(|a| a == "-p");
    let paths: Vec<&str> = args
        .iter()
        .filter(|a| *a != "-p")
        .map(|s| s.as_str())
        .collect();

    if paths.is_empty() {
        return CommandResult::error(1, "mkdir: missing operand\n".to_string());
    }

    for path in paths {
        let path = resolve_path(path, cwd);
        if recursive {
            if let Err(e) = mkdir_recursive(&path, vfs) {
                return CommandResult::error(1, format!("mkdir: {e}\n"));
            }
        } else if let Err(e) = vfs.mkdir(&path) {
            return CommandResult::error(1, format!("mkdir: {e}\n"));
        }
    }
    CommandResult::success("")
}

/// Create a directory and all missing parents.
fn mkdir_recursive(path: &str, vfs: &dyn VirtualFs) -> Result<(), String> {
    // Collect ancestor segments
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current = String::new();
    for part in &parts {
        current = format!("{current}/{part}");
        if vfs.exists(&current) {
            // Verify it's a dir
            match vfs.metadata(&current) {
                Ok(m) if m.is_dir => continue,
                Ok(_) => return Err(format!("not a directory: {current}")),
                Err(e) => return Err(e.to_string()),
            }
        }
        vfs.mkdir(&current).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Phase 2 builtins ─────────────────────────────────────────────────

fn builtin_cp(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    if args.len() < 2 {
        return CommandResult::error(1, "cp: missing operand\n".to_string());
    }
    let src = resolve_path(&args[0], cwd);
    let dst = resolve_path(&args[1], cwd);
    match vfs.read(&src) {
        Ok(data) => match vfs.write(&dst, &data) {
            Ok(()) => CommandResult::success(""),
            Err(e) => CommandResult::error(1, format!("cp: {dst}: {e}\n")),
        },
        Err(e) => CommandResult::error(1, format!("cp: {src}: {e}\n")),
    }
}

fn builtin_mv(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    if args.len() < 2 {
        return CommandResult::error(1, "mv: missing operand\n".to_string());
    }
    let src = resolve_path(&args[0], cwd);
    let dst = resolve_path(&args[1], cwd);
    match vfs.read(&src) {
        Ok(data) => {
            if let Err(e) = vfs.write(&dst, &data) {
                return CommandResult::error(1, format!("mv: {dst}: {e}\n"));
            }
            if let Err(e) = vfs.remove(&src) {
                return CommandResult::error(1, format!("mv: {src}: {e}\n"));
            }
            CommandResult::success("")
        }
        Err(e) => CommandResult::error(1, format!("mv: {src}: {e}\n")),
    }
}

fn builtin_rm(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let recursive = args.iter().any(|a| a == "-r" || a == "-rf" || a == "-fr");
    let paths: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    if paths.is_empty() {
        return CommandResult::error(1, "rm: missing operand\n".to_string());
    }

    for path in paths {
        let path = resolve_path(path, cwd);
        if !recursive {
            // Check it's a file, not a directory
            if let Ok(m) = vfs.metadata(&path)
                && m.is_dir
            {
                return CommandResult::error(1, format!("rm: {path}: is a directory\n"));
            }
        }
        if let Err(e) = vfs.remove(&path) {
            return CommandResult::error(1, format!("rm: {path}: {e}\n"));
        }
    }
    CommandResult::success("")
}

fn builtin_head(args: &[String], stdin: &str, vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let (n, file) = parse_n_and_file(args, 10);
    let input = match get_input(file.as_deref(), stdin, vfs, cwd, "head") {
        Ok(s) => s,
        Err(r) => return r,
    };
    let lines: Vec<&str> = input.lines().take(n).collect();
    if lines.is_empty() {
        CommandResult::success("")
    } else {
        CommandResult::success(format!("{}\n", lines.join("\n")))
    }
}

fn builtin_tail(args: &[String], stdin: &str, vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let (n, file) = parse_n_and_file(args, 10);
    let input = match get_input(file.as_deref(), stdin, vfs, cwd, "tail") {
        Ok(s) => s,
        Err(r) => return r,
    };
    let all_lines: Vec<&str> = input.lines().collect();
    let start = all_lines.len().saturating_sub(n);
    let lines = &all_lines[start..];
    if lines.is_empty() {
        CommandResult::success("")
    } else {
        CommandResult::success(format!("{}\n", lines.join("\n")))
    }
}

/// Parse `-n N` and optional file argument from args. Returns (count, optional_file).
fn parse_n_and_file(args: &[String], default_n: usize) -> (usize, Option<String>) {
    let mut n = default_n;
    let mut file = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-n" {
            if i + 1 < args.len() {
                n = args[i + 1].parse().unwrap_or(default_n);
                i += 2;
                continue;
            }
        } else if !args[i].starts_with('-') {
            file = Some(args[i].clone());
        }
        i += 1;
    }
    (n, file)
}

/// Get input from a file or stdin.
fn get_input(
    file: Option<&str>,
    stdin: &str,
    vfs: &dyn VirtualFs,
    cwd: &str,
    cmd: &str,
) -> Result<String, CommandResult> {
    match file {
        Some(path) => {
            let resolved = resolve_path(path, cwd);
            match vfs.read(&resolved) {
                Ok(data) => String::from_utf8(data)
                    .map_err(|e| CommandResult::error(1, format!("{cmd}: {path}: {e}\n"))),
                Err(e) => Err(CommandResult::error(1, format!("{cmd}: {path}: {e}\n"))),
            }
        }
        None => Ok(stdin.to_string()),
    }
}

fn builtin_sed(args: &[String], stdin: &str) -> CommandResult {
    if args.is_empty() {
        return CommandResult::error(1, "sed: missing expression\n".to_string());
    }
    let expr = &args[0];
    // Parse s/pat/repl/[g]
    if !expr.starts_with("s") || expr.len() < 4 {
        return CommandResult::error(1, format!("sed: unsupported expression: {expr}\n"));
    }
    let delim = expr.chars().nth(1).unwrap();
    let rest = &expr[2..];
    let parts: Vec<&str> = rest.splitn(3, delim).collect();
    if parts.len() < 2 {
        return CommandResult::error(1, format!("sed: bad substitution: {expr}\n"));
    }
    let pattern = parts[0];
    let replacement = parts[1];
    let flags = if parts.len() > 2 { parts[2] } else { "" };
    let global = flags.contains('g');

    let mut out = String::new();
    for line in stdin.lines() {
        let replaced = if global {
            line.replace(pattern, replacement)
        } else {
            line.replacen(pattern, replacement, 1)
        };
        out.push_str(&replaced);
        out.push('\n');
    }
    CommandResult::success(out)
}

fn builtin_wc(args: &[String], stdin: &str, vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let flag_l = args.iter().any(|a| a == "-l");
    let flag_w = args.iter().any(|a| a == "-w");
    let flag_c = args.iter().any(|a| a == "-c");
    let files: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    let input = if files.is_empty() {
        stdin.to_string()
    } else {
        let mut combined = String::new();
        for f in &files {
            let resolved = resolve_path(f, cwd);
            match vfs.read(&resolved) {
                Ok(data) => match String::from_utf8(data) {
                    Ok(s) => combined.push_str(&s),
                    Err(e) => return CommandResult::error(1, format!("wc: {f}: {e}\n")),
                },
                Err(e) => return CommandResult::error(1, format!("wc: {f}: {e}\n")),
            }
        }
        combined
    };

    let lines = input.lines().count();
    let words = input.split_whitespace().count();
    let chars = input.len();

    let specific = flag_l || flag_w || flag_c;
    if !specific {
        // Default: lines words chars
        CommandResult::success(format!("{lines} {words} {chars}\n"))
    } else {
        let mut parts = Vec::new();
        if flag_l {
            parts.push(lines.to_string());
        }
        if flag_w {
            parts.push(words.to_string());
        }
        if flag_c {
            parts.push(chars.to_string());
        }
        CommandResult::success(format!("{}\n", parts.join(" ")))
    }
}

fn builtin_find(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    if args.is_empty() {
        return CommandResult::error(1, "find: missing path\n".to_string());
    }
    let search_path = resolve_path(&args[0], cwd);
    let mut name_pattern: Option<&str> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "-name" && i + 1 < args.len() {
            name_pattern = Some(&args[i + 1]);
            i += 2;
        } else {
            i += 1;
        }
    }

    let mut results = Vec::new();
    find_recursive(vfs, &search_path, name_pattern, &mut results);
    results.sort();

    if results.is_empty() {
        CommandResult::success("")
    } else {
        CommandResult::success(format!("{}\n", results.join("\n")))
    }
}

fn find_recursive(
    vfs: &dyn VirtualFs,
    path: &str,
    pattern: Option<&str>,
    results: &mut Vec<String>,
) {
    // Check if path itself matches
    if let Some(pat) = pattern {
        let basename = path.rsplit('/').next().unwrap_or(path);
        if glob_match(pat, basename) {
            results.push(path.to_string());
        }
    } else {
        results.push(path.to_string());
    }

    // If directory, recurse
    if let Ok(entries) = vfs.list_dir(path) {
        for entry in entries {
            let child = if path == "/" {
                format!("/{entry}")
            } else {
                format!("{path}/{entry}")
            };
            find_recursive(vfs, &child, pattern, results);
        }
    }
}

/// Simple glob matching supporting `*` and `?`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(p: &[char], t: &[char]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            // Match zero characters or one character then continue
            glob_match_inner(&p[1..], t) || (!t.is_empty() && glob_match_inner(p, &t[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&p[1..], &t[1..]),
        (Some(pc), Some(tc)) if pc == tc => glob_match_inner(&p[1..], &t[1..]),
        _ => false,
    }
}

fn builtin_sort(_args: &[String], stdin: &str) -> CommandResult {
    let mut lines: Vec<&str> = stdin.lines().collect();
    lines.sort();
    if lines.is_empty() {
        CommandResult::success("")
    } else {
        CommandResult::success(format!("{}\n", lines.join("\n")))
    }
}

fn builtin_uniq(stdin: &str) -> CommandResult {
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    for line in stdin.lines() {
        if prev != Some(line) {
            out.push_str(line);
            out.push('\n');
            prev = Some(line);
        }
    }
    CommandResult::success(out)
}

fn builtin_cut(args: &[String], stdin: &str) -> CommandResult {
    let mut delimiter = "\t";
    let mut field_spec = String::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-d" && i + 1 < args.len() {
            delimiter = &args[i + 1];
            i += 2;
        } else if args[i] == "-f" && i + 1 < args.len() {
            field_spec = args[i + 1].clone();
            i += 2;
        } else {
            i += 1;
        }
    }

    if field_spec.is_empty() {
        return CommandResult::error(1, "cut: missing field spec\n".to_string());
    }

    let fields = parse_field_spec(&field_spec);
    let mut out = String::new();
    for line in stdin.lines() {
        let parts: Vec<&str> = line.split(delimiter).collect();
        let selected: Vec<&str> = fields
            .iter()
            .filter_map(|&f| {
                if f > 0 && f <= parts.len() {
                    Some(parts[f - 1])
                } else {
                    None
                }
            })
            .collect();
        out.push_str(&selected.join(delimiter));
        out.push('\n');
    }
    CommandResult::success(out)
}

/// Parse field spec like "1", "1,3", "1-3".
fn parse_field_spec(spec: &str) -> Vec<usize> {
    let mut fields = Vec::new();
    for part in spec.split(',') {
        if let Some((start, end)) = part.split_once('-') {
            let s: usize = start.parse().unwrap_or(1);
            let e: usize = end.parse().unwrap_or(s);
            for f in s..=e {
                fields.push(f);
            }
        } else if let Ok(f) = part.parse::<usize>() {
            fields.push(f);
        }
    }
    fields
}

fn builtin_tr(args: &[String], stdin: &str) -> CommandResult {
    if args.len() < 2 {
        return CommandResult::error(1, "tr: missing operand\n".to_string());
    }
    let set1: Vec<char> = args[0].chars().collect();
    let set2: Vec<char> = args[1].chars().collect();

    let mut out = String::new();
    for ch in stdin.chars() {
        if let Some(pos) = set1.iter().position(|&c| c == ch) {
            let replacement = if pos < set2.len() {
                set2[pos]
            } else {
                *set2.last().unwrap_or(&ch)
            };
            out.push(replacement);
        } else {
            out.push(ch);
        }
    }
    CommandResult::success(out)
}

fn builtin_tee(args: &[String], stdin: &str, vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    if args.is_empty() {
        return CommandResult::success(stdin.to_string());
    }
    for path in args {
        let resolved = resolve_path(path, cwd);
        if let Err(e) = vfs.write(&resolved, stdin.as_bytes()) {
            return CommandResult::error(1, format!("tee: {path}: {e}\n"));
        }
    }
    CommandResult::success(stdin.to_string())
}

// ── Phase 1 builtins (continued) ────────────────────────────────────

fn builtin_grep(args: &[String], stdin: &str, vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    if args.is_empty() {
        return CommandResult::error(1, "grep: missing pattern\n".to_string());
    }

    let pattern = &args[0];
    let files = &args[1..];

    let input = if files.is_empty() {
        // Read from stdin (piped input)
        stdin.to_string()
    } else {
        // Read from files
        let mut combined = String::new();
        for file in files {
            let resolved = resolve_path(file, cwd);
            match vfs.read(&resolved) {
                Ok(data) => match String::from_utf8(data) {
                    Ok(s) => combined.push_str(&s),
                    Err(e) => {
                        return CommandResult::error(1, format!("grep: {file}: {e}\n"));
                    }
                },
                Err(e) => {
                    return CommandResult::error(1, format!("grep: {file}: {e}\n"));
                }
            }
        }
        combined
    };

    let mut matches = String::new();
    for line in input.lines() {
        if line.contains(pattern.as_str()) {
            matches.push_str(line);
            matches.push('\n');
        }
    }

    if matches.is_empty() {
        CommandResult::error(1, "")
    } else {
        CommandResult::success(matches)
    }
}

fn builtin_touch(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let paths: Vec<&str> = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .collect();
    if paths.is_empty() {
        return CommandResult::error(1, "touch: missing file operand\n".to_string());
    }

    for path in paths {
        let resolved = resolve_path(path, cwd);
        if vfs.metadata(&resolved).is_ok() {
            continue;
        }
        if let Err(e) = vfs.write(&resolved, b"") {
            return CommandResult::error(1, format!("touch: {path}: {e}\n"));
        }
    }
    CommandResult::success("")
}

fn builtin_bracket_test(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    if args.last().map(String::as_str) != Some("]") {
        return CommandResult::error(2, "[: missing closing ']'\n".to_string());
    }
    builtin_test(&args[..args.len().saturating_sub(1)], vfs, cwd)
}

fn builtin_test(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let mut args = args;
    let mut negated = false;
    if args.first().map(String::as_str) == Some("!") {
        negated = true;
        args = &args[1..];
    }

    let ok = match args {
        [] => false,
        [value] => !value.is_empty(),
        [flag, path] if matches!(flag.as_str(), "-e" | "-f" | "-d") => {
            let resolved = resolve_path(path, cwd);
            match vfs.metadata(&resolved) {
                Ok(meta) => match flag.as_str() {
                    "-e" => true,
                    "-f" => !meta.is_dir,
                    "-d" => meta.is_dir,
                    _ => false,
                },
                Err(_) => false,
            }
        }
        [left, op, right] if matches!(op.as_str(), "=" | "==" | "!=") => match op.as_str() {
            "=" | "==" => left == right,
            "!=" => left != right,
            _ => false,
        },
        _ => false,
    };

    let ok = if negated { !ok } else { ok };
    if ok {
        CommandResult::success("")
    } else {
        CommandResult::error(1, "")
    }
}

fn builtin_printf(args: &[String]) -> CommandResult {
    if args.is_empty() {
        return CommandResult::success("");
    }
    let format = decode_printf_escapes(&args[0]);
    let values = &args[1..];
    let mut out = String::new();

    if values.is_empty() {
        out.push_str(&format.replace("%%", "%").replace("%s", ""));
        return CommandResult::success(out);
    }

    for value in values {
        let rendered = format
            .replacen("%s", value, 1)
            .replace("%%", "%")
            .replace("%s", "");
        out.push_str(&rendered);
    }
    CommandResult::success(out)
}

fn decode_printf_escapes(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn builtin_basename(args: &[String]) -> CommandResult {
    let Some(path) = args.first() else {
        return CommandResult::error(1, "basename: missing operand\n".to_string());
    };
    let trimmed = path.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    CommandResult::success(format!("{base}\n"))
}

fn builtin_dirname(args: &[String]) -> CommandResult {
    let Some(path) = args.first() else {
        return CommandResult::error(1, "dirname: missing operand\n".to_string());
    };
    let trimmed = path.trim_end_matches('/');
    let dir = match trimmed.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((dir, _)) if !dir.is_empty() => dir,
        _ => ".",
    };
    CommandResult::success(format!("{dir}\n"))
}

fn resolve_path(path: &str, cwd: &str) -> String {
    crate::executor::resolve_against_cwd(path, cwd)
}
