use std::collections::HashMap;

use crate::CommandResult;

pub(crate) fn expand_vars(
    input: &str,
    env: &HashMap<String, String>,
    last_status: i32,
    mut run_command: impl FnMut(&str) -> CommandResult,
) -> String {
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut result = String::new();
    let mut current_status = last_status;
    let mut i = 0;

    while i < len {
        if chars[i] == '$' && i + 1 < len {
            if chars[i + 1] == '(' {
                i += 2;
                let mut depth = 1;
                let mut cmd_str = String::new();
                while i < len && depth > 0 {
                    if chars[i] == '(' {
                        depth += 1;
                    } else if chars[i] == ')' {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    cmd_str.push(chars[i]);
                    i += 1;
                }
                let sub_result = run_command(&cmd_str);
                current_status = sub_result.exit_code;
                result.push_str(sub_result.stdout.trim_end_matches('\n'));
                continue;
            }

            if chars[i + 1] == '?' {
                result.push_str(&current_status.to_string());
                i += 2;
                continue;
            }

            if chars[i + 1] == '{' {
                i += 2;
                let mut var_name = String::new();
                while i < len && chars[i] != '}' {
                    var_name.push(chars[i]);
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                let val = if var_name == "?" {
                    current_status.to_string()
                } else {
                    env.get(&var_name).cloned().unwrap_or_default()
                };
                result.push_str(&val);
                continue;
            }

            i += 1;
            let mut var_name = String::new();
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                var_name.push(chars[i]);
                i += 1;
            }
            let val = env.get(&var_name).cloned().unwrap_or_default();
            result.push_str(&val);
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}
