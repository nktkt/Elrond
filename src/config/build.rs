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
            "stream" => cfg.stream = Some(build_stream(expect_block(d)?)?),
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
            "proxy_cache_path" => {
                let zone = parse_cache_path(&d.args, d.line)?;
                http.cache_zones.push(zone);
            }
            "limit_req_zone" => {
                let zone = parse_limit_req_zone(&d.args, d.line)?;
                http.limit_req_zones.push(zone);
            }
            "limit_conn_zone" => {
                let zone = parse_limit_conn_zone(&d.args, d.line)?;
                http.limit_conn_zones.push(zone);
            }
            "map" => {
                let decl = build_map(&d.args, expect_block(d)?, d.line)?;
                http.maps.push(decl);
            }
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
    let mut method = LbMethod::RoundRobin;
    let mut health_check: Option<HealthCheckCfg> = None;

    for d in dirs {
        match d.name.as_str() {
            "server" => {
                let addr = d.args.first().cloned().ok_or_else(|| {
                    format!("line {}: 'server' requires an address", d.line)
                })?;
                let mut s = UpstreamServer {
                    addr,
                    ..UpstreamServer::default()
                };
                for a in d.args.iter().skip(1) {
                    if let Some(w) = a.strip_prefix("weight=") {
                        s.weight = w.parse().map_err(|_| {
                            format!("line {}: invalid weight '{w}'", d.line)
                        })?;
                    } else if let Some(n) = a.strip_prefix("max_fails=") {
                        s.max_fails = n.parse().map_err(|_| {
                            format!("line {}: invalid max_fails '{n}'", d.line)
                        })?;
                    } else if let Some(t) = a.strip_prefix("fail_timeout=") {
                        s.fail_timeout = parse_duration(t).ok_or_else(|| {
                            format!(
                                "line {}: invalid fail_timeout '{t}' (expected e.g. '10s', '500ms', '1m')",
                                d.line
                            )
                        })?;
                    } else if a == "backup" {
                        s.backup = true;
                    } else if a == "down" {
                        s.down = true;
                    }
                    // unknown flags (e.g. slow_start, drain) are silently
                    // accepted for forward compatibility.
                }
                servers.push(s);
            }
            "least_conn" => method = LbMethod::LeastConn,
            "ip_hash" => method = LbMethod::IpHash,
            "hash" | "keepalive" | "zone" => { /* accepted; not yet used */ }
            "health_check" => {
                health_check = Some(parse_health_check(&d.args, d.line)?);
            }
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
    Ok(Upstream {
        name,
        method,
        servers,
        health_check,
    })
}

/// Parse `health_check uri=/path interval=10s fails=2 passes=2 timeout=2s match=200;`
/// All arguments optional; missing values fall back to `HealthCheckCfg::default()`.
fn parse_health_check(args: &[String], line: usize) -> Result<HealthCheckCfg, String> {
    let mut hc = HealthCheckCfg::default();
    for a in args {
        if let Some(v) = a.strip_prefix("uri=") {
            hc.uri = v.to_string();
        } else if let Some(v) = a.strip_prefix("interval=") {
            hc.interval = parse_duration(v).ok_or_else(|| {
                format!("line {line}: invalid health_check interval '{v}'")
            })?;
        } else if let Some(v) = a.strip_prefix("timeout=") {
            hc.timeout = parse_duration(v).ok_or_else(|| {
                format!("line {line}: invalid health_check timeout '{v}'")
            })?;
        } else if let Some(v) = a.strip_prefix("fails=") {
            hc.fails = v
                .parse()
                .map_err(|_| format!("line {line}: invalid health_check fails '{v}'"))?;
        } else if let Some(v) = a.strip_prefix("passes=") {
            hc.passes = v
                .parse()
                .map_err(|_| format!("line {line}: invalid health_check passes '{v}'"))?;
        } else if let Some(v) = a.strip_prefix("match=") {
            hc.expected_status = v.parse().map_err(|_| {
                format!("line {line}: invalid health_check match status '{v}'")
            })?;
        }
    }
    Ok(hc)
}

fn build_stream(dirs: &[Directive]) -> Result<Stream, String> {
    let mut stream = Stream::default();
    for d in dirs {
        match d.name.as_str() {
            "upstream" => {
                let name = arg1(d)?;
                stream
                    .upstreams
                    .push(build_upstream(name, expect_block(d)?, d.line)?);
            }
            "server" => stream
                .servers
                .push(build_stream_server(expect_block(d)?)?),
            "include" => {}
            // Stream-context directives we accept for forward compatibility.
            "access_log" | "error_log" | "log_format" | "proxy_timeout"
            | "proxy_connect_timeout" | "tcp_nodelay" | "resolver" => {}
            other => {
                return Err(format!(
                    "line {}: unknown directive '{other}' in stream context",
                    d.line
                ))
            }
        }
    }
    Ok(stream)
}

fn build_stream_server(dirs: &[Directive]) -> Result<StreamServer, String> {
    let mut server = StreamServer::default();
    for d in dirs {
        match d.name.as_str() {
            "listen" => {
                let a = arg1(d)?;
                if d.args.iter().any(|x| x == "ssl") {
                    return Err(format!(
                        "line {}: 'listen ... ssl' in a stream server (TLS \
                         pass-through / termination) is not supported yet",
                        d.line
                    ));
                }
                if d.args.iter().any(|x| x == "udp") {
                    return Err(format!(
                        "line {}: 'listen ... udp' is not supported yet \
                         (UDP stream proxying is on the roadmap)",
                        d.line
                    ));
                }
                server.listen = Some(parse_listen(&a, d.line)?);
            }
            "proxy_pass" => server.proxy_pass = Some(arg1(d)?),
            "include" => {}
            "proxy_timeout" | "proxy_connect_timeout" | "tcp_nodelay" => {}
            other => {
                return Err(format!(
                    "line {}: unknown directive '{other}' in stream server context",
                    d.line
                ))
            }
        }
    }
    if server.listen.is_none() {
        return Err("a stream 'server' block is missing its 'listen' directive".into());
    }
    if server.proxy_pass.is_none() {
        return Err("a stream 'server' block is missing 'proxy_pass'".into());
    }
    Ok(server)
}

/// Parse Nginx-style time values: `10s`, `500ms`, `1m`, `2h`, `1d`, or a bare
/// integer (interpreted as seconds).
pub(super) fn parse_duration(s: &str) -> Option<std::time::Duration> {
    use std::time::Duration;
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(Duration::from_secs(n));
    }
    let (num, unit) = s
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| s.split_at(i))?;
    let n: u64 = num.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        "h" => Some(Duration::from_secs(n * 60 * 60)),
        "d" => Some(Duration::from_secs(n * 60 * 60 * 24)),
        _ => None,
    }
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
                    server.tls = true;
                }
                server.listen = Some(parse_listen(&a, d.line)?);
            }
            "server_name" => server.server_name = d.args.first().cloned(),
            "ssl_certificate" => server.ssl_certificate = Some(arg1(d)?),
            "ssl_certificate_key" => server.ssl_certificate_key = Some(arg1(d)?),
            "ssl_protocols" | "ssl_ciphers" | "ssl_prefer_server_ciphers"
            | "ssl_session_cache" | "ssl_session_timeout" | "ssl_session_tickets"
            | "ssl_dhparam" | "ssl_ecdh_curve" | "ssl_stapling" => {
                /* Accepted for forward compatibility; not yet applied. */
            }
            "gzip" => {
                server.gzip = Some(parse_on_off(d.args.first().map(String::as_str), d.line)?);
            }
            "gzip_types" => {
                for a in &d.args {
                    server.gzip_types.push(a.to_lowercase());
                }
            }
            "add_header" => {
                let name = arg1(d)?;
                let value = d.args.get(1).cloned().unwrap_or_default();
                server.add_headers.push((name, Template::parse(&value)));
            }
            "gzip_disable" | "gzip_min_length" | "gzip_comp_level"
            | "gzip_proxied" | "gzip_vary" | "gzip_buffers" => {
                /* Tolerated; not yet applied. */
            }
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
            | "error_page" => {}
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
    if server.tls {
        if server.ssl_certificate.is_none() || server.ssl_certificate_key.is_none() {
            return Err(format!(
                "'listen ... ssl' on {} requires both 'ssl_certificate' and 'ssl_certificate_key'",
                server.listen.unwrap()
            ));
        }
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
    let mut expires: Option<std::time::Duration> = None;
    let mut gzip: Option<bool> = None;
    let mut autoindex: bool = false;
    let mut try_files: Option<Vec<TryFilesEntry>> = None;
    let mut auth_basic_realm: Option<String> = None;
    let mut auth_basic_user_file: Option<String> = None;
    let mut limit_req: Option<(String, u32)> = None;
    let mut limit_conn: Option<(String, u32)> = None;
    let mut access_rules: Vec<(bool, String)> = Vec::new();
    let mut proxy_cache: Option<String> = None;
    let mut proxy_cache_key: Option<Template> = None;
    let mut proxy_cache_valid: Vec<(Vec<u16>, std::time::Duration)> = Vec::new();

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
            "metrics" => Some(Action::Metrics),
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
            "expires" => {
                let v = arg1(d)?;
                expires = Some(parse_duration(&v).ok_or_else(|| {
                    format!(
                        "line {}: invalid expires value '{v}' (expected e.g. '1h', '30d')",
                        d.line
                    )
                })?);
                None
            }
            "gzip" => {
                gzip = Some(parse_on_off(d.args.first().map(String::as_str), d.line)?);
                None
            }
            "proxy_cache" => {
                proxy_cache = Some(arg1(d)?);
                None
            }
            "proxy_cache_key" => {
                proxy_cache_key = Some(Template::parse(&arg1(d)?));
                None
            }
            "proxy_cache_valid" => {
                let rule = parse_cache_valid(&d.args, d.line)?;
                proxy_cache_valid.push(rule);
                None
            }
            "proxy_cache_bypass" | "proxy_no_cache" | "proxy_cache_lock"
            | "proxy_cache_use_stale" | "proxy_cache_revalidate"
            | "proxy_cache_methods" | "proxy_cache_min_uses" => None,
            "autoindex" => {
                autoindex = parse_on_off(d.args.first().map(String::as_str), d.line)?;
                None
            }
            "auth_basic" => {
                let realm = arg1(d)?;
                auth_basic_realm = if realm == "off" { None } else { Some(realm) };
                None
            }
            "auth_basic_user_file" => {
                auth_basic_user_file = Some(arg1(d)?);
                None
            }
            "limit_req" => {
                limit_req = Some(parse_limit_req(&d.args, d.line)?);
                None
            }
            "limit_conn" => {
                limit_conn = Some(parse_limit_conn(&d.args, d.line)?);
                None
            }
            "allow" => {
                access_rules.push((true, arg1(d)?));
                None
            }
            "deny" => {
                access_rules.push((false, arg1(d)?));
                None
            }
            "limit_req_status" | "limit_conn_status" => None,
            "include" => None,
            "try_files" => {
                if d.args.len() < 2 {
                    return Err(format!(
                        "line {}: 'try_files' needs at least two arguments",
                        d.line
                    ));
                }
                let mut entries: Vec<TryFilesEntry> =
                    Vec::with_capacity(d.args.len());
                let last_i = d.args.len() - 1;
                for (i, raw) in d.args.iter().enumerate() {
                    if let Some(code) = raw.strip_prefix('=') {
                        if i != last_i {
                            return Err(format!(
                                "line {}: '=N' status is only valid as the last 'try_files' entry",
                                d.line
                            ));
                        }
                        let n: u16 = code.parse().map_err(|_| {
                            format!(
                                "line {}: invalid status code '{raw}' in try_files",
                                d.line
                            )
                        })?;
                        entries.push(TryFilesEntry::Status(n));
                    } else {
                        entries.push(TryFilesEntry::Path(Template::parse(raw)));
                    }
                }
                try_files = Some(entries);
                None
            }
            "index"
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

    // try_files takes precedence over a plain Root: the user-declared
    // content directive becomes the root for the try_files probes.
    let action = if let Some(entries) = try_files {
        // Determine the root: location-level Root if present, else
        // server-level cascade.
        let root = match &action {
            Some(Action::Root { dir }) => dir.clone(),
            Some(other) => {
                return Err(format!(
                    "line {line}: 'try_files' in location '{path}' \
                     cannot be combined with a content directive \
                     ({other:?}); only 'root' is allowed alongside it"
                ));
            }
            None => match server_root {
                Some(r) => r.to_string(),
                None => {
                    return Err(format!(
                        "line {line}: 'try_files' in location '{path}' \
                         needs a 'root' (in this location or at the server level)"
                    ));
                }
            },
        };
        Action::TryFiles { root, entries }
    } else {
        match action {
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
        }
    };

    if auth_basic_realm.is_some() && auth_basic_user_file.is_none() {
        return Err(format!(
            "line {line}: location '{path}' has 'auth_basic' but no \
             'auth_basic_user_file'"
        ));
    }

    Ok(Location {
        kind,
        path,
        action,
        set_headers,
        add_headers,
        expires,
        gzip,
        autoindex,
        auth_basic_realm,
        auth_basic_user_file,
        limit_req,
        limit_conn,
        access_rules,
        proxy_cache,
        proxy_cache_key,
        proxy_cache_valid,
    })
}

