//! Lowers a generic [`Directive`] tree into the typed [`Config`] model,
//! validating directive context and arguments along the way.

use std::net::SocketAddr;

use super::ast::*;
use super::parser::Directive;
use crate::template::Template;

pub fn build(dirs: &[Directive]) -> Result<Config, String> {
    let mut cfg = Config::default();

    for d in dirs {
        match d.name.as_str() {
            "worker_processes" => cfg.worker_processes = Some(arg1(d)?),
            "pid" => cfg.pid = Some(arg1(d)?),
            "error_log" => cfg.error_log = Some(arg1(d)?),
            "events" => { /* parsed for compatibility; ignored in v0.2.0 */ }
            "http" => cfg.http = Some(build_http(expect_block(d)?)?),
            // Includes should already be expanded by `crate::config::mod`,
            // but tolerate stray ones so `parse_str` (no file context) still
            // accepts configs that name them.
            "include" => {}
            "user" | "worker_rlimit_nofile" | "load_module" | "daemon"
            | "master_process" | "worker_shutdown_timeout" => {}
            other => {
                return Err(format!(
                    "line {}: unknown directive '{other}' in main context",
                    d.line
                ))
            }
        }
    }

    Ok(cfg)
}

fn build_http(dirs: &[Directive]) -> Result<Http, String> {
    let mut http = Http::default();

    for d in dirs {
        match d.name.as_str() {
            "access_log" => http.access_log = Some(arg1(d)?),
            "upstream" => {
                let name = arg1(d)?;
                http.upstreams.push(build_upstream(name, expect_block(d)?, d.line)?);
            }
            "server" => http.servers.push(build_server(expect_block(d)?)?),
            "include" => {}
            "error_log" | "sendfile" | "tcp_nopush" | "tcp_nodelay"
            | "keepalive_timeout" | "types_hash_max_size" | "default_type"
            | "gzip" | "server_tokens" | "client_max_body_size" | "log_format"
            | "types" | "map_hash_bucket_size" | "map_hash_max_size" => {}
            other => {
                return Err(format!(
                    "line {}: unknown directive '{other}' in http context",
                    d.line
                ))
            }
        }
    }
    Ok(http)
}

fn build_upstream(
    name: String,
    dirs: &[Directive],
    line: usize,
) -> Result<Upstream, String> {
    let mut servers = Vec::new();

    for d in dirs {
        match d.name.as_str() {
            "server" => {
                let addr = d.args.first().cloned().ok_or_else(|| {
                    format!("line {}: 'server' requires an address", d.line)
                })?;
                let mut weight = 1u32;
                for a in d.args.iter().skip(1) {
                    if let Some(w) = a.strip_prefix("weight=") {
                        weight = w.parse().map_err(|_| {
                            format!("line {}: invalid weight '{w}'", d.line)
                        })?;
                    }
                }
                servers.push(UpstreamServer { addr, weight });
            }
            "least_conn" | "ip_hash" | "hash" | "keepalive" | "zone" => {}
            other => {
                return Err(format!(
                    "line {}: unknown directive '{other}' in upstream context",
                    d.line
                ))
            }
        }
    }
    if servers.is_empty() {
        return Err(format!("line {line}: upstream '{name}' has no servers"));
    }
    Ok(Upstream { name, servers })
}

fn build_server(dirs: &[Directive]) -> Result<Server, String> {
    let mut server = Server::default();

    // First pass: pick up the server-level `root` so we can cascade it into
    // locations that have no content directive of their own.
    for d in dirs {
        if d.name == "root" {
            server.root = Some(arg1(d)?);
        }
    }

    for d in dirs {
        match d.name.as_str() {
            "listen" => {
                let a = arg1(d)?;
                if d.args.iter().any(|x| x == "ssl") {
                    return Err(format!(
                        "line {}: 'listen ... ssl' (TLS) is not supported yet",
                        d.line
                    ));
                }
                server.listen = Some(parse_listen(&a, d.line)?);
            }
            "server_name" => server.server_name = d.args.first().cloned(),
            "root" => { /* handled in first pass */ }
            "location" => {
                if d.args.is_empty() {
                    return Err(format!(
                        "line {}: 'location' requires a path pattern",
                        d.line
                    ));
                }
                let (kind, path) =
                    parse_location_pattern(&d.args[0], &d.args, d.line)?;
                server.locations.push(build_location(
                    path,
                    kind,
                    expect_block(d)?,
                    d.line,
                    server.root.as_deref(),
                )?);
            }
            "return" => {
                return Err(format!(
                    "line {}: 'return' at server level is not supported; \
                     place it inside a location block",
                    d.line
                ))
            }
            "include" => {}
            "access_log" | "error_log" | "index" | "client_max_body_size"
            | "add_header" | "error_page" => {}
            other => {
                return Err(format!(
                    "line {}: unknown directive '{other}' in server context",
                    d.line
                ))
            }
        }
    }

    if server.listen.is_none() {
        return Err("a 'server' block is missing its 'listen' directive".into());
    }
    Ok(server)
}

