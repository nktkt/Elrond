//! HTTP/3 (RFC 9114) server on top of QUIC (RFC 9000).
//!
//! Spun up alongside the regular TLS HTTP listener on the same UDP port
//! whenever a `server { listen ... http3; … }` block is configured.
//!
//! v0.38.0 supports:
//!   - Bound `quinn::Endpoint` per listener (TLS 1.3, ALPN `h3`).
//!   - Per-connection `h3::server::Connection`, accepting any number of
//!     concurrent streams.
//!   - `Return` / `Static` / `Metrics` / `TryFiles` / `Proxy` actions.
//!   - Request body is buffered up to `client_max_body_size` before
//!     dispatch (HTTP/3 client-streamed uploads are common but we cap
//!     them to avoid unbounded memory).
//!
//! Out of scope today: connection migration is handled by quinn but not
//! tested; 0-RTT is not enabled; `Alt-Svc` advertisement on HTTP/1.1+2
//! responses isn't auto-emitted (operators can add `add_header alt-svc`).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, Response, Uri};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::app::{ActionRt, ListenerCfg, ServerState};
use crate::body::{full, text, BoxError, ElrondBody};
use crate::metrics;
use crate::request_ctx::RequestCtx;
use crate::{proxy, static_files};

/// Per-connection request-body cap when the location's
/// `client_max_body_size` is `0` (unlimited). Avoids buffering an
/// unbounded body just because the operator said "no limit".
const ABSOLUTE_BODY_CAP: usize = 32 * 1024 * 1024;

/// Run one HTTP/3 listener until `shutdown` flips. Returns when the
/// endpoint stops accepting (either we asked it to or quinn errored).
pub async fn run(
    addr: SocketAddr,
    server_config: quinn::ServerConfig,
    cfg_rx: watch::Receiver<Arc<ListenerCfg>>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    info!("listening on h3://{addr}");

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let cfg = cfg_rx.borrow().clone();
                tokio::spawn(async move {
                    metrics::record_conn_accepted();
                    metrics::record_tls_handshake_success();
                    let _guard = metrics::ConnGuard::new();
                    match incoming.await {
                        Ok(conn) => {
                            let peer = conn.remote_address();
                            if let Err(e) = handle_connection(conn, peer, cfg).await {
                                debug!("h3 connection from {peer}: {e}");
                            }
                        }
                        Err(e) => {
                            metrics::record_tls_handshake_failure();
                            debug!("h3 handshake failure: {e}");
                        }
                    }
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("{addr}: draining QUIC endpoint");
                    break;
                }
            }
        }
    }
    endpoint.close(0u32.into(), b"shutting down");
    endpoint.wait_idle().await;
    info!("{addr}: h3 stopped");
    Ok(())
}

async fn handle_connection(
    quic_conn: quinn::Connection,
    peer: SocketAddr,
    cfg: Arc<ListenerCfg>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut h3_conn =
        h3::server::Connection::new(h3_quinn::Connection::new(quic_conn)).await?;

    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let (req, stream) = match resolver.resolve_request().await {
                        Ok(r) => r,
                        Err(e) => {
                            debug!("h3 stream resolve_request: {e}");
                            return;
                        }
                    };
                    if let Err(e) = handle_stream(req, stream, peer, cfg).await {
                        debug!("h3 stream from {peer}: {e}");
                    }
                });
            }
            Ok(None) => break,
            Err(e) => {
                debug!("h3 accept on connection from {peer}: {e}");
                break;
            }
        }
    }
    Ok(())
}

type H3RequestStream =
    h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

async fn handle_stream(
    req: Request<()>,
    mut stream: H3RequestStream,
    peer: SocketAddr,
    cfg: Arc<ListenerCfg>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (parts, _) = req.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();

    // Pick the right vhost the same way HTTP/1+2 does.
    let host_header = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()));
    let state = cfg.pick_state(host_header).clone();

    // Buffer the body up to a sane cap (client_max_body_size, or
    // ABSOLUTE_BODY_CAP if the operator set "unlimited").
    let limit = if state.client_max_body_size == 0 {
        ABSOLUTE_BODY_CAP
    } else {
        state.client_max_body_size
    };
    let mut buf = BytesMut::new();
    loop {
        let chunk = match stream.recv_data().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                debug!("h3 recv_data: {e}");
                return Err(e.into());
            }
        };
        let bytes = bytes::Bytes::copy_from_slice(chunk.chunk());
        if buf.len() + bytes.len() > limit {
            // Send 413 and abort.
            send_simple(&mut stream, 413, "413 Request Entity Too Large\n")
                .await?;
            return Ok(());
        }
        buf.extend_from_slice(&bytes);
    }
    let body_bytes = buf.freeze();

    let response = dispatch(state, parts.method, parts.uri, headers, body_bytes, peer).await;
    let (rparts, rbody) = response.into_parts();
    let resp_for_send: Response<()> = Response::from_parts(rparts.clone(), ());
    stream.send_response(resp_for_send).await?;

    // Stream the response body to the client.
    let mut body = rbody;
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(f) => {
                if let Ok(data) = f.into_data() {
                    if data.has_remaining() {
                        stream.send_data(data).await?;
                    }
                }
            }
            Err(e) => {
                debug!("h3 response body stream: {e}");
                break;
            }
        }
    }
    stream.finish().await?;

    let status = rparts.status.as_u16();
    metrics::record_request(status);
    info!(
        target: "access",
        "{} \"{} {}\" {} (h3)",
        peer.ip(),
        method,
        uri.path(),
        status
    );
    Ok(())
}

