pub(crate) fn read_delimiter(chars: &[char], mut i: usize) -> (String, usize) {
    if i >= chars.len() {
        return (String::new(), i);
    }
    if matches!(chars[i], '\'' | '"') {
        let quote = chars[i];
        i += 1;
        let mut delimiter = String::new();
        while i < chars.len() && chars[i] != quote {
            delimiter.push(chars[i]);
            i += 1;
        }
        if i < chars.len() {
            i += 1;
        }
        return (delimiter, i);
    }

    let mut delimiter = String::new();
    while i < chars.len()
        && !chars[i].is_ascii_whitespace()
        && !matches!(chars[i], '|' | '>' | '<' | '&' | ';')
    {
        delimiter.push(chars[i]);
        i += 1;
    }
    (delimiter, i)
}

pub(crate) fn consume_body(chars: &[char], mut i: usize, delimiter: &str) -> (String, usize) {
    let mut content = String::new();
    while i < chars.len() {
        let line_start = i;
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        let line: String = chars[line_start..i].iter().collect();
        let has_newline = i < chars.len() && chars[i] == '\n';
        if has_newline {
            i += 1;
        }
        if line == delimiter {
            return (content, i);
        }
        content.push_str(&line);
        if has_newline {
            content.push('\n');
        }
    }
    (content, i)
}
