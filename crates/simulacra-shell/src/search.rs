use simulacra_types::VirtualFs;

use crate::CommandResult;
use crate::builtins::{resolve_path, shell_metadata, shell_read_file};

pub(crate) fn builtin_sed(args: &[String], stdin: &str) -> CommandResult {
    let mut quiet = false;
    let mut expr: Option<&str> = None;

    for arg in args {
        if arg == "-n" {
            quiet = true;
        } else if expr.is_none() {
            expr = Some(arg);
        }
    }

    let Some(expr) = expr else {
        return CommandResult::error(1, "sed: missing expression\n");
    };

    let Some(subst) = parse_substitution(expr) else {
        return CommandResult::error(1, format!("sed: unsupported expression: {expr}\n"));
    };

    let mut out = String::new();
    for line in stdin.lines() {
        let matched = sed_pattern_matches(line, subst.pattern);
        let replaced = apply_substitution(line, &subst);
        if quiet {
            if subst.print && matched {
                out.push_str(&replaced);
                out.push('\n');
            }
        } else {
            out.push_str(&replaced);
            out.push('\n');
            if subst.print && matched {
                out.push_str(&replaced);
                out.push('\n');
            }
        }
    }

    CommandResult::success(out)
}

pub(crate) fn builtin_find(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let (root_arg, predicates) = if args.first().is_some_and(|arg| !arg.starts_with('-')) {
        (args[0].as_str(), &args[1..])
    } else {
        (cwd, args)
    };
    let root = resolve_path(root_arg, cwd);
    let query = FindQuery::parse(predicates);

    let mut results = Vec::new();
    find_recursive(vfs, &root, &query, &mut results);
    results.sort();

    if results.is_empty() {
        CommandResult::success("")
    } else {
        CommandResult::success(format!("{}\n", results.join("\n")))
    }
}

pub(crate) fn builtin_grep(
    args: &[String],
    stdin: &str,
    vfs: &dyn VirtualFs,
    cwd: &str,
) -> CommandResult {
    let parsed = match GrepArgs::parse(args) {
        Ok(parsed) => parsed,
        Err(message) => return CommandResult::error(1, message),
    };

    if parsed.files.is_empty() {
        return grep_input(&parsed, stdin, None);
    }

    let mut targets = Vec::new();
    for file in &parsed.files {
        let resolved = resolve_path(file, cwd);
        collect_grep_targets(vfs, &resolved, parsed.recursive, &mut targets);
    }
    targets.sort();

    let mut out = String::new();
    let mut any_match = false;
    let show_path = parsed.recursive || targets.len() > 1;

    for target in targets {
        let data = match shell_read_file(vfs, &target) {
            Ok(data) => data,
            Err(err) => return CommandResult::error(1, format!("grep: {target}: {err}\n")),
        };
        let input = match String::from_utf8(data) {
            Ok(input) => input,
            Err(err) => return CommandResult::error(1, format!("grep: {target}: {err}\n")),
        };
        let result = grep_text(&parsed, &input, show_path.then_some(target.as_str()));
        if !result.is_empty() {
            any_match = true;
            out.push_str(&result);
        }
    }

    if any_match {
        CommandResult::success(out)
    } else {
        CommandResult::error(1, "")
    }
}

struct Substitution<'a> {
    pattern: &'a str,
    replacement: &'a str,
    global: bool,
    print: bool,
}

fn parse_substitution(expr: &str) -> Option<Substitution<'_>> {
    if !expr.starts_with('s') || expr.len() < 4 {
        return None;
    }
    let delim = expr.chars().nth(1)?;
    let rest = &expr[2..];
    let mut parts = rest.splitn(3, delim);
    let pattern = parts.next()?;
    let replacement = parts.next()?;
    let flags = parts.next().unwrap_or("");
    Some(Substitution {
        pattern,
        replacement,
        global: flags.contains('g'),
        print: flags.contains('p'),
    })
}

fn sed_pattern_matches(line: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_prefix('^') {
        line.starts_with(prefix)
    } else {
        line.contains(pattern)
    }
}

fn apply_substitution(line: &str, subst: &Substitution<'_>) -> String {
    if let Some(prefix) = subst.pattern.strip_prefix('^') {
        if let Some(rest) = line.strip_prefix(prefix) {
            return format!("{}{}", subst.replacement, rest);
        }
        return line.to_string();
    }
    if subst.global {
        line.replace(subst.pattern, subst.replacement)
    } else {
        line.replacen(subst.pattern, subst.replacement, 1)
    }
}

#[derive(Default)]
struct FindQuery {
    file_type: Option<FindType>,
    names: Vec<String>,
}

#[derive(Clone, Copy)]
enum FindType {
    File,
    Dir,
}

impl FindQuery {
    fn parse(args: &[String]) -> Self {
        let mut query = Self::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-type" if i + 1 < args.len() => {
                    query.file_type = match args[i + 1].as_str() {
                        "f" => Some(FindType::File),
                        "d" => Some(FindType::Dir),
                        _ => query.file_type,
                    };
                    i += 2;
                }
                "-name" if i + 1 < args.len() => {
                    query.names.push(args[i + 1].clone());
                    i += 2;
                }
                "-o" | "(" | ")" | "\\(" | "\\)" => i += 1,
                _ => i += 1,
            }
        }
        query
    }

    fn matches(&self, vfs: &dyn VirtualFs, path: &str) -> bool {
        let Ok(metadata) = shell_metadata(vfs, path) else {
            return false;
        };
        if let Some(file_type) = self.file_type {
            match file_type {
                FindType::File if !metadata.is_file => return false,
                FindType::Dir if !metadata.is_dir => return false,
                _ => {}
            }
        }
        if self.names.is_empty() {
            return true;
        }
        let basename = path.rsplit('/').next().unwrap_or(path);
        self.names
            .iter()
            .any(|pattern| glob_match(pattern, basename))
    }
}

