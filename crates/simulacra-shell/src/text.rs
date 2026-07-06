use crate::CommandResult;

pub(crate) fn builtin_sort(_args: &[String], stdin: &str) -> CommandResult {
    let mut lines: Vec<&str> = stdin.lines().collect();
    lines.sort();
    if lines.is_empty() {
        CommandResult::success("")
    } else {
        CommandResult::success(format!("{}\n", lines.join("\n")))
    }
}

pub(crate) fn builtin_uniq(stdin: &str) -> CommandResult {
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

pub(crate) fn builtin_cut(args: &[String], stdin: &str) -> CommandResult {
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

pub(crate) fn builtin_tr(args: &[String], stdin: &str) -> CommandResult {
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

pub(crate) fn builtin_printf(args: &[String]) -> CommandResult {
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

pub(crate) fn builtin_basename(args: &[String]) -> CommandResult {
    let Some(path) = args.first() else {
        return CommandResult::error(1, "basename: missing operand\n".to_string());
    };
    let trimmed = path.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    CommandResult::success(format!("{base}\n"))
}

pub(crate) fn builtin_dirname(args: &[String]) -> CommandResult {
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