/// Build a `map $src $output { … }` declaration.
fn build_map(
    args: &[String],
    block: &[Directive],
    line: usize,
) -> Result<MapDecl, String> {
    if args.len() < 2 {
        return Err(format!(
            "line {line}: 'map' requires '<source-template> <output-var> {{ … }}'"
        ));
    }
    let source = Template::parse(&args[0]);
    let output_name = args[1]
        .strip_prefix('$')
        .ok_or_else(|| {
            format!(
                "line {line}: 'map' output variable must start with '$' (got '{}')",
                args[1]
            )
        })?
        .to_string();
    let mut rules = Vec::with_capacity(block.len());
    for entry in block {
        let pattern = if entry.name == "default" {
            MapPattern::Default
        } else {
            MapPattern::Literal(entry.name.clone())
        };
        let value_raw = entry.args.first().cloned().unwrap_or_default();
        rules.push(MapRule {
            pattern,
            value: Template::parse(&value_raw),
        });
    }
    Ok(MapDecl {
        source,
        output_name,
        rules,
    })
}

fn parse_limit_conn_zone(
    args: &[String],
    line: usize,
) -> Result<LimitConnZoneDecl, String> {
    if args.len() < 2 {
        return Err(format!(
            "line {line}: 'limit_conn_zone' requires '<key> zone=NAME:SIZE'"
        ));
    }
    let key_template = Template::parse(&args[0]);
    let mut name: Option<String> = None;
    let mut max_entries: Option<usize> = None;
    for a in args.iter().skip(1) {
        if let Some(spec) = a.strip_prefix("zone=") {
            let (n, size) = spec
                .split_once(':')
                .ok_or_else(|| format!("line {line}: zone= must be NAME:SIZE"))?;
            let bytes = parse_size(size).ok_or_else(|| {
                format!("line {line}: invalid zone size '{size}'")
            })?;
            name = Some(n.to_string());
            max_entries = Some((bytes / 16).max(1));
        }
    }
    Ok(LimitConnZoneDecl {
        name: name.ok_or_else(|| {
            format!("line {line}: 'limit_conn_zone' missing 'zone=NAME:SIZE'")
        })?,
        key_template,
        max_entries: max_entries.unwrap_or(4096),
    })
}