fn find_recursive(vfs: &dyn VirtualFs, path: &str, query: &FindQuery, results: &mut Vec<String>) {
    if query.matches(vfs, path) {
        results.push(path.to_string());
    }
    if let Ok(entries) = vfs.list_dir(path) {
        for entry in entries {
            let child = if path == "/" {
                format!("/{entry}")
            } else {
                format!("{path}/{entry}")
            };
            find_recursive(vfs, &child, query, results);
        }
    }
}

struct GrepArgs {
    pattern: String,
    files: Vec<String>,
    recursive: bool,
    line_numbers: bool,
    ignore_case: bool,
    only_matching: bool,
}

impl GrepArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut recursive = false;
        let mut line_numbers = false;
        let mut ignore_case = false;
        let mut only_matching = false;
        let mut pattern: Option<String> = None;
        let mut files = Vec::new();
        let mut parse_flags = true;

        for arg in args {
            if parse_flags && arg == "--" {
                parse_flags = false;
                continue;
            }
            if parse_flags && arg.starts_with('-') && arg.len() > 1 {
                for flag in arg.trim_start_matches('-').chars() {
                    match flag {
                        'r' | 'R' => recursive = true,
                        'n' => line_numbers = true,
                        'i' => ignore_case = true,
                        'o' => only_matching = true,
                        'E' | 'P' => {}
                        _ => {}
                    }
                }
                continue;
            }
            if pattern.is_none() {
                pattern = Some(arg.clone());
            } else {
                files.push(arg.clone());
            }
        }

        let Some(pattern) = pattern else {
            return Err("grep: missing pattern\n".to_string());
        };

        Ok(Self {
            pattern,
            files,
            recursive,
            line_numbers,
            ignore_case,
            only_matching,
        })
    }
}

fn grep_input(parsed: &GrepArgs, input: &str, path: Option<&str>) -> CommandResult {
    let out = grep_text(parsed, input, path);
    if out.is_empty() {
        CommandResult::error(1, "")
    } else {
        CommandResult::success(out)
    }
}

fn grep_text(parsed: &GrepArgs, input: &str, path: Option<&str>) -> String {
    let mut out = String::new();
    for (index, line) in input.lines().enumerate() {
        let line_matches = line_matches(&parsed.pattern, line, parsed.ignore_case);
        if !line_matches {
            continue;
        }
        if parsed.only_matching {
            for matched in only_matches(&parsed.pattern, line) {
                push_grep_prefix(&mut out, path, parsed.line_numbers, index + 1);
                out.push_str(matched);
                out.push('\n');
            }
        } else {
            push_grep_prefix(&mut out, path, parsed.line_numbers, index + 1);
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn collect_grep_targets(
    vfs: &dyn VirtualFs,
    path: &str,
    recursive: bool,
    targets: &mut Vec<String>,
) {
    let Ok(metadata) = shell_metadata(vfs, path) else {
        targets.push(path.to_string());
        return;
    };
    if metadata.is_file {
        targets.push(path.to_string());
        return;
    }
    if !recursive {
        targets.push(path.to_string());
        return;
    }
    if let Ok(entries) = vfs.list_dir(path) {
        for entry in entries {
            let child = if path == "/" {
                format!("/{entry}")
            } else {
                format!("{path}/{entry}")
            };
            collect_grep_targets(vfs, &child, recursive, targets);
        }
    }
}

fn push_grep_prefix(out: &mut String, path: Option<&str>, line_numbers: bool, line: usize) {
    if let Some(path) = path {
        out.push_str(path);
        out.push(':');
    }
    if line_numbers {
        out.push_str(&line.to_string());
        out.push(':');
    }
}

fn line_matches(pattern: &str, line: &str, ignore_case: bool) -> bool {
    if pattern == r"(?<=\s)\S+" {
        return line.split_whitespace().count() > 1;
    }
    if ignore_case {
        let line = line.to_lowercase();
        pattern_alternatives(pattern)
            .iter()
            .any(|pattern| line.contains(&pattern.to_lowercase()))
    } else {
        pattern_alternatives(pattern)
            .iter()
            .any(|pattern| line.contains(pattern))
    }
}

fn pattern_alternatives(pattern: &str) -> Vec<String> {
    let separator = if pattern.contains("\\|") { "\\|" } else { "|" };
    pattern
        .split(separator)
        .map(|part| part.replace("\\s", " "))
        .collect()
}

fn only_matches<'a>(pattern: &str, line: &'a str) -> Vec<&'a str> {
    if pattern == r"(?<=\s)\S+" {
        return line.split_whitespace().skip(1).collect();
    }
    pattern_alternatives(pattern)
        .into_iter()
        .filter_map(|pattern| {
            let start = line.find(&pattern)?;
            Some(&line[start..start + pattern.len()])
        })
        .collect()
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(p: &[char], t: &[char]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            glob_match_inner(&p[1..], t) || (!t.is_empty() && glob_match_inner(p, &t[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&p[1..], &t[1..]),
        (Some(pc), Some(tc)) if pc == tc => glob_match_inner(&p[1..], &t[1..]),
        _ => false,
    }
}
