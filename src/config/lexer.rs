//! Tokenizer for the Nginx-style configuration syntax.
//!
//! The grammar is intentionally tiny: barewords, quoted strings, and the three
//! punctuation tokens `{`, `}`, `;`. Comments run from `#` to end of line.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokKind {
    /// A bareword or quoted string. Quotes and escapes are already resolved.
    Word(String),
    OpenBrace,
    CloseBrace,
    Semicolon,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokKind,
    pub line: usize,
}

/// Split `input` into tokens, tracking 1-based line numbers for diagnostics.
pub fn lex(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut line = 1usize;
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            '\n' => {
                line += 1;
                chars.next();
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '#' => {
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '{' => {
                chars.next();
                tokens.push(Token { kind: TokKind::OpenBrace, line });
            }
            '}' => {
                chars.next();
                tokens.push(Token { kind: TokKind::CloseBrace, line });
            }
            ';' => {
                chars.next();
                tokens.push(Token { kind: TokKind::Semicolon, line });
            }
            '"' | '\'' => {
                let quote = c;
                let start_line = line;
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some(ch) if ch == quote => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some(other) => s.push(other),
                            None => {
                                return Err(format!(
                                    "line {start_line}: unterminated string"
                                ))
                            }
                        },
                        Some('\n') => {
                            line += 1;
                            s.push('\n');
                        }
                        Some(ch) => s.push(ch),
                        None => {
                            return Err(format!(
                                "line {start_line}: unterminated string"
                            ))
                        }
                    }
                }
                tokens.push(Token { kind: TokKind::Word(s), line: start_line });
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '{' | '}' | ';' | '#') {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                tokens.push(Token { kind: TokKind::Word(s), line });
            }
        }
    }

    Ok(tokens)
}
