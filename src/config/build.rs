//! Lowers a generic [`Directive`] tree into the typed [`Config`] model,
//! validating directive context and arguments along the way.

use std::net::SocketAddr;

use super::ast::*;
use super::parser::Directive;

/// Build a validated [`Config`] from a parsed directive tree.
pub fn build(dirs: &[Directive]) -> Result<Config, String> {
    let mut cfg = Config::default();

    for d in dirs {
        match d.name.as_str() {
            "worker_processes" => cfg.worker_processes = Some(arg1(d)?),
            "pid" => cfg.pid = Some(arg1(d)?),
            "error_log" => cfg.error_log = Some(arg1(d)?),
            "events" => { /* parsed for compatibility; ignored in v0.1.0 */ }
            "http" => cfg.http = Some(build_http(expect_block(d)?)?),
            // Common main-context directives we accept but do not act on yet.
            "user" | "worker_rlimit_nofile" | "include" | "load_module"
            | "daemon" | "master_process" | "worker_shutdown_timeout" => {}
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
            // Tolerated http-context directives not implemented in v0.1.0.
            "error_log" | "sendfile" | "tcp_nopush" | "tcp_nodelay"
            | "keepalive_timeout" | "types_hash_max_size" | "default_type"
            | "include" | "gzip" | "server_tokens" | "client_max_body_size"
            | "log_format" => {}
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
                    // max_fails / fail_timeout / backup / down: accepted, unused.
                }
                servers.push(UpstreamServer { addr, weight });
            }
            // Accepted for compatibility; v0.1.0 always uses round-robin.
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

    for d in dirs {
        match d.name.as_str() {
            "listen" => {
                let a = arg1(d)?;
                if d.args.iter().any(|x| x == "ssl") {
                    return Err(format!(
                        "line {}: 'listen ... ssl' (TLS) is not supported in v0.1.0",
                        d.line
                    ));
                }
                server.listen = Some(parse_listen(&a, d.line)?);
            }
            "server_name" => server.server_name = d.args.first().cloned(),
            "location" => {
                let path = arg1(d)?;
                server
                    .locations
                    .push(build_location(path, expect_block(d)?, d.line)?);
            }
            "return" => {
                return Err(format!(
                    "line {}: 'return' at server level is not supported in v0.1.0; \
                     place it inside a location block",
                    d.line
                ))
            }
            // Tolerated server-context directives not applied in v0.1.0.
            "access_log" | "error_log" | "root" | "index" | "client_max_body_size"
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

fn build_location(
    path: String,
    dirs: &[Directive],
    line: usize,
) -> Result<Location, String> {
    let mut action: Option<Action> = None;

    for d in dirs {
        let candidate = match d.name.as_str() {
            "return" => {
                let status: u16 = arg1(d)?.parse().map_err(|_| {
                    format!("line {}: invalid status code", d.line)
                })?;
                let body = d.args.get(1).cloned().unwrap_or_default();
                Some(Action::Return { status, body })
            }
            "proxy_pass" => Some(Action::ProxyPass { target: arg1(d)? }),
            "root" => Some(Action::Root { dir: arg1(d)? }),
            // Accepted but not applied in v0.1.0.
            "index" | "proxy_set_header" | "alias" | "try_files" | "autoindex"
            | "add_header" | "proxy_buffering" | "proxy_read_timeout"
            | "proxy_connect_timeout" | "proxy_send_timeout" | "expires" => None,
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

    let action = action.ok_or_else(|| {
        format!(
            "line {line}: location '{path}' has no action \
             (expected return, proxy_pass, or root)"
        )
    })?;
    Ok(Location { path, action })
}

/// Parse a `listen` value: either a bare port or a full `host:port` address.
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
