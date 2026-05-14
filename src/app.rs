//! Runtime model: the validated [`Config`] lowered into ready-to-serve state.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::{Action, Config};

/// Per-listener runtime state, shared across all connections on that listener.
pub type SharedState = Arc<ServerState>;

/// Everything needed to serve: one entry per `server` block.
pub struct Runtime {
    pub servers: Vec<(SocketAddr, SharedState)>,
}

pub struct ServerState {
    pub server_name: Option<String>,
    /// Locations sorted by prefix length, longest first.
    locations: Vec<LocationRt>,
}

impl ServerState {
    /// Longest-prefix-wins routing. `/` acts as a catch-all.
    pub fn route(&self, path: &str) -> Option<&ActionRt> {
        self.locations
            .iter()
            .find(|l| l.prefix == "/" || path.starts_with(&l.prefix))
            .map(|l| &l.action)
    }
}

struct LocationRt {
    prefix: String,
    action: ActionRt,
}

/// Runtime form of a location's content directive.
pub enum ActionRt {
    Return { status: u16, body: String },
    Proxy(Arc<Balancer>),
    Static { root: PathBuf },
}

/// Round-robin selector over a set of upstream addresses. Weighting is encoded
/// by repeating an address in `addrs` `weight` times.
pub struct Balancer {
    pub name: String,
    addrs: Vec<String>,
    counter: AtomicUsize,
}

impl Balancer {
    /// Pick the next upstream address in round-robin order.
    pub fn pick(&self) -> &str {
        let i = self.counter.fetch_add(1, Ordering::Relaxed);
        &self.addrs[i % self.addrs.len()]
    }
}

/// Lower a validated [`Config`] into a [`Runtime`].
pub fn build(cfg: &Config) -> Result<Runtime, String> {
    let http = cfg
        .http
        .as_ref()
        .ok_or("config has no 'http' block; nothing to serve")?;

    // Pre-build a balancer for every named upstream.
    let mut balancers: HashMap<String, Arc<Balancer>> = HashMap::new();
    for up in &http.upstreams {
        let mut addrs = Vec::new();
        for s in &up.servers {
            for _ in 0..s.weight.max(1) {
                addrs.push(s.addr.clone());
            }
        }
        balancers.insert(
            up.name.clone(),
            Arc::new(Balancer {
                name: up.name.clone(),
                addrs,
                counter: AtomicUsize::new(0),
            }),
        );
    }

    let mut servers = Vec::new();
    for s in &http.servers {
        let addr = s
            .listen
            .ok_or("a 'server' block is missing its 'listen' directive")?;

        let mut locations = Vec::with_capacity(s.locations.len());
        for loc in &s.locations {
            let action = match &loc.action {
                Action::Return { status, body } => ActionRt::Return {
                    status: *status,
                    body: body.clone(),
                },
                Action::Root { dir } => ActionRt::Static {
                    root: PathBuf::from(dir),
                },
                Action::ProxyPass { target } => {
                    ActionRt::Proxy(resolve_proxy(target, &balancers))
                }
            };
            locations.push(LocationRt {
                prefix: loc.path.clone(),
                action,
            });
        }
        // Longest prefix first so `route` can return on the first match.
        locations.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));

        servers.push((
            addr,
            Arc::new(ServerState {
                server_name: s.server_name.clone(),
                locations,
            }),
        ));
    }

    if servers.is_empty() {
        return Err("config has no 'server' blocks".into());
    }
    Ok(Runtime { servers })
}

/// Resolve a `proxy_pass` target to a balancer: either a named upstream or a
/// single direct address.
fn resolve_proxy(
    target: &str,
    balancers: &HashMap<String, Arc<Balancer>>,
) -> Arc<Balancer> {
    let host = target
        .strip_prefix("http://")
        .unwrap_or(target)
        .trim_end_matches('/');

    if let Some(b) = balancers.get(host) {
        return b.clone();
    }
    Arc::new(Balancer {
        name: host.to_string(),
        addrs: vec![host.to_string()],
        counter: AtomicUsize::new(0),
    })
}