fn parse_location_pattern(
    first: &str,
    args: &[String],
    line: usize,
) -> Result<(LocationKind, String), String> {
    match first {
        "=" => {
            let p = args.get(1).ok_or_else(|| {
                format!("line {line}: 'location =' requires a path")
            })?;
            Ok((LocationKind::Exact, p.clone()))
        }
        "~" | "~*" | "^~" => Err(format!(
            "line {line}: location modifier '{first}' is not supported yet"
        )),
        path => Ok((LocationKind::Prefix, path.to_string())),
    }
}

fn build_location(
    path: String,
    kind: LocationKind,
    dirs: &[Directive],
    line: usize,
    server_root: Option<&str>,
) -> Result<Location, String> {
    let mut action: Option<Action> = None;
    let mut set_headers = Vec::new();
    let mut add_headers = Vec::new();

    for d in dirs {
        let candidate = match d.name.as_str() {
            "return" => {
                let status: u16 = arg1(d)?.parse().map_err(|_| {
                    format!("line {}: invalid status code", d.line)
                })?;
                let body = d.args.get(1).cloned().unwrap_or_default();
                Some(Action::Return {
                    status,
                    body: Template::parse(&body),
                })
            }
            "proxy_pass" => Some(Action::ProxyPass { target: arg1(d)? }),
            "root" => Some(Action::Root { dir: arg1(d)? }),
            "alias" => Some(Action::Alias { dir: arg1(d)? }),
            "proxy_set_header" => {
                let name = arg1(d)?;
                let value = d.args.get(1).cloned().unwrap_or_default();
                set_headers.push((name, Template::parse(&value)));
                None
            }
            "add_header" => {
                let name = arg1(d)?;
                let value = d.args.get(1).cloned().unwrap_or_default();
                add_headers.push((name, Template::parse(&value)));
                None
            }
            "include" => None,
            "index" | "try_files" | "autoindex" | "expires"
            | "proxy_buffering" | "proxy_read_timeout"
            | "proxy_connect_timeout" | "proxy_send_timeout"
            | "proxy_next_upstream" | "proxy_hide_header"
            | "proxy_pass_header" | "proxy_redirect" => None,
            other => {
                return Err(format!(
                    "line {}: unknown directive '{other}' in location context",
                    d.line
                ))
            }
        };
        if let Some(found) = candidate {
            if action.is_some() {
                return Err(format!(
                    "line {}: location '{path}' has more than one content directive",
                    d.line
                ));
            }
            action = Some(found);
        }
    }

    let action = match action {
        Some(a) => a,
        None => match server_root {
            Some(r) => Action::Root { dir: r.to_string() },
            None => {
                return Err(format!(
                    "line {line}: location '{path}' has no action \
                     (expected return, proxy_pass, root, or alias)"
                ))
            }
        },
    };

    Ok(Location {
        kind,
        path,
        action,
        set_headers,
        add_headers,
    })
}

fn parse_listen(s: &str, line: usize) -> Result<SocketAddr, String> {
    if let Ok(port) = s.parse::<u16>() {
        return Ok(SocketAddr::from(([0, 0, 0, 0], port)));
    }
    s.parse::<SocketAddr>()
        .map_err(|_| format!("line {line}: invalid listen address '{s}'"))
}

fn arg1(d: &Directive) -> Result<String, String> {
    d.args.first().cloned().ok_or_else(|| {
        format!("line {}: directive '{}' requires an argument", d.line, d.name)
    })
}

fn expect_block(d: &Directive) -> Result<&[Directive], String> {
    d.block.as_deref().ok_or_else(|| {
        format!("line {}: directive '{}' requires a {{ }} block", d.line, d.name)
    })
}
