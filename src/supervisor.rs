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

use crate::app::{self, Balancer, ServerState, TlsHandles};
use crate::{config, server, stream};

struct HttpListener {
    addr: SocketAddr,
    state_tx: watch::Sender<Arc<ServerState>>,
    /// Present iff the listener terminates TLS. Used to push a freshly
    /// rebuilt `TlsAcceptor` on `SIGHUP` reload (cert hot-reload).
    tls_tx: Option<watch::Sender<Arc<tokio_rustls::TlsAcceptor>>>,
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
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
        for (addr, state, tls_handles) in runtime.servers {
            http_listeners.push(spawn_http(addr, state, tls_handles).await?);
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
        new: Vec<(SocketAddr, Arc<ServerState>, Option<TlsHandles>)>,
    ) {
        let mut wanted: HashMap<
            SocketAddr,
            (Arc<ServerState>, Option<TlsHandles>),
        > = new.into_iter().map(|(a, s, t)| (a, (s, t))).collect();

        let mut kept = Vec::with_capacity(self.http_listeners.len());
        for l in self.http_listeners.drain(..) {
            if let Some((new_state, new_tls)) = wanted.remove(&l.addr) {
                // Hot-reload certificate when the listener stays TLS.
                if let (Some(tls_tx), Some(handles)) = (&l.tls_tx, &new_tls) {
                    let acceptor = Arc::new(
                        tokio_rustls::TlsAcceptor::from(handles.server_config.clone()),
                    );
                    if tls_tx.send(acceptor).is_err() {
                        warn!(
                            "reload: TLS listener {} has gone away (could not push cert)",
                            l.addr
                        );
                    } else {
                        info!(
                            "reload: TLS listener {} re-loaded cert from {} / {}",
                            l.addr,
                            handles.cert_path.display(),
                            handles.key_path.display()
                        );
                    }
                } else if l.tls_tx.is_some() != new_tls.is_some() {
                    // Toggling TLS on/off in place is not supported — would
                    // require respawning the listener with a different
                    // accept-path. Log loudly so it's not silent.
                    warn!(
                        "reload: TLS toggled on listener {} — restart Elrond for this change",
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
    tls_handles: Option<TlsHandles>,
) -> std::io::Result<HttpListener> {
    let listener = TcpListener::bind(addr).await?;
    let (state_tx, state_rx) = watch::channel(state);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let (tls_tx, tls_rx) = if let Some(h) = &tls_handles {
        let acceptor = Arc::new(tokio_rustls::TlsAcceptor::from(h.server_config.clone()));
        let (tx, rx) = watch::channel(acceptor);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let join = tokio::spawn(async move {
        if let Err(e) =
            server::run(addr, listener, tls_rx, state_rx, shutdown_rx).await
        {
            error!("listener on {addr}: {e}");
        }
    });
    Ok(HttpListener {
        addr,
        state_tx,
        tls_tx,
        shutdown_tx,
        join,
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