fn parse_limit_conn(args: &[String], line: usize) -> Result<(String, u32), String> {
    if args.len() < 2 {
        return Err(format!(
            "line {line}: 'limit_conn' requires 'zone=NAME N' (or 'NAME N')"
        ));
    }
    let zone = if let Some(z) = args[0].strip_prefix("zone=") {
        z.to_string()
    } else {
        args[0].clone()
    };
    let max_conn: u32 = args[1].parse().map_err(|_| {
        format!("line {line}: 'limit_conn' max '{}' is not a number", args[1])
    })?;
    Ok((zone, max_conn))
}

/// Parse `limit_req_zone <key> zone=NAME:SIZE rate=Nr/s;`.
fn parse_limit_req_zone(
    args: &[String],
    line: usize,
) -> Result<LimitReqZoneDecl, String> {
    if args.len() < 3 {
        return Err(format!(
            "line {line}: 'limit_req_zone' requires '<key> zone=NAME:SIZE rate=Nr/s'"
        ));
    }
    let key_template = Template::parse(&args[0]);
    let mut name: Option<String> = None;
    let mut max_entries: Option<usize> = None;
    let mut rate_per_sec: Option<f64> = None;

    for a in args.iter().skip(1) {
        if let Some(spec) = a.strip_prefix("zone=") {
            let (n, size) = spec
                .split_once(':')
                .ok_or_else(|| format!("line {line}: zone= must be NAME:SIZE"))?;
            let bytes = parse_size(size).ok_or_else(|| {
                format!("line {line}: invalid zone size '{size}'")
            })?;
            name = Some(n.to_string());
            // Translate bytes into an entry cap.
            max_entries = Some(
                (bytes / crate::limit::APPROX_BYTES_PER_ENTRY).max(1),
            );
        } else if let Some(rs) = a.strip_prefix("rate=") {
            rate_per_sec = Some(crate::limit::parse_rate(rs).ok_or_else(|| {
                format!("line {line}: invalid rate '{rs}' (expected e.g. '5r/s', '60r/m')")
            })?);
        }
    }

    Ok(LimitReqZoneDecl {
        name: name
            .ok_or_else(|| format!("line {line}: 'limit_req_zone' missing 'zone=NAME:SIZE'"))?,
        key_template,
        rate_per_sec: rate_per_sec
            .ok_or_else(|| format!("line {line}: 'limit_req_zone' missing 'rate=Nr/s'"))?,
        max_entries: max_entries.unwrap_or(1024),
    })
}

