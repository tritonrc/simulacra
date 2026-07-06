use simulacra_types::VirtualFs;

use crate::CommandResult;
use crate::builtins::{resolve_path, shell_read_file};

pub(crate) fn builtin_awk(
    args: &[String],
    stdin: &str,
    vfs: &dyn VirtualFs,
    cwd: &str,
) -> CommandResult {
    let mut field_separator: Option<String> = None;
    let mut program: Option<&str> = None;
    let mut files: &[String] = &[];
    let mut i = 0;

    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "-F" {
            if i + 1 >= args.len() {
                return CommandResult::error(1, "awk: option -F requires an argument\n");
            }
            field_separator = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        if let Some(separator) = arg.strip_prefix("-F")
            && !separator.is_empty()
        {
            field_separator = Some(separator.to_string());
            i += 1;
            continue;
        }
        program = Some(arg);
        files = &args[i + 1..];
        break;
    }

    let Some(program) = program else {
        return CommandResult::error(1, "awk: missing program\n");
    };

    let Some(print_expr) = parse_print_expression(program) else {
        return CommandResult::error(1, format!("awk: unsupported program: {program}\n"));
    };

    let mut out = String::new();
    let mut record_number = 0;

    if files.is_empty() {
        render_records(
            stdin,
            &print_expr,
            field_separator.as_deref(),
            &mut record_number,
            &mut out,
        );
    } else {
        for file in files {
            let path = resolve_path(file, cwd);
            match shell_read_file(vfs, &path).and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|err| simulacra_types::VfsError::Io(format!("invalid UTF-8: {err}")))
            }) {
                Ok(content) => render_records(
                    &content,
                    &print_expr,
                    field_separator.as_deref(),
                    &mut record_number,
                    &mut out,
                ),
                Err(err) => return CommandResult::error(1, format!("awk: {file}: {err}\n")),
            }
        }
    }

    CommandResult::success(out)
}

fn render_records(
    input: &str,
    print_expr: &PrintExpr,
    field_separator: Option<&str>,
    record_number: &mut usize,
    out: &mut String,
) {
    for line in input.lines() {
        *record_number += 1;
        let value = print_expr.render(line, *record_number, field_separator);
        out.push_str(&value);
        out.push('\n');
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrintExpr {
    groups: Vec<Vec<PrintAtom>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrintAtom {
    WholeLine,
    Field(usize),
    LastField,
    RecordNumber,
    Literal(String),
}

impl PrintExpr {
    fn render(&self, line: &str, record_number: usize, separator: Option<&str>) -> String {
        self.groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|atom| atom.render(line, record_number, separator))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl PrintAtom {
    fn render(&self, line: &str, record_number: usize, separator: Option<&str>) -> String {
        match self {
            Self::WholeLine => line.to_string(),
            Self::Field(index) => field(line, separator, *index)
                .unwrap_or_default()
                .to_string(),
            Self::LastField => field(line, separator, usize::MAX)
                .unwrap_or_default()
                .to_string(),
            Self::RecordNumber => record_number.to_string(),
            Self::Literal(value) => value.clone(),
        }
    }
}

fn parse_print_expression(program: &str) -> Option<PrintExpr> {
    let inner = program.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    let expr = inner.strip_prefix("print")?.trim();
    if expr.is_empty() {
        return Some(PrintExpr {
            groups: vec![vec![PrintAtom::WholeLine]],
        });
    }

    let groups = split_print_groups(expr)
        .into_iter()
        .map(parse_concat_group)
        .collect::<Option<Vec<_>>>()?;
    Some(PrintExpr { groups })
}

fn split_print_groups(expr: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in expr.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            ',' if !in_string => {
                groups.push(&expr[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }

    groups.push(&expr[start..]);
    groups
}

fn parse_concat_group(group: &str) -> Option<Vec<PrintAtom>> {
    let mut atoms = Vec::new();
    let mut rest = group.trim();

    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }

        if let Some(after) = rest.strip_prefix("NR")
            && token_boundary(after)
        {
            atoms.push(PrintAtom::RecordNumber);
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("$NF")
            && token_boundary(after)
        {
            atoms.push(PrintAtom::LastField);
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("$0")
            && token_boundary(after)
        {
            atoms.push(PrintAtom::WholeLine);
            rest = after;
            continue;
        }
        if let Some(after_dollar) = rest.strip_prefix('$') {
            let digit_count = after_dollar
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .map(char::len_utf8)
                .sum::<usize>();
            if digit_count > 0 {
                let (digits, after) = after_dollar.split_at(digit_count);
                if token_boundary(after) {
                    atoms.push(PrintAtom::Field(digits.parse().ok()?));
                    rest = after;
                    continue;
                }
            }
        }
        if rest.starts_with('"') {
            let (literal, after) = parse_string_literal(rest)?;
            atoms.push(PrintAtom::Literal(literal));
            rest = after;
            continue;
        }

        return None;
    }

    if atoms.is_empty() { None } else { Some(atoms) }
}

fn token_boundary(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('"')
}

fn parse_string_literal(input: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut escaped = false;

    for (index, ch) in input.char_indices().skip(1) {
        if escaped {
            match ch {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                other => out.push(other),
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some((out, &input[index + ch.len_utf8()..])),
            other => out.push(other),
        }
    }

    None
}

fn field<'a>(line: &'a str, separator: Option<&str>, index: usize) -> Option<&'a str> {
    let fields = if let Some(separator) = separator {
        line.split(separator).collect::<Vec<_>>()
    } else {
        line.split_whitespace().collect::<Vec<_>>()
    };
    if index == usize::MAX {
        fields.last().copied()
    } else {
        fields.get(index.saturating_sub(1)).copied()
    }
}
