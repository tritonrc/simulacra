use simulacra_types::VirtualFs;

use crate::CommandResult;
use crate::builtins::{resolve_path, shell_metadata, shell_read_file};

pub(crate) fn builtin_rg(args: &[String], vfs: &dyn VirtualFs, cwd: &str) -> CommandResult {
    let query = match RgQuery::parse(args) {
        Ok(query) => query,
        Err(message) => return CommandResult::error(1, message),
    };

    let roots = if query.paths.is_empty() {
        vec![cwd.to_string()]
    } else {
        query
            .paths
            .iter()
            .map(|path| resolve_path(path, cwd))
            .collect()
    };

    let mut files = Vec::new();
    for root in roots {
        collect_files(vfs, &root, &query, &mut files);
    }
    files.sort();
    files.dedup();

    if query.files_only {
        return file_list_result(files);
    }

    let mut out = String::new();
    for file in files {
        let data = match shell_read_file(vfs, &file) {
            Ok(data) => data,
            Err(err) => return CommandResult::error(1, format!("rg: {file}: {err}\n")),
        };
        let input = match String::from_utf8(data) {
            Ok(input) => input,
            Err(err) => return CommandResult::error(1, format!("rg: {file}: {err}\n")),
        };
        if query.list_matching {
            if input
                .lines()
                .any(|line| line_matches(&query.pattern, line, query.ignore_case))
            {
                out.push_str(&file);
                out.push('\n');
            }
            continue;
        }
        for (index, line) in input.lines().enumerate() {
            if line_matches(&query.pattern, line, query.ignore_case) {
                out.push_str(&file);
                out.push(':');
                if query.line_numbers {
                    out.push_str(&(index + 1).to_string());
                    out.push(':');
                }
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    if out.is_empty() {
        CommandResult::error(1, "")
    } else {
        CommandResult::success(out)
    }
}

#[derive(Default)]
struct RgQuery {
    files_only: bool,
    line_numbers: bool,
    ignore_case: bool,
    list_matching: bool,
    globs: Vec<String>,
    pattern: String,
    paths: Vec<String>,
}

impl RgQuery {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut query = Self::default();
        let mut parse_flags = true;
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if parse_flags && arg == "--" {
                parse_flags = false;
                i += 1;
                continue;
            }
            if parse_flags && matches!(arg.as_str(), "-g" | "--glob") {
                let Some(glob) = args.get(i + 1) else {
                    return Err("rg: option requires an argument: --glob\n".to_string());
                };
                query.globs.push(glob.clone());
                i += 2;
                continue;
            }
            if parse_flags && arg == "--files" {
                query.files_only = true;
                i += 1;
                continue;
            }
            if parse_flags && matches!(arg.as_str(), "--hidden" | "--no-ignore") {
                i += 1;
                continue;
            }
            if parse_flags && arg.starts_with('-') && arg.len() > 1 {
                for flag in arg.trim_start_matches('-').chars() {
                    match flag {
                        'n' => query.line_numbers = true,
                        'i' => query.ignore_case = true,
                        'l' => query.list_matching = true,
                        _ => {}
                    }
                }
                i += 1;
                continue;
            }
            if query.files_only || !query.pattern.is_empty() {
                query.paths.push(arg.clone());
            } else {
                query.pattern = arg.clone();
            }
            i += 1;
        }

        if !query.files_only && query.pattern.is_empty() {
            return Err("rg: missing pattern\n".to_string());
        }
        if !query.files_only && !query.line_numbers {
            query.line_numbers = true;
        }
        Ok(query)
    }

    fn matches_glob(&self, path: &str) -> bool {
        self.globs.is_empty()
            || self
                .globs
                .iter()
                .any(|glob| glob_match(glob, path.rsplit('/').next().unwrap_or(path)))
    }
}

fn collect_files(vfs: &dyn VirtualFs, path: &str, query: &RgQuery, files: &mut Vec<String>) {
    let Ok(metadata) = shell_metadata(vfs, path) else {
        return;
    };
    if metadata.is_file {
        if query.matches_glob(path) {
            files.push(path.to_string());
        }
        return;
    }
    if let Ok(entries) = vfs.list_dir(path) {
        for entry in entries {
            let child = if path == "/" {
                format!("/{entry}")
            } else {
                format!("{path}/{entry}")
            };
            collect_files(vfs, &child, query, files);
        }
    }
}

fn file_list_result(files: Vec<String>) -> CommandResult {
    if files.is_empty() {
        CommandResult::success("")
    } else {
        CommandResult::success(format!("{}\n", files.join("\n")))
    }
}

fn line_matches(pattern: &str, line: &str, ignore_case: bool) -> bool {
    if ignore_case {
        line.to_lowercase().contains(&pattern.to_lowercase())
    } else {
        line.contains(pattern)
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_inner(&pattern, &text)
}

fn glob_match_inner(pattern: &[char], text: &[char]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            glob_match_inner(&pattern[1..], text)
                || (!text.is_empty() && glob_match_inner(pattern, &text[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&pattern[1..], &text[1..]),
        (Some(pc), Some(tc)) if pc == tc => glob_match_inner(&pattern[1..], &text[1..]),
        _ => false,
    }
}