/// Parse `limit_req zone=NAME [burst=N] [nodelay];`. `nodelay` is parsed
/// for compatibility but is the only mode v0.17.0 implements.
fn parse_limit_req(args: &[String], line: usize) -> Result<(String, u32), String> {
    let mut zone: Option<String> = None;
    let mut burst: u32 = 0;
    for a in args {
        if let Some(z) = a.strip_prefix("zone=") {
            zone = Some(z.to_string());
        } else if let Some(b) = a.strip_prefix("burst=") {
            burst = b
                .parse()
                .map_err(|_| format!("line {line}: invalid burst '{b}'"))?;
        }
        // "nodelay" / "delay=N" accepted; nodelay is implicit today.
    }
    Ok((
        zone.ok_or_else(|| format!("line {line}: 'limit_req' requires 'zone=NAME'"))?,
        burst,
    ))
}

/// Parse a `proxy_cache_path` directive. v0.11.0 needs only the
/// `keys_zone=NAME:SIZE` pair; other arguments (levels, inactive, …) are
/// recognized but ignored.
fn parse_cache_path(
    args: &[String],
    line: usize,
) -> Result<CacheZone, String> {
    if args.is_empty() {
        return Err(format!(
            "line {line}: 'proxy_cache_path' requires arguments (e.g. 'proxy_cache_path /var/cache keys_zone=app:10m')"
        ));
    }
    let mut zone: Option<CacheZone> = None;
    for a in args {
        if let Some(spec) = a.strip_prefix("keys_zone=") {
            let (name, size) = spec
                .split_once(':')
                .ok_or_else(|| format!("line {line}: keys_zone must be NAME:SIZE"))?;
            let max_bytes = parse_size(size).ok_or_else(|| {
                format!("line {line}: invalid keys_zone size '{size}'")
            })?;
            zone = Some(CacheZone {
                name: name.to_string(),
                max_bytes,
            });
        }
        // `levels=`, `max_size=`, `inactive=`, `use_temp_path=`, `loader_*=` …
        // are silently accepted for forward compatibility.
    }
    zone.ok_or_else(|| {
        format!("line {line}: 'proxy_cache_path' needs 'keys_zone=NAME:SIZE'")
    })
}

