//! TCP `stream` proxy: accept a connection on a listener, pick an upstream
//! peer from the configured `Balancer`, and shovel bytes in both directions
//! until either side closes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::app::Balancer;
use crate::metrics;

/// Per-datagram timeout for the UDP relay: how long to wait for the
/// upstream to answer before giving up and dropping the exchange.
const UDP_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Max single-datagram size we'll accept on the listener side.
const UDP_MAX_DATAGRAM: usize = 65_507;

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

/// Stateless UDP relay. Each incoming datagram is forwarded to a peer
/// chosen by the balancer; the reply is sent back to the original
/// client address. There is no session table — high-throughput / DNS-
/// like traffic works; long-lived UDP flows (QUIC, RTP) are out of
/// scope for v0.33.
pub async fn run_udp(
    addr: SocketAddr,
    sock: Arc<UdpSocket>,
    balancer_rx: watch::Receiver<Arc<Balancer>>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    info!("stream listening on udp://{addr}");

    let mut buf = vec![0u8; UDP_MAX_DATAGRAM];
    loop {
        tokio::select! {
            recv = sock.recv_from(&mut buf) => {
                let (n, src) = match recv {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("udp stream recv on {addr}: {e}");
                        continue;
                    }
                };
                metrics::record_stream_accepted();
                let data = buf[..n].to_vec();
                let balancer = balancer_rx.borrow().clone();
                let client_sock = sock.clone();
                tokio::spawn(async move {
                    let _guard = metrics::StreamConnGuard::new();
                    handle_udp(client_sock, src, data, balancer).await;
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("udp stream {addr}: draining");
                    break;
                }
            }
        }
    }
    info!("udp stream {addr}: stopped");
    Ok(())
}

async fn handle_udp(
    client_sock: Arc<UdpSocket>,
    client_addr: SocketAddr,
    request: Vec<u8>,
    balancer: Arc<Balancer>,
) {
    let upstream_peer = match balancer.pick_for_addr(client_addr.ip()) {
        Some(p) => p,
        None => {
            warn!(
                "udp: balancer '{}' has no available peer for {client_addr}",
                balancer.name
            );
            return;
        }
    };
    let _g = upstream_peer.enter();
    let upstream_addr = upstream_peer.addr.clone();
    debug!("udp: {client_addr} -> '{}' [{}]", balancer.name, upstream_addr);

    // Ephemeral socket per exchange so the kernel allocates a fresh
    // source port and we don't need a NAT-style session table.
    let up_sock = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            warn!("udp: bind ephemeral socket: {e}");
            upstream_peer.record_failure();
            return;
        }
    };
    if let Err(e) = up_sock.connect(&upstream_addr).await {
        warn!("udp: connect '{upstream_addr}': {e}");
        upstream_peer.record_failure();
        return;
    }
    if let Err(e) = up_sock.send(&request).await {
        warn!("udp: send to '{upstream_addr}': {e}");
        upstream_peer.record_failure();
        return;
    }
    metrics::record_stream_bytes(request.len() as u64, 0);

    let mut reply = vec![0u8; UDP_MAX_DATAGRAM];
    let recv = tokio::time::timeout(UDP_REPLY_TIMEOUT, up_sock.recv(&mut reply)).await;
    match recv {
        Ok(Ok(n)) => {
            upstream_peer.record_success();
            metrics::record_stream_bytes(0, n as u64);
            if let Err(e) = client_sock.send_to(&reply[..n], client_addr).await {
                debug!("udp: send back to {client_addr}: {e}");
            }
        }
        Ok(Err(e)) => {
            warn!("udp: recv from '{upstream_addr}': {e}");
            upstream_peer.record_failure();
        }
        Err(_) => {
            debug!(
                "udp: '{upstream_addr}' did not reply within {UDP_REPLY_TIMEOUT:?}"
            );
            upstream_peer.record_failure();
        }
    }
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
