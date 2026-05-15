//! Process-level supervisor: spawns HTTP and stream listeners, handles
//! configuration reload (SIGHUP), and coordinates graceful shutdown.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::app::{self, Balancer, ServerState};
use crate::{config, server, stream};

struct HttpListener {
    addr: SocketAddr,
    state_tx: watch::Sender<Arc<ServerState>>,
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
    tls: bool,
}

struct StreamListener {
    addr: SocketAddr,
    balancer_tx: watch::Sender<Arc<Balancer>>,
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

pub struct Supervisor {
    config_path: PathBuf,
    http_listeners: Vec<HttpListener>,
    stream_listeners: Vec<StreamListener>,
}

impl Supervisor {
    pub async fn start(
        config_path: PathBuf,
        runtime: app::Runtime,
    ) -> std::io::Result<Self> {
        let mut http_listeners = Vec::with_capacity(runtime.servers.len());
        for (addr, state, tls) in runtime.servers {
            http_listeners.push(spawn_http(addr, state, tls).await?);
        }
        let mut stream_listeners = Vec::with_capacity(runtime.stream_servers.len());
        for (addr, balancer) in runtime.stream_servers {
            stream_listeners.push(spawn_stream(addr, balancer).await?);
        }
        Ok(Self {
            config_path,
            http_listeners,
            stream_listeners,
        })
    }

    pub fn listener_count(&self) -> usize {
        self.http_listeners.len() + self.stream_listeners.len()
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

        self.reload_http(runtime.servers).await;
        self.reload_stream(runtime.stream_servers).await;

        info!(
            "reload: complete; {} http + {} stream listener(s) active",
            self.http_listeners.len(),
            self.stream_listeners.len()
        );
    }

    async fn reload_http(
        &mut self,
        new: Vec<(SocketAddr, Arc<ServerState>, Option<Arc<rustls::ServerConfig>>)>,
    ) {
        let mut wanted: HashMap<
            SocketAddr,
            (Arc<ServerState>, Option<Arc<rustls::ServerConfig>>),
        > = new.into_iter().map(|(a, s, t)| (a, (s, t))).collect();

        let mut kept = Vec::with_capacity(self.http_listeners.len());
        for l in self.http_listeners.drain(..) {
            if let Some((new_state, new_tls)) = wanted.remove(&l.addr) {
                if l.tls && new_tls.is_some() {
                    info!(
                        "reload: TLS listener {} keeps its certificate \
                         (cert reload comes later)",
                        l.addr
                    );
                }
                if l.state_tx.send(new_state).is_err() {
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
        for (addr, (state, tls)) in wanted {
            match spawn_http(addr, state, tls).await {
                Ok(l) => {
                    info!("reload: started new http listener on {addr}");
                    kept.push(l);
                }
                Err(e) => error!("reload: could not bind http {addr}: {e}"),
            }
        }
        self.http_listeners = kept;
    }

    async fn reload_stream(&mut self, new: Vec<(SocketAddr, Arc<Balancer>)>) {
        let mut wanted: HashMap<SocketAddr, Arc<Balancer>> = new.into_iter().collect();

        let mut kept = Vec::with_capacity(self.stream_listeners.len());
        for l in self.stream_listeners.drain(..) {
            if let Some(new_balancer) = wanted.remove(&l.addr) {
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
        for (addr, balancer) in wanted {
            match spawn_stream(addr, balancer).await {
                Ok(l) => {
                    info!("reload: started new stream listener on {addr}");
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
        for l in self.http_listeners {
            let _ = l.join.await;
        }
        for l in self.stream_listeners {
            let _ = l.join.await;
        }
    }
}

async fn spawn_http(
    addr: SocketAddr,
    state: Arc<ServerState>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> std::io::Result<HttpListener> {
    let listener = TcpListener::bind(addr).await?;
    let (state_tx, state_rx) = watch::channel(state);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let tls_acceptor = tls.as_ref().map(|c| tokio_rustls::TlsAcceptor::from(c.clone()));
    let is_tls = tls.is_some();
    let join = tokio::spawn(async move {
        if let Err(e) =
            server::run(addr, listener, tls_acceptor, state_rx, shutdown_rx).await
        {
            error!("listener on {addr}: {e}");
        }
    });
    Ok(HttpListener {
        addr,
        state_tx,
        shutdown_tx,
        join,
        tls: is_tls,
    })
}

async fn spawn_stream(
    addr: SocketAddr,
    balancer: Arc<Balancer>,
) -> std::io::Result<StreamListener> {
    let listener = TcpListener::bind(addr).await?;
    let (balancer_tx, balancer_rx) = watch::channel(balancer);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        if let Err(e) = stream::run(addr, listener, balancer_rx, shutdown_rx).await {
            error!("stream listener on {addr}: {e}");
        }
    });
    Ok(StreamListener {
        addr,
        balancer_tx,
        shutdown_tx,
        join,
    })
}
