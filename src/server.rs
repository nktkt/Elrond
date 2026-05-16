//! The data plane: accept connections on a listener, route each request,
//! and drain gracefully on shutdown.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper_util::rt::TokioExecutor;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::app::{ActionRt, ListenerCfg, LocationRt, ServerState};

/// Upper bound on how many bytes we'll buffer to gzip a proxied response.
/// Anything larger (or with no `Content-Length`) streams uncompressed.
const PROXY_GZIP_MAX_COLLECT: usize = 256 * 1024;
use crate::body::{full, text, ElrondBody};
use crate::gzip;
use crate::metrics;
use crate::request_ctx::RequestCtx;
use crate::template::Template;
use crate::{proxy, static_files};

/// Run one listener until `shutdown` flips to `true`, then drain in-flight
/// connections. Each new connection snapshots the current `state` from
/// `state_rx`, so a reload that updates `state_rx` reaches new connections
/// without disturbing in-flight ones.
pub async fn run(
    addr: SocketAddr,
    listener: TcpListener,
    tls_rx: Option<watch::Receiver<Arc<tokio_rustls::TlsAcceptor>>>,
    cfg_rx: watch::Receiver<Arc<ListenerCfg>>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let scheme = if tls_rx.is_some() { "https" } else { "http" };
    {
        let cfg = cfg_rx.borrow();
        let names: Vec<&str> = cfg
            .vhosts
            .iter()
            .filter_map(|v| v.server_name.as_deref())
            .collect();
        if names.is_empty() {
            info!("listening on {scheme}://{addr}");
        } else {
            info!(
                "listening on {scheme}://{addr} (vhosts: {})",
                names.join(", ")
            );
        }
    }

    let graceful = GracefulShutdown::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!("accept error on {addr}: {e}");
                        continue;
                    }
                };

                metrics::record_conn_accepted();
                let cfg = cfg_rx.borrow().clone();
                // Per-accept snapshot of the current TLS acceptor (if any).
                // Supports cert hot-reload: a `SIGHUP` that pushes a new
                // acceptor into `tls_rx` reaches subsequent connections
                // without disturbing in-flight handshakes.
                let tls_acceptor_for_this_conn =
                    tls_rx.as_ref().map(|rx| (**rx.borrow()).clone());

                match tls_acceptor_for_this_conn {
                    None => {
                        let io = TokioIo::new(stream);
                        let cfg = cfg.clone();
                        let service = service_fn(move |req| {
                            let cfg = cfg.clone();
                            async move { handle_listener(cfg, req, peer).await }
                        });
                        let conn = http1::Builder::new().serve_connection(io, service);
                        let watched = graceful.watch(conn);
                        tokio::spawn(async move {
                            let _conn_guard = metrics::ConnGuard::new();
                            if let Err(e) = watched.await {
                                debug!("connection from {peer} ended: {e}");
                            }
                        });
                    }
                    Some(acceptor) => {
                        // TLS handshake is performed in the spawned task so the
                        // accept loop is not stalled. After handshake we branch
                        // on ALPN: h2 → HTTP/2, otherwise → HTTP/1.1.
                        let cfg = cfg.clone();
                        tokio::spawn(async move {
                            let _conn_guard = metrics::ConnGuard::new();
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    metrics::record_tls_handshake_success();
                                    let alpn = tls_stream
                                        .get_ref()
                                        .1
                                        .alpn_protocol()
                                        .map(|p| p.to_vec());
                                    let cfg = cfg.clone();
                                    let service = service_fn(move |req| {
                                        let cfg = cfg.clone();
                                        async move {
                                            handle_listener(cfg, req, peer).await
                                        }
                                    });
                                    let io = TokioIo::new(tls_stream);
                                    if alpn.as_deref() == Some(b"h2") {
                                        if let Err(e) = http2::Builder::new(
                                            TokioExecutor::new(),
                                        )
                                        .serve_connection(io, service)
                                        .await
                                        {
                                            debug!(
                                                "h2 connection from {peer} ended: {e}"
                                            );
                                        }
                                    } else if let Err(e) = http1::Builder::new()
                                        .serve_connection(io, service)
                                        .await
                                    {
                                        debug!(
                                            "tls/h1 connection from {peer} ended: {e}"
                                        );
                                    }
                                }
                                Err(e) => {
                                    metrics::record_tls_handshake_failure();
                                    debug!("tls handshake from {peer} failed: {e}");
                                }
                            }
                        });
                    }
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("{addr}: draining in-flight connections");
                    break;
                }
            }
        }
    }

    drop(listener);
    graceful.shutdown().await;
    info!("{addr}: stopped");
    Ok(())
}

