//! The data plane: accept connections on a listener, route each request, and
//! drain gracefully on shutdown.

use std::convert::Infallible;
use std::net::SocketAddr;

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::app::{ActionRt, SharedState};
use crate::body::{text, ElrondBody};
use crate::{proxy, static_files};

/// Run one listener until `shutdown` flips to `true`, then drain in-flight
/// connections before returning.
pub async fn run(
    addr: SocketAddr,
    state: SharedState,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    if let Some(name) = &state.server_name {
        info!("listening on http://{addr} (server_name {name})");
    } else {
        info!("listening on http://{addr}");
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

                let io = TokioIo::new(stream);
                let state = state.clone();
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

/// Route a single request to its handler and emit an access-log line.
async fn handle(
    state: SharedState,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<ElrondBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = match state.route(&path) {
        Some(ActionRt::Return { status, body }) => text(*status, body.clone()),
        Some(ActionRt::Static { root }) => static_files::serve(root, &path).await,
        Some(ActionRt::Proxy(balancer)) => {
            proxy::forward(balancer.clone(), req, peer).await
        }
        None => text(404, "404 Not Found\n"),
    };

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
