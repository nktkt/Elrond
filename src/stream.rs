//! TCP `stream` proxy: accept a connection on a listener, pick an upstream
//! peer from the configured `Balancer`, and shovel bytes in both directions
//! until either side closes.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::app::Balancer;
use crate::metrics;

pub async fn run(
    addr: SocketAddr,
    listener: TcpListener,
    balancer_rx: watch::Receiver<Arc<Balancer>>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    info!("stream listening on tcp://{addr}");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (client_stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!("stream accept on {addr}: {e}");
                        continue;
                    }
                };
                metrics::record_stream_accepted();
                let balancer = balancer_rx.borrow().clone();
                tokio::spawn(async move {
                    let _guard = metrics::StreamConnGuard::new();
                    handle(client_stream, peer, balancer).await;
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("stream {addr}: draining");
                    break;
                }
            }
        }
    }

    info!("stream {addr}: stopped");
    Ok(())
}

async fn handle(
    mut client: TcpStream,
    peer: SocketAddr,
    balancer: Arc<Balancer>,
) {
    let upstream_peer = match balancer.pick_for_addr(peer.ip()) {
        Some(p) => p,
        None => {
            warn!(
                "stream: balancer '{}' has no available peer for {peer}",
                balancer.name
            );
            return;
        }
    };
    let _g = upstream_peer.enter();
    let upstream_addr = upstream_peer.addr.clone();
    debug!(
        "stream: {peer} -> '{}' [{}]",
        balancer.name, upstream_addr
    );

    match TcpStream::connect(&upstream_addr).await {
        Ok(mut server) => {
            match tokio::io::copy_bidirectional(&mut client, &mut server).await {
                Ok((to_upstream, from_upstream)) => {
                    upstream_peer.record_success();
                    metrics::record_stream_bytes(to_upstream, from_upstream);
                    debug!(
                        "stream: {peer} <-> {upstream_addr} done \
                         (client→up {to_upstream}, up→client {from_upstream})"
                    );
                }
                Err(e) => {
                    debug!("stream copy {peer} <-> {upstream_addr}: {e}");
                    upstream_peer.record_failure();
                }
            }
        }
        Err(e) => {
            warn!("stream: connect '{upstream_addr}' failed: {e}");
            upstream_peer.record_failure();
        }
    }
}
