//! Nginx-style configuration: lexing, parsing, include expansion, and
//! lowering to a typed model.
//!
//! ```text
//! text ──lex──▶ tokens ──parse──▶ directive tree
//!           ──expand_includes──▶ flattened tree ──build──▶ Config
//! ```

mod build;
mod lexer;
mod parser;

pub mod ast;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use ast::*;

/// Read and fully parse a configuration file. `include` directives are
/// expanded relative to the file's directory; cycles are reported as errors.
pub fn load(path: &Path) -> Result<Config, String> {
    let abs = path
        .canonicalize()
        .map_err(|e| format!("cannot read config '{}': {e}", path.display()))?;
    let text = std::fs::read_to_string(&abs)
        .map_err(|e| format!("cannot read config '{}': {e}", abs.display()))?;

    let tokens = lexer::lex(&text)?;
    let dirs = parser::parse(&tokens)?;

    let mut visited = HashSet::new();
    visited.insert(abs.clone());
    let base = abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let dirs = expand_includes(dirs, &base, &mut visited)?;

    build::build(&dirs)
}

/// Parse configuration directly from a string. `include` directives are left
/// as-is (and tolerated by `build`). Primarily used by tests.
#[cfg(test)]
pub fn parse_str(text: &str) -> Result<Config, String> {
    let tokens = lexer::lex(text)?;
    let dirs = parser::parse(&tokens)?;
    build::build(&dirs)
}

fn expand_includes(
    dirs: Vec<parser::Directive>,
    base: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<Vec<parser::Directive>, String> {
    let mut out = Vec::with_capacity(dirs.len());
    for mut d in dirs {
        if d.name == "include" {
            let raw = d.args.first().cloned().ok_or_else(|| {
                format!("line {}: 'include' requires a path", d.line)
            })?;
            let candidate = if Path::new(&raw).is_absolute() {
                PathBuf::from(&raw)
            } else {
                base.join(&raw)
            };
            let path = candidate.canonicalize().map_err(|e| {
                format!("line {}: include '{raw}': {e}", d.line)
            })?;
            if !visited.insert(path.clone()) {
                return Err(format!(
                    "line {}: include cycle on '{}'",
                    d.line,
                    path.display()
                ));
            }
            let text = std::fs::read_to_string(&path).map_err(|e| {
                format!("line {}: include '{}': {e}", d.line, path.display())
            })?;
            let tokens = lexer::lex(&text)?;
            let sub = parser::parse(&tokens)?;
            let parent = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let expanded = expand_includes(sub, &parent, visited)?;
            visited.remove(&path);
            out.extend(expanded);
        } else {
            if let Some(block) = d.block.take() {
                d.block = Some(expand_includes(block, base, visited)?);
            }
            out.push(d);
        }
    }
    Ok(out)
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
    fn exact_match_location_parses() {
        let cfg = parse_str(
            r#"http { server { listen 8080;
                  location = /healthz { return 200 "ok"; }
                  location / { return 404; }
               } }"#,
        )
        .unwrap();
        let server = &cfg.http.unwrap().servers[0];
        assert_eq!(server.locations.len(), 2);
        assert_eq!(server.locations[0].kind, LocationKind::Exact);
        assert_eq!(server.locations[0].path, "/healthz");
        assert_eq!(server.locations[1].kind, LocationKind::Prefix);
    }

    #[test]
    fn server_root_cascades_into_location() {
        let cfg = parse_str(
            r#"http { server { listen 8080; root /var/www;
                  location / { }
               } }"#,
        )
        .unwrap();
        let loc = &cfg.http.unwrap().servers[0].locations[0];
        match &loc.action {
            Action::Root { dir } => assert_eq!(dir, "/var/www"),
            other => panic!("expected Root, got {other:?}"),
        }
    }

    #[test]
    fn proxy_set_header_collected() {
        let cfg = parse_str(
            r#"http { server { listen 8080;
                  location / {
                      proxy_pass http://127.0.0.1:3000;
                      proxy_set_header Host $host;
                      proxy_set_header X-Real-IP $remote_addr;
                  }
               } }"#,
        )
        .unwrap();
        let loc = &cfg.http.unwrap().servers[0].locations[0];
        assert_eq!(loc.set_headers.len(), 2);
        assert_eq!(loc.set_headers[0].0, "Host");
    }

    #[test]
    fn add_header_collected() {
        let cfg = parse_str(
            r#"http { server { listen 8080;
                  location / {
                      return 200 "ok";
                      add_header X-Powered-By "elrond";
                  }
               } }"#,
        )
        .unwrap();
        let loc = &cfg.http.unwrap().servers[0].locations[0];
        assert_eq!(loc.add_headers.len(), 1);
        assert_eq!(loc.add_headers[0].0, "X-Powered-By");
    }

    #[test]
    fn alias_action_parses() {
        let cfg = parse_str(
            r#"http { server { listen 8080;
                  location /assets/ { alias /var/assets/; }
               } }"#,
        )
        .unwrap();
        let loc = &cfg.http.unwrap().servers[0].locations[0];
        match &loc.action {
            Action::Alias { dir } => assert_eq!(dir, "/var/assets/"),
            other => panic!("expected Alias, got {other:?}"),
        }
    }

    #[test]
    fn comments_and_strings_are_handled() {
        let cfg = parse_str(
            r#"
            # a comment
            http {
                server {
                    listen 8080; # inline comment
                    location / { return 200 "line one\nline two"; }
                }
            }
        "#,
        )
        .unwrap();
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
        let err = parse_str(
            "http { server { listen 443 ssl; location / { return 200; } } }",
        )
        .unwrap_err();
        assert!(err.to_lowercase().contains("tls"), "got: {err}");
    }

    #[test]
    fn rejects_unsupported_location_modifier() {
        let err = parse_str(
            "http { server { listen 8080; location ~ \\.php$ { return 200; } } }",
        )
        .unwrap_err();
        assert!(err.contains("location modifier"), "got: {err}");
    }

    #[test]
    fn rejects_location_without_action() {
        let err = parse_str(
            "http { server { listen 8080; location / { } } }",
        )
        .unwrap_err();
        assert!(err.contains("no action"), "got: {err}");
    }

    #[test]
    fn include_expansion_via_load() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("elrond-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let inc_path = tmp.join("inc.conf");
        let mut f = std::fs::File::create(&inc_path).unwrap();
        writeln!(f, "location /from-include {{ return 200 \"included\"; }}").unwrap();
        let main_path = tmp.join("main.conf");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, "http {{ server {{ listen 8080; include inc.conf; location / {{ return 200 \"root\"; }} }} }}").unwrap();

        let cfg = load(&main_path).expect("config should load");
        let locs = &cfg.http.unwrap().servers[0].locations;
        assert_eq!(locs.len(), 2);
        assert!(locs.iter().any(|l| l.path == "/from-include"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn include_cycle_detected() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("elrond-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let a_path = tmp.join("a.conf");
        let b_path = tmp.join("b.conf");
        writeln!(std::fs::File::create(&a_path).unwrap(), "include b.conf;").unwrap();
        writeln!(std::fs::File::create(&b_path).unwrap(), "include a.conf;").unwrap();
        let main_path = tmp.join("main.conf");
        writeln!(
            std::fs::File::create(&main_path).unwrap(),
            "http {{ include a.conf; server {{ listen 8080; location / {{ return 200; }} }} }}"
        )
        .unwrap();
        let err = load(&main_path).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
