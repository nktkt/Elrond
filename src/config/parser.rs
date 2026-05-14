//! Recursive-descent parser turning a token stream into a generic directive
//! tree. Directive *meaning* is resolved later in [`crate::config::build`].

use super::lexer::{TokKind, Token};

/// One configuration directive: a name, its arguments, and an optional block.
#[derive(Debug, Clone)]
pub struct Directive {
    pub name: String,
    pub args: Vec<String>,
    pub block: Option<Vec<Directive>>,
    pub line: usize,
}

/// Parse a full configuration file (the implicit top-level "main" context).
pub fn parse(tokens: &[Token]) -> Result<Vec<Directive>, String> {
    let mut pos = 0;
    let dirs = parse_block(tokens, &mut pos, false)?;
    Ok(dirs)
}

/// Parse directives until EOF (`nested == false`) or a matching `}`
/// (`nested == true`).
fn parse_block(
    tokens: &[Token],
    pos: &mut usize,
    nested: bool,
) -> Result<Vec<Directive>, String> {
    let mut dirs = Vec::new();

    loop {
        match tokens.get(*pos) {
            None => {
                if nested {
                    return Err("unexpected end of file: missing '}'".into());
                }
                return Ok(dirs);
            }
            Some(Token { kind: TokKind::CloseBrace, line }) => {
                if !nested {
                    return Err(format!("line {line}: unexpected '}}'"));
                }
                *pos += 1;
                return Ok(dirs);
            }
            Some(Token { kind: TokKind::Word(name), line }) => {
                let line = *line;
                let name = name.clone();
                *pos += 1;

                let mut args = Vec::new();
                let block;
                loop {
                    match tokens.get(*pos) {
                        Some(Token { kind: TokKind::Word(w), .. }) => {
                            args.push(w.clone());
                            *pos += 1;
                        }
                        Some(Token { kind: TokKind::Semicolon, .. }) => {
                            *pos += 1;
                            block = None;
                            break;
                        }
                        Some(Token { kind: TokKind::OpenBrace, .. }) => {
                            *pos += 1;
                            block = Some(parse_block(tokens, pos, true)?);
                            break;
                        }
                        Some(Token { kind: TokKind::CloseBrace, line }) => {
                            return Err(format!(
                                "line {line}: expected ';' or '{{' before '}}'"
                            ));
                        }
                        None => {
                            return Err(format!(
                                "line {line}: unexpected end of file in directive '{name}'"
                            ));
                        }
                    }
                }

                dirs.push(Directive { name, args, block, line });
            }
            Some(Token { kind: TokKind::Semicolon, line }) => {
                return Err(format!("line {line}: unexpected ';'"));
            }
            Some(Token { kind: TokKind::OpenBrace, line }) => {
                return Err(format!("line {line}: unexpected '{{'"));
            }
        }
    }
}
