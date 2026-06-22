//! Shell command parser.
//!
//! Supports simple commands, pipes, redirects, `&&`/`||`, env-var expansion,
//! and `$(cmd)` command substitution (parsed but resolved at execution time).

/// A complete shell line: one or more pipelines joined by `&&` or `||`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellLine {
    pub items: Vec<ShellItem>,
}

/// An item in a shell line: a pipeline plus an optional connector to the *next* item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellItem {
    pub pipeline: Pipeline,
    /// How this item connects to the next one (`None` for the last item).
    pub connector: Option<Connector>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    And,       // &&
    Or,        // ||
    Semicolon, // ;
}

/// A pipeline of one or more simple commands connected by `|`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

/// A simple command: program + args + optional redirects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub redirects: Vec<Redirect>,
    /// Tracks whether the program was single-quoted (literal, no expansion).
    pub program_literal: bool,
    /// Parallel to `args` — `true` means the arg was single-quoted and must
    /// not undergo variable expansion.
    pub literal_args: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub kind: RedirectKind,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    /// `> file` — truncate and write
    Truncate,
    /// `>> file` — append
    Append,
}

/// Parse a shell line into a structured [`ShellLine`].
pub fn parse(input: &str) -> ShellLine {
    let input = input.trim();
    if input.is_empty() {
        return ShellLine { items: vec![] };
    }

    let tokens = tokenize(input);
    parse_tokens(&tokens)
}

// ── Tokenizer ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    /// Content from single-quoted strings — must NOT undergo variable expansion.
    SingleQuoted(String),
    Pipe,
    And,
    Or,
    Semicolon,
    RedirectTruncate,
    RedirectAppend,
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Backslash-newline: line continuation (POSIX shell behavior).
        // Consume both characters and treat as whitespace.
        if chars[i] == '\\' && i + 1 < len && chars[i + 1] == '\n' {
            i += 2;
            continue;
        }

        // Skip whitespace
        if chars[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Two-char operators
        if i + 1 < len {
            let two = format!("{}{}", chars[i], chars[i + 1]);
            match two.as_str() {
                "&&" => {
                    tokens.push(Token::And);
                    i += 2;
                    continue;
                }
                "||" => {
                    tokens.push(Token::Or);
                    i += 2;
                    continue;
                }
                ">>" => {
                    tokens.push(Token::RedirectAppend);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }

        // Single-char operators
        match chars[i] {
            '|' => {
                tokens.push(Token::Pipe);
                i += 1;
                continue;
            }
            '>' => {
                tokens.push(Token::RedirectTruncate);
                i += 1;
                continue;
            }
            // Lone `&` (background) treated as `&&` — prevents infinite loop
            '&' => {
                tokens.push(Token::And);
                i += 1;
                continue;
            }
            ';' => {
                tokens.push(Token::Semicolon);
                i += 1;
                continue;
            }
            _ => {}
        }

        // Quoted strings
        if chars[i] == '\'' || chars[i] == '"' {
            let quote = chars[i];
            i += 1;
            let mut word = String::new();
            while i < len && chars[i] != quote {
                // Handle backslash escapes inside double quotes
                if quote == '"' && chars[i] == '\\' && i + 1 < len {
                    let next = chars[i + 1];
                    match next {
                        '"' | '\\' | '$' | '`' => {
                            word.push(next);
                            i += 2;
                            continue;
                        }
                        _ => {}
                    }
                }
                word.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1; // skip closing quote
            }
            // Tag single-quoted words so expand_vars can skip them
            if quote == '\'' {
                tokens.push(Token::SingleQuoted(word));
            } else {
                tokens.push(Token::Word(word));
            }
            continue;
        }

        // Regular word (may contain $VAR, ${VAR}, $(cmd))
        let mut word = String::new();
        while i < len
            && !chars[i].is_ascii_whitespace()
            && !matches!(chars[i], '|' | '>' | '&' | ';')
        {
            // Backslash-newline inside a word: line continuation.
            // Skip both chars and break out — the outer loop will handle
            // whatever comes on the next line.
            if chars[i] == '\\' && i + 1 < len && chars[i + 1] == '\n' {
                i += 2;
                break;
            }
            // Handle $( ... ) as part of word
            if chars[i] == '$' && i + 1 < len && chars[i + 1] == '(' {
                word.push('$');
                word.push('(');
                i += 2;
                let mut depth = 1;
                while i < len && depth > 0 {
                    if chars[i] == '(' {
                        depth += 1;
                    } else if chars[i] == ')' {
                        depth -= 1;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            word.push(chars[i]);
            i += 1;
        }
        if !word.is_empty() {
            tokens.push(Token::Word(word));
        }
    }

    tokens
}

// ── Parser ───────────────────────────────────────────────────────────

fn parse_tokens(tokens: &[Token]) -> ShellLine {
    // Split by And/Or connectors into pipeline groups.
    let mut items = Vec::new();
    let mut current: Vec<&Token> = Vec::new();

    for token in tokens {
        match token {
            Token::And | Token::Or | Token::Semicolon => {
                // Skip empty pipelines (e.g. leading `;` or `;;`)
                if current.is_empty() {
                    continue;
                }
                let pipeline = parse_pipeline(&current);
                let connector = match token {
                    Token::And => Connector::And,
                    Token::Or => Connector::Or,
                    Token::Semicolon => Connector::Semicolon,
                    _ => unreachable!(),
                };
                items.push(ShellItem {
                    pipeline,
                    connector: Some(connector),
                });
                current.clear();
            }
            _ => {
                current.push(token);
            }
        }
    }

    if !current.is_empty() {
        let pipeline = parse_pipeline(&current);
        items.push(ShellItem {
            pipeline,
            connector: None,
        });
    }

    ShellLine { items }
}

fn parse_pipeline(tokens: &[&Token]) -> Pipeline {
    // Split by Pipe into command groups.
    let mut commands = Vec::new();
    let mut current: Vec<&Token> = Vec::new();

    for token in tokens {
        if **token == Token::Pipe {
            commands.push(parse_command(&current));
            current.clear();
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        commands.push(parse_command(&current));
    }

    Pipeline { commands }
}

fn parse_command(tokens: &[&Token]) -> Command {
    let mut program = String::new();
    let mut program_literal = false;
    let mut args = Vec::new();
    let mut literal_args = Vec::new();
    let mut redirects = Vec::new();
    let mut expect_redirect: Option<RedirectKind> = None;

    for token in tokens {
        match token {
            Token::RedirectTruncate => {
                expect_redirect = Some(RedirectKind::Truncate);
            }
            Token::RedirectAppend => {
                expect_redirect = Some(RedirectKind::Append);
            }
            Token::Word(w) | Token::SingleQuoted(w) => {
                let is_literal = matches!(token, Token::SingleQuoted(_));
                if let Some(kind) = expect_redirect.take() {
                    redirects.push(Redirect {
                        kind,
                        target: w.clone(),
                    });
                } else if program.is_empty() {
                    program = w.clone();
                    program_literal = is_literal;
                } else {
                    args.push(w.clone());
                    literal_args.push(is_literal);
                }
            }
            _ => {} // Pipe/And/Or shouldn't appear here
        }
    }

    Command {
        program,
        args,
        redirects,
        program_literal,
        literal_args,
    }
}