async fn handle_listener(
    cfg: Arc<ListenerCfg>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<ElrondBody>, Infallible> {
    // Pick the right vhost for this request based on its Host header.
    let host_header = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));
    let state = cfg.pick_state(host_header).clone();
    handle(state, req, peer).await
}

async fn handle(
    state: Arc<ServerState>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<ElrondBody>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    // Enforce client_max_body_size up front via Content-Length when the
    // client advertised one. Streamed bodies without a length still
    // accumulate against backend / cache buffering limits; this is the
    // cheap, common-case guard against hostile uploads.
    if state.client_max_body_size > 0 {
        if let Some(len_header) = headers.get("content-length") {
            if let Some(n) = len_header
                .to_str()
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
            {
                if n > state.client_max_body_size {
                    let resp = text(
                        413,
                        format!(
                            "413 Request Entity Too Large (limit {} bytes)\n",
                            state.client_max_body_size
                        ),
                    );
                    metrics::record_request(413);
                    info!(
                        target: "access",
                        "{} \"{} {}\" 413 (client_max_body_size)",
                        peer.ip(),
                        method,
                        uri.path()
                    );
                    return Ok(resp);
                }
            }
        }
    }

    // Evaluate `map` declarations in declaration order with an accumulating
    // user-vars map, so a later map can reference an earlier map's output.
    // Recursion-by-loop is not supported (each map sees only what was
    // declared above it), matching Nginx semantics.
    let mut user_vars: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(state.maps.len());
    for m in state.maps.iter() {
        let stage_ctx = RequestCtx {
            peer,
            server_name: state.server_name.as_deref(),
            method: &method,
            uri: &uri,
            headers: &headers,
            scheme: state.scheme,
            user_vars: &user_vars,
        };
        let source_value = m.source.render(&stage_ctx);
        let mut chosen: Option<&crate::config::MapRule> = None;
        let mut default_rule: Option<&crate::config::MapRule> = None;
        for r in &m.rules {
            match &r.pattern {
                crate::config::MapPattern::Literal(s) if *s == source_value => {
                    chosen = Some(r);
                    break;
                }
                crate::config::MapPattern::Default => {
                    default_rule = default_rule.or(Some(r));
                }
                _ => {}
            }
        }
        let value = chosen
            .or(default_rule)
            .map(|r| r.value.render(&stage_ctx))
            .unwrap_or_default();
        user_vars.insert(m.output_name.clone(), value);
    }

    let ctx = RequestCtx {
        peer,
        server_name: state.server_name.as_deref(),
        method: &method,
        uri: &uri,
        headers: &headers,
        scheme: state.scheme,
        user_vars: &user_vars,
    };

    let path = uri.path().to_string();

    let (mut response, matched): (Response<ElrondBody>, Option<&LocationRt>) =
        match state.route(&path) {
            Some(loc) => {
                // allow / deny — fastest reject, before any work.
                if !loc.access_rules.is_empty()
                    && !crate::access::check(&loc.access_rules, peer.ip())
                {
                    let denied = text(403, "403 Forbidden\n");
                    metrics::record_request(403);
                    info!(
                        target: "access",
                        "{} \"{} {}\" 403 (allow/deny)",
                        peer.ip(),
                        method,
                        path
                    );
                    return Ok(denied);
                }

                // limit_req — rate-limit checks come first so we don't
                // do auth or upstream work just to throw it away on 503.
                if let Some(apply) = &loc.limit_req {
                    if let Some(denied) = crate::limit::enforce(apply, &ctx) {
                        let status = denied.status().as_u16();
                        metrics::record_request(status);
                        info!(
                            target: "access",
                            "{} \"{} {}\" {} (limit_req)",
                            peer.ip(),
                            method,
                            path,
                            status
                        );
                        return Ok(denied);
                    }
                }

                // limit_conn — try to acquire a slot, held by an RAII guard
                // for the lifetime of this request.
                let _conn_guard = if let Some(apply) = &loc.limit_conn {
                    match crate::limit::enforce_conn(apply, &ctx) {
                        Ok(g) => Some(g),
                        Err(denied) => {
                            let status = denied.status().as_u16();
                            metrics::record_request(status);
                            info!(
                                target: "access",
                                "{} \"{} {}\" {} (limit_conn)",
                                peer.ip(),
                                method,
                                path,
                                status
                            );
                            return Ok(denied);
                        }
                    }
                } else {
                    None
                };

                // auth_basic — challenge before doing any work.
                if let Some(auth) = &loc.auth {
                    if let Err(challenge) = crate::auth::check(auth, &headers) {
                        let status = challenge.status().as_u16();
                        metrics::record_request(status);
                        info!(
                            target: "access",
                            "{} \"{} {}\" {} (auth_basic challenge)",
                            peer.ip(),
                            method,
                            path,
                            status
                        );
                        return Ok(challenge);
                    }
                }
                let resp = match &loc.action {
                    ActionRt::Return { status, body } => {
                        text(*status, body.render(&ctx))
                    }
                    ActionRt::Static { root, kind } => {
                        static_files::serve(
                            root,
                            kind,
                            &path,
                            &headers,
                            &method,
                            loc.autoindex,
                        )
                        .await
                    }
                    ActionRt::Proxy {
                        balancer,
                        set_headers,
                        cache,
                    } => {
                        proxy::forward(
                            balancer.clone(),
                            set_headers.clone(),
                            cache.clone(),
                            loc.proxy_read_timeout,
                            req,
                            peer,
                            &ctx,
                        )
                        .await
                    }
                    ActionRt::Metrics => {
                        let body = metrics::render(crate::VERSION);
                        Response::builder()
                            .status(200)
                            .header(
                                "content-type",
                                "text/plain; version=0.0.4; charset=utf-8",
                            )
                            .body(full(body))
                            .expect("metrics response is well-formed")
                    }
                    ActionRt::TryFiles { root, entries } => {
                        static_files::try_files(
                            root,
                            entries,
                            &ctx,
                            &headers,
                            &method,
                        )
                        .await
                    }
                };
                (resp, Some(loc))
            }
            None => (text(404, "404 Not Found\n"), None),
        };

    let mut gzip_eligible = false;
    if let Some(loc) = matched {
        apply_add_headers(response.headers_mut(), &loc.add_headers, &ctx);
        if let Some(d) = loc.expires {
            apply_expires(response.headers_mut(), d);
        }
        // Proxy responses stream — only static/return bodies are gzip-eligible
        // in v0.10.0. Detection is by the action variant we just served.
        gzip_eligible = loc.gzip.unwrap_or(state.gzip);
    }

    if gzip_eligible {
        // Already-buffered responses (Static / Return / Metrics / TryFiles)
        // have an exact in-memory body; let gzip collect freely. Proxy
        // responses stream, so guard the buffer with a size limit and skip
        // gzip when the upstream didn't tell us the size.
        let max_collect = match matched.map(|l| &l.action) {
            Some(ActionRt::Proxy { .. }) => Some(PROXY_GZIP_MAX_COLLECT),
            _ => None,
        };
        response = gzip::maybe_compress(
            response,
            &headers,
            true,
            &state.gzip_types,
            max_collect,
        )
        .await;
    }

    let status = response.status().as_u16();
    metrics::record_request(status);
    info!(
        target: "access",
        "{} \"{} {}\" {}",
        peer.ip(),
        method,
        path,
        status
    );
    Ok(response)
}

fn apply_add_headers(
    target: &mut hyper::HeaderMap,
    headers: &[(HeaderName, Template)],
    ctx: &RequestCtx<'_>,
) {
    for (name, tmpl) in headers {
        let value = tmpl.render(ctx);
        match HeaderValue::from_str(&value) {
            Ok(v) => {
                target.insert(name, v);
            }
            Err(_) => debug!("add_header: invalid value for '{name}'"),
        }
    }
}

/// Apply `expires <duration>` semantics: set both `Cache-Control: max-age=N`
/// and `Expires: <http-date>`.
fn apply_expires(target: &mut hyper::HeaderMap, dur: std::time::Duration) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = dur.as_secs();
    if let Ok(v) = HeaderValue::from_str(&format!("max-age={secs}")) {
        target.insert("cache-control", v);
    }
    if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
        let expiry = now.as_secs().saturating_add(secs);
        if let Ok(v) = HeaderValue::from_str(&crate::http_date::format(expiry)) {
            target.insert("expires", v);
        }
    }
}
