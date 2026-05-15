//! The data plane: accept connections on a listener, route each request,
//! and drain gracefully on shutdown.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::app::{ActionRt, HeaderList, ServerState};
use crate::body::{text, ElrondBody};
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
    state_rx: watch::Receiver<Arc<ServerState>>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    {
        let s = state_rx.borrow();
        if let Some(name) = &s.server_name {
            info!("listening on http://{addr} (server_name {name})");
        } else {
            info!("listening on http://{addr}");
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

                let state = state_rx.borrow().clone();
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let state = state.clone();
                    async move { handle(state, req, peer).await }
                });

                let conn = http1::Builder::new().serve_connection(io, service);
                let watched = graceful.watch(conn);
                tokio::spawn(async move {
                    if let Err(e) = watched.await {
                        debug!("connection from {peer} ended: {e}");
                    }
                });
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
        scheme: "http",
    };

    let path = uri.path().to_string();

    let (mut response, add_headers): (Response<ElrondBody>, Option<HeaderList>) =
        match state.route(&path) {
            Some(loc) => {
                let resp = match &loc.action {
                    ActionRt::Return { status, body } => {
                        text(*status, body.render(&ctx))
                    }
                    ActionRt::Static { root, kind } => {
                        static_files::serve(root, kind, &path).await
                    }
                    ActionRt::Proxy {
                        balancer,
                        set_headers,
                    } => {
                        proxy::forward(
                            balancer.clone(),
                            set_headers.clone(),
                            req,
                            peer,
                            &ctx,
                        )
                        .await
                    }
                };
                (resp, Some(loc.add_headers.clone()))
            }
            None => (text(404, "404 Not Found\n"), None),
        };

    if let Some(list) = add_headers {
        apply_add_headers(response.headers_mut(), &list, &ctx);
    }

    info!(
        target: "access",
        "{} \"{} {}\" {}",
        peer.ip(),
        method,
        path,
        response.status().as_u16()
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
