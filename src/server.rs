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

use crate::app::{ActionRt, LocationRt, ServerState};
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
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    state_rx: watch::Receiver<Arc<ServerState>>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let scheme = if tls_acceptor.is_some() { "https" } else { "http" };
    {
        let s = state_rx.borrow();
        if let Some(name) = &s.server_name {
            info!("listening on {scheme}://{addr} (server_name {name})");
        } else {
            info!("listening on {scheme}://{addr}");
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
                let state = state_rx.borrow().clone();

                match tls_acceptor.clone() {
                    None => {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| {
                            let state = state.clone();
                            async move { handle(state, req, peer).await }
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
                                    let service = service_fn(move |req| {
                                        let state = state.clone();
                                        async move { handle(state, req, peer).await }
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

async fn handle(
    state: Arc<ServerState>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<ElrondBody>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    let ctx = RequestCtx {
        peer,
        server_name: state.server_name.as_deref(),
        method: &method,
        uri: &uri,
        headers: &headers,
        scheme: state.scheme,
    };

    let path = uri.path().to_string();

    let (mut response, matched): (Response<ElrondBody>, Option<&LocationRt>) =
        match state.route(&path) {
            Some(loc) => {
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
        gzip_eligible = matches!(
            &loc.action,
            ActionRt::Return { .. } | ActionRt::Static { .. } | ActionRt::Metrics
        ) && loc.gzip.unwrap_or(state.gzip);
    }

    if gzip_eligible {
        response = gzip::maybe_compress(
            response,
            &headers,
            true,
            &state.gzip_types,
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
