//! Process-level supervisor: spawns HTTP and stream listeners, handles
//! configuration reload (SIGHUP), coordinates graceful shutdown, and
//! pushes hot-reloaded TLS acceptors to running listeners.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::app::{self, Balancer, ListenerCfg};
use crate::{config, http3, server, stream};

/// An HTTP/3 endpoint bound on a UDP port. The QUIC endpoint reads the
/// current `ListenerCfg` snapshot at every accept just like the TCP/TLS
/// side, so vhost changes from `SIGHUP` reload reach new connections.
struct H3Listener {
    addr: SocketAddr,
    /// `Some` if the next reload needs to push a fresh acceptor (TLS
    /// certs swapped). We don't currently swap quinn endpoints in
    /// place — cert hot-reload via `SIGHUP` reaches the TLS HTTP/1+2
    /// listener; for HTTP/3, certs are picked up at process restart.
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

struct HttpListener {
    addr: SocketAddr,
    cfg_tx: watch::Sender<Arc<ListenerCfg>>,
    /// `Some` iff this is a TLS listener. Used to push a freshly rebuilt
    /// `TlsAcceptor` (with possibly new certs and a new SNI resolver) on
    /// `SIGHUP` reload.
    tls_tx: Option<watch::Sender<Arc<tokio_rustls::TlsAcceptor>>>,
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

struct StreamListener {
    addr: SocketAddr,
    /// `true` for UDP listeners. Used by reload to decide whether the
    /// transport can be swapped in place (same protocol) or requires a
    /// fresh listener.
    udp: bool,
    balancer_tx: watch::Sender<Arc<Balancer>>,
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

pub struct Supervisor {
    config_path: PathBuf,
    http_listeners: Vec<HttpListener>,
    stream_listeners: Vec<StreamListener>,
    h3_listeners: Vec<H3Listener>,
}

impl Supervisor {
    pub async fn start(
        config_path: PathBuf,
        runtime: app::Runtime,
    ) -> std::io::Result<Self> {
        let mut http_listeners = Vec::with_capacity(runtime.listeners.len());
        for cfg in runtime.listeners {
            http_listeners.push(spawn_http(cfg).await?);
        }
        let mut stream_listeners = Vec::with_capacity(runtime.stream_servers.len());
        for (addr, balancer, udp) in runtime.stream_servers {
            stream_listeners.push(spawn_stream(addr, balancer, udp).await?);
        }
        let mut h3_listeners: Vec<H3Listener> = Vec::new();
        // Walk the http listeners we just built. For any with h3_tls
        // configured, spawn a sibling QUIC endpoint on the same UDP port.
        for l in &http_listeners {
            // Grab the current ListenerCfg snapshot from the watch sender.
            let cfg_snapshot = l.cfg_tx.subscribe().borrow().clone();
            if cfg_snapshot.h3_tls.is_some() {
                match spawn_h3(cfg_snapshot, l.cfg_tx.subscribe()).await {
                    Ok(h) => h3_listeners.push(h),
                    Err(e) => {
                        error!("failed to start HTTP/3 listener on {}: {e}", l.addr);
                    }
                }
            }
        }
        Ok(Self {
            config_path,
            http_listeners,
            stream_listeners,
            h3_listeners,
        })
    }

    pub fn listener_count(&self) -> usize {
        self.http_listeners.len()
            + self.stream_listeners.len()
            + self.h3_listeners.len()
    }

    pub async fn reload(&mut self) {
        let path = self.config_path.clone();
        info!("reload: re-reading {}", path.display());

        let cfg = match config::load(&path) {
            Ok(c) => c,
            Err(e) => {
                error!("reload: config error: {e} (keeping running configuration)");
                return;
            }
        };
        let runtime = match app::build(&cfg) {
            Ok(r) => r,
            Err(e) => {
                error!("reload: build error: {e} (keeping running configuration)");
                return;
            }
        };

        self.reload_http(runtime.listeners).await;
        self.reload_stream(runtime.stream_servers).await;

        info!(
            "reload: complete; {} http + {} stream listener(s) active",
            self.http_listeners.len(),
            self.stream_listeners.len()
        );
    }

    async fn reload_http(&mut self, new: Vec<ListenerCfg>) {
        let mut wanted: HashMap<SocketAddr, ListenerCfg> =
            new.into_iter().map(|c| (c.addr, c)).collect();

        let mut kept = Vec::with_capacity(self.http_listeners.len());
        for l in self.http_listeners.drain(..) {
            if let Some(new_cfg) = wanted.remove(&l.addr) {
                let new_cfg_arc = Arc::new(new_cfg);
                // Hot-reload TLS acceptor if both sides are TLS.
                if let (Some(tls_tx), Some(server_config)) =
                    (&l.tls_tx, &new_cfg_arc.tls)
                {
                    let acceptor = Arc::new(
                        tokio_rustls::TlsAcceptor::from(server_config.clone()),
                    );
                    if tls_tx.send(acceptor).is_err() {
                        warn!(
                            "reload: TLS listener {} has gone away (could not push cert)",
                            l.addr
                        );
                    } else {
                        let cert_count = new_cfg_arc.tls_paths.len();
                        info!(
                            "reload: TLS listener {} re-loaded {} certificate(s)",
                            l.addr, cert_count
                        );
                    }
                } else if l.tls_tx.is_some() != new_cfg_arc.tls.is_some() {
                    warn!(
                        "reload: TLS toggled on listener {} — restart Elrond for this change",
                        l.addr
                    );
                }
                if l.cfg_tx.send(new_cfg_arc).is_err() {
                    warn!("reload: http listener on {} has gone away", l.addr);
                }
                kept.push(l);
            } else {
                info!("reload: closing http listener on {} (no longer in config)", l.addr);
                let _ = l.shutdown_tx.send(true);
                let join = l.join;
                tokio::spawn(async move {
                    let _ = join.await;
                });
            }
        }
        for (_, cfg) in wanted.into_iter() {
            let addr = cfg.addr;
            match spawn_http(cfg).await {
                Ok(l) => {
                    info!("reload: started new http listener on {addr}");
                    kept.push(l);
                }
                Err(e) => error!("reload: could not bind http {addr}: {e}"),
            }
        }
        self.http_listeners = kept;
    }

