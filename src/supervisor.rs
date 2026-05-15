//! Process-level supervisor: spawns one task per `listen` directive,
//! handles configuration reload (SIGHUP), and coordinates graceful shutdown.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::app::{self, ServerState};
use crate::config;
use crate::server;

/// A single running listener, plus the channels used to update its state or
/// ask it to drain.
struct Listener {
    addr: SocketAddr,
    state_tx: watch::Sender<Arc<ServerState>>,
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

pub struct Supervisor {
    config_path: PathBuf,
    listeners: Vec<Listener>,
}

impl Supervisor {
    /// Start listeners for every `server` block in the loaded runtime.
    pub async fn start(
        config_path: PathBuf,
        runtime: app::Runtime,
    ) -> std::io::Result<Self> {
        let mut listeners = Vec::with_capacity(runtime.servers.len());
        for (addr, state) in runtime.servers {
            listeners.push(spawn_listener(addr, state).await?);
        }
        Ok(Self {
            config_path,
            listeners,
        })
    }

    /// Re-read the configuration file. On any failure the running listeners
    /// keep their old configuration, exactly mirroring Nginx's HUP behavior.
    ///
    /// On success:
    /// - Listeners whose `listen` address still exists get the new state
    ///   pushed to them via `state_tx`. In-flight connections finish on the
    ///   old state; new connections accept on the new state.
    /// - Listeners whose `listen` address was removed are signaled to drain.
    /// - Brand-new `listen` addresses get fresh listeners.
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

        let mut wanted: HashMap<SocketAddr, Arc<ServerState>> =
            runtime.servers.into_iter().collect();

        let mut kept = Vec::with_capacity(self.listeners.len());
        for l in self.listeners.drain(..) {
            if let Some(new_state) = wanted.remove(&l.addr) {
                if l.state_tx.send(new_state).is_err() {
                    warn!("reload: listener on {} has gone away", l.addr);
                }
                kept.push(l);
            } else {
                info!("reload: closing listener on {} (no longer in config)", l.addr);
                let _ = l.shutdown_tx.send(true);
                let join = l.join;
                tokio::spawn(async move {
                    let _ = join.await;
                });
            }
        }

        for (addr, state) in wanted {
            match spawn_listener(addr, state).await {
                Ok(l) => {
                    info!("reload: started new listener on {addr}");
                    kept.push(l);
                }
                Err(e) => {
                    error!("reload: could not bind {addr}: {e}");
                }
            }
        }

        self.listeners = kept;
        info!("reload: complete; {} listener(s) active", self.listeners.len());
    }

    pub fn listener_count(&self) -> usize {
        self.listeners.len()
    }

    /// Signal every listener to drain, then wait for them.
    pub async fn shutdown(self) {
        for l in &self.listeners {
            let _ = l.shutdown_tx.send(true);
        }
        for l in self.listeners {
            let _ = l.join.await;
        }
    }
}

async fn spawn_listener(
    addr: SocketAddr,
    state: Arc<ServerState>,
) -> std::io::Result<Listener> {
    let listener = TcpListener::bind(addr).await?;
    let (state_tx, state_rx) = watch::channel(state);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        if let Err(e) = server::run(addr, listener, state_rx, shutdown_rx).await {
            error!("listener on {addr}: {e}");
        }
    });
    Ok(Listener {
        addr,
        state_tx,
        shutdown_tx,
        join,
    })
}
