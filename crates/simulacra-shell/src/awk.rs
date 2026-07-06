use crate::CommandResult;

pub(crate) fn builtin_awk(args: &[String], stdin: &str) -> CommandResult {
    let mut field_separator: Option<String> = None;
    let mut program: Option<&str> = None;
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
        break;
    }

    let Some(program) = program else {
        return CommandResult::error(1, "awk: missing program\n");
    };

    let Some(print_expr) = parse_print_expression(program) else {
        return CommandResult::error(1, format!("awk: unsupported program: {program}\n"));
    };

    let mut out = String::new();
    for line in stdin.lines() {
        let value = match print_expr {
            PrintExpr::WholeLine => line.to_string(),
            PrintExpr::Field(index) => field(line, field_separator.as_deref(), index)
                .unwrap_or_default()
                .to_string(),
            PrintExpr::LastField => field(line, field_separator.as_deref(), usize::MAX)
                .unwrap_or_default()
                .to_string(),
        };
        out.push_str(&value);
        out.push('\n');
    }

    CommandResult::success(out)
}

#[derive(Clone, Copy)]
enum PrintExpr {
    WholeLine,
    Field(usize),
    LastField,
}

fn parse_print_expression(program: &str) -> Option<PrintExpr> {
    let inner = program.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    let expr = inner.strip_prefix("print")?.trim();
    match expr {
        "$0" | "" => Some(PrintExpr::WholeLine),
        "$NF" => Some(PrintExpr::LastField),
        field if field.starts_with('$') => field[1..].parse::<usize>().ok().map(PrintExpr::Field),
        _ => None,
    }
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