/// `proxy_cache_valid [code | any]... <duration>;`
fn parse_cache_valid(
    args: &[String],
    line: usize,
) -> Result<(Vec<u16>, std::time::Duration), String> {
    if args.is_empty() {
        return Err(format!(
            "line {line}: 'proxy_cache_valid' requires a duration argument"
        ));
    }
    let (last, head) = args.split_last().unwrap();
    let ttl = parse_duration(last).ok_or_else(|| {
        format!("line {line}: invalid duration '{last}' in proxy_cache_valid")
    })?;
    let mut codes = Vec::new();
    for c in head {
        if c == "any" {
            // Empty Vec means "any status" downstream.
            return Ok((Vec::new(), ttl));
        }
        let code: u16 = c.parse().map_err(|_| {
            format!("line {line}: invalid status code '{c}' in proxy_cache_valid")
        })?;
        codes.push(code);
    }
    Ok((codes, ttl))
}

/// Parse Nginx-style sizes: `10m`, `1g`, `512k`, or a bare byte count.
fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if let Ok(n) = s.parse::<usize>() {
        return Some(n);
    }
    let (num, unit) = s
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| s.split_at(i))?;
    let n: usize = num.parse().ok()?;
    Some(match unit.to_ascii_lowercase().as_str() {
        "k" => n * 1024,
        "m" => n * 1024 * 1024,
        "g" => n * 1024 * 1024 * 1024,
        _ => return None,
    })
}

fn parse_on_off(s: Option<&str>, line: usize) -> Result<bool, String> {
    match s {
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(format!("line {line}: expected 'on' or 'off', got '{other}'")),
        None => Err(format!("line {line}: 'gzip' requires 'on' or 'off'")),
    }
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
