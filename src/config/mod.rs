//! Nginx-style configuration: lexing, parsing, and lowering to a typed model.
//!
//! ```text
//! text ──lex──▶ tokens ──parse──▶ directive tree ──build──▶ Config
//! ```

mod build;
mod lexer;
mod parser;

pub mod ast;

use std::path::Path;

pub use ast::*;

/// Read and fully parse a configuration file.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config '{}': {e}", path.display()))?;
    parse_str(&text)
}

/// Parse configuration directly from a string. Used by [`load`] and tests.
pub fn parse_str(text: &str) -> Result<Config, String> {
    let tokens = lexer::lex(text)?;
    let dirs = parser::parse(&tokens)?;
    build::build(&dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_return() {
        let cfg = parse_str(
            r#"http { server { listen 8080; location / { return 200 "hi"; } } }"#,
        )
        .unwrap();
        let http = cfg.http.unwrap();
        assert_eq!(http.servers.len(), 1);
        assert_eq!(http.servers[0].locations.len(), 1);
    }

    #[test]
    fn parses_upstream_and_proxy() {
        let src = r#"
            http {
                upstream app {
                    server 127.0.0.1:3000 weight=2;
                    server 127.0.0.1:3001;
                }
                server {
                    listen 8080;
                    location / { proxy_pass http://app; }
                }
            }
        "#;
        let cfg = parse_str(src).unwrap();
        let http = cfg.http.unwrap();
        assert_eq!(http.upstreams.len(), 1);
        assert_eq!(http.upstreams[0].servers.len(), 2);
        assert_eq!(http.upstreams[0].servers[0].weight, 2);
    }

    #[test]
    fn comments_and_strings_are_handled() {
        let src = r#"
            # a comment
            http {
                server {
                    listen 8080; # inline comment
                    location / { return 200 "line one\nline two"; }
                }
            }
        "#;
        let cfg = parse_str(src).unwrap();
        assert!(cfg.http.is_some());
    }

    #[test]
    fn rejects_unknown_directive() {
        let err = parse_str("http { bogus_directive 1; }").unwrap_err();
        assert!(err.contains("bogus_directive"), "got: {err}");
    }

    #[test]
    fn rejects_unclosed_block() {
        let err = parse_str("http { server { listen 8080; ").unwrap_err();
        assert!(err.contains('}') || err.to_lowercase().contains("end of file"));
    }

    #[test]
    fn rejects_tls_listen() {
        let err =
            parse_str("http { server { listen 443 ssl; location / { return 200; } } }")
                .unwrap_err();
        assert!(err.to_lowercase().contains("tls"), "got: {err}");
    }

    #[test]
    fn rejects_location_without_action() {
        let err =
            parse_str("http { server { listen 8080; location / { } } }").unwrap_err();
        assert!(err.contains("no action"), "got: {err}");
    }
}