/// Dispatch the request through the same `ServerState` routing as
/// HTTP/1+2. Returns a `Response<ElrondBody>` that the HTTP/3 sender
/// streams back to the client.
async fn dispatch(
    state: Arc<ServerState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body_bytes: Bytes,
    peer: SocketAddr,
) -> Response<ElrondBody> {
    // Pre-build user_vars by walking maps in declaration order (same as
    // HTTP/1+2 handler). Reuse the helper inline.
    let empty: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let base_ctx = RequestCtx {
        peer,
        server_name: state.server_name.as_deref(),
        method: &method,
        uri: &uri,
        headers: &headers,
        scheme: state.scheme,
        user_vars: &empty,
    };
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
    let _ = base_ctx;
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
    let matched = state.route(&path);
    let mut response = match matched {
        Some(loc) => {
            // ACL → limit_req → limit_conn → auth_request → auth_basic →
            // action. Mirror order matches HTTP/1+2 (server.rs handle).
            if !loc.access_rules.is_empty()
                && !crate::access::check(&loc.access_rules, peer.ip())
            {
                return text(403, "403 Forbidden\n");
            }
            if let Some(apply) = &loc.limit_req {
                if let Some(denied) = crate::limit::enforce(apply, &ctx) {
                    return denied;
                }
            }
            let _conn_guard = if let Some(apply) = &loc.limit_conn {
                match crate::limit::enforce_conn(apply, &ctx) {
                    Ok(g) => Some(g),
                    Err(denied) => return denied,
                }
            } else {
                None
            };
            if let Some(url_tmpl) = &loc.auth_request {
                if let Err(denied) =
                    crate::auth_request::check(url_tmpl, &ctx, &headers).await
                {
                    return denied;
                }
            }
            if let Some(auth) = &loc.auth {
                if let Err(challenge) = crate::auth::check(auth, &headers) {
                    return challenge;
                }
            }
            if !loc.mirrors.is_empty() {
                crate::mirror::dispatch(&loc.mirrors, &ctx, &headers, &method);
            }

            match &loc.action {
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
                ActionRt::Proxy {
                    target,
                    set_headers,
                    cache,
                } => {
                    match target.resolve(&ctx) {
                        Some(balancer) => {
                            // Build a Request<ElrondBody> from the buffered
                            // body and forward through the same proxy path
                            // HTTP/1+2 uses.
                            let body: ElrondBody = Full::new(body_bytes.clone())
                                .map_err(|n: std::convert::Infallible| match n {})
                                .boxed();
                            let mut req_parts =
                                hyper::Request::new(()).into_parts().0;
                            req_parts.method = method.clone();
                            req_parts.uri = uri.clone();
                            req_parts.headers = headers.clone();
                            let req = hyper::Request::from_parts(req_parts, body);
                            proxy::forward(
                                balancer,
                                set_headers.clone(),
                                cache.clone(),
                                loc.proxy_read_timeout,
                                loc.proxy_ssl_verify,
                                loc.proxy_client.clone(),
                                req,
                                peer,
                                &ctx,
                            )
                            .await
                        }
                        None => text(
                            502,
                            "502 Bad Gateway (empty / unresolvable proxy_pass)\n",
                        ),
                    }
                }
            }
        }
        None => text(404, "404 Not Found\n"),
    };

    // add_header / expires from the matched location.
    if let Some(loc) = matched {
        for (name, tmpl) in loc.add_headers.iter() {
            let v = tmpl.render(&ctx);
            if let Ok(hv) = hyper::header::HeaderValue::from_str(&v) {
                response.headers_mut().insert(name, hv);
            }
        }
        if let Some(d) = loc.expires {
            apply_expires(response.headers_mut(), d);
        }
    }
    response
}

fn apply_expires(target: &mut hyper::HeaderMap, dur: std::time::Duration) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = dur.as_secs();
    if let Ok(v) =
        hyper::header::HeaderValue::from_str(&format!("max-age={secs}"))
    {
        target.insert("cache-control", v);
    }
    if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
        let expiry = now.as_secs().saturating_add(secs);
        if let Ok(v) =
            hyper::header::HeaderValue::from_str(&crate::http_date::format(expiry))
        {
            target.insert("expires", v);
        }
    }
}

async fn send_simple(
    stream: &mut H3RequestStream,
    status: u16,
    body: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resp = Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(())
        .expect("static response is well-formed");
    stream.send_response(resp).await?;
    stream
        .send_data(Bytes::copy_from_slice(body.as_bytes()))
        .await?;
    stream.finish().await?;
    Ok(())
}

/// Build a `quinn::ServerConfig` from a rustls `ServerConfig` produced
/// by [`crate::tls::build_h3_server_config`].
pub fn quinn_server_config(
    rustls_cfg: Arc<rustls::ServerConfig>,
) -> Result<quinn::ServerConfig, String> {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from((*rustls_cfg).clone())
        .map_err(|e| format!("quinn QuicServerConfig: {e}"))?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
}

#[allow(unused_imports)]
use http_body_util::Empty as _Empty;