    async fn reload_stream(&mut self, new: Vec<(SocketAddr, Arc<Balancer>, bool)>) {
        let mut wanted: HashMap<SocketAddr, (Arc<Balancer>, bool)> = new
            .into_iter()
            .map(|(a, b, u)| (a, (b, u)))
            .collect();

        let mut kept = Vec::with_capacity(self.stream_listeners.len());
        for l in self.stream_listeners.drain(..) {
            if let Some((new_balancer, new_udp)) = wanted.remove(&l.addr) {
                if new_udp != l.udp {
                    warn!(
                        "reload: stream listener {} changed transport (TCP↔UDP) \
                         — restart Elrond to apply",
                        l.addr
                    );
                    kept.push(l);
                    continue;
                }
                if l.balancer_tx.send(new_balancer).is_err() {
                    warn!("reload: stream listener on {} has gone away", l.addr);
                }
                kept.push(l);
            } else {
                info!("reload: closing stream listener on {} (no longer in config)", l.addr);
                let _ = l.shutdown_tx.send(true);
                let join = l.join;
                tokio::spawn(async move {
                    let _ = join.await;
                });
            }
        }
        for (addr, (balancer, udp)) in wanted {
            match spawn_stream(addr, balancer, udp).await {
                Ok(l) => {
                    info!(
                        "reload: started new stream listener on {addr} ({})",
                        if udp { "udp" } else { "tcp" }
                    );
                    kept.push(l);
                }
                Err(e) => error!("reload: could not bind stream {addr}: {e}"),
            }
        }
        self.stream_listeners = kept;
    }

    pub async fn shutdown(self) {
        for l in &self.http_listeners {
            let _ = l.shutdown_tx.send(true);
        }
        for l in &self.stream_listeners {
            let _ = l.shutdown_tx.send(true);
        }
        for l in &self.h3_listeners {
            let _ = l.shutdown_tx.send(true);
        }
        for l in self.http_listeners {
            let _ = l.join.await;
        }
        for l in self.stream_listeners {
            let _ = l.join.await;
        }
        for l in self.h3_listeners {
            let _ = l.join.await;
        }
    }
}

async fn spawn_http(cfg: ListenerCfg) -> std::io::Result<HttpListener> {
    let addr = cfg.addr;
    let listener = TcpListener::bind(addr).await?;

    let tls_handles = cfg.tls.clone();
    let cfg_arc = Arc::new(cfg);
    let (cfg_tx, cfg_rx) = watch::channel(cfg_arc);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let (tls_tx, tls_rx) = if let Some(server_config) = tls_handles {
        let acceptor = Arc::new(tokio_rustls::TlsAcceptor::from(server_config));
        let (tx, rx) = watch::channel(acceptor);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let join = tokio::spawn(async move {
        if let Err(e) =
            server::run(addr, listener, tls_rx, cfg_rx, shutdown_rx).await
        {
            error!("listener on {addr}: {e}");
        }
    });
    Ok(HttpListener {
        addr,
        cfg_tx,
        tls_tx,
        shutdown_tx,
        join,
    })
}

async fn spawn_h3(
    cfg: Arc<ListenerCfg>,
    cfg_rx: watch::Receiver<Arc<ListenerCfg>>,
) -> Result<H3Listener, String> {
    let rustls_cfg = cfg
        .h3_tls
        .clone()
        .ok_or("listener has no h3_tls configured")?;
    let quinn_cfg = http3::quinn_server_config(rustls_cfg)?;
    let addr = cfg.addr;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        if let Err(e) = http3::run(addr, quinn_cfg, cfg_rx, shutdown_rx).await {
            error!("http3 listener on {addr}: {e}");
        }
    });
    Ok(H3Listener {
        addr,
        shutdown_tx,
        join,
    })
}

async fn spawn_stream(
    addr: SocketAddr,
    balancer: Arc<Balancer>,
    udp: bool,
) -> std::io::Result<StreamListener> {
    let (balancer_tx, balancer_rx) = watch::channel(balancer);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let join = if udp {
        let sock = std::sync::Arc::new(tokio::net::UdpSocket::bind(addr).await?);
        tokio::spawn(async move {
            if let Err(e) = stream::run_udp(addr, sock, balancer_rx, shutdown_rx).await {
                error!("udp stream listener on {addr}: {e}");
            }
        })
    } else {
        let listener = TcpListener::bind(addr).await?;
        tokio::spawn(async move {
            if let Err(e) =
                stream::run(addr, listener, balancer_rx, shutdown_rx).await
            {
                error!("stream listener on {addr}: {e}");
            }
        })
    };
    Ok(StreamListener {
        addr,
        udp,
        balancer_tx,
        shutdown_tx,
        join,
    })
}
