//! Runtime model: the validated [`Config`] lowered into ready-to-serve state.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use hyper::header::HeaderName;

use crate::config::{Action, Config, LocationKind};
use crate::template::Template;

pub type SharedState = Arc<ServerState>;
pub type HeaderList = Arc<Vec<(HeaderName, Template)>>;

pub struct Runtime {
    pub servers: Vec<(SocketAddr, SharedState)>,
}

pub struct ServerState {
    pub server_name: Option<String>,
    exact_locs: Vec<LocationRt>,
    prefix_locs: Vec<LocationRt>,
}

impl ServerState {
    /// Nginx-style routing: exact matches first, then longest prefix.
    pub fn route(&self, path: &str) -> Option<&LocationRt> {
        for l in &self.exact_locs {
            if l.path == path {
                return Some(l);
            }
        }
        for l in &self.prefix_locs {
            if l.path == "/" || path.starts_with(&l.path) {
                return Some(l);
            }
        }
        None
    }
}

pub struct LocationRt {
    pub path: String,
    pub action: ActionRt,
    pub add_headers: HeaderList,
}

pub enum ActionRt {
    Return {
        status: u16,
        body: Template,
    },
    Proxy {
        balancer: Arc<Balancer>,
        set_headers: HeaderList,
    },
    Static {
        root: PathBuf,
        kind: StaticKind,
    },
}

pub enum StaticKind {
    /// Nginx `root`: filesystem path = root + full URI path.
    Root,
    /// Nginx `alias`: filesystem path = alias + (URI path - location prefix).
    Alias { prefix: String },
}

pub struct Balancer {
    pub name: String,
    addrs: Vec<String>,
    counter: AtomicUsize,
}

impl Balancer {
    pub fn pick(&self) -> &str {
        let i = self.counter.fetch_add(1, Ordering::Relaxed);
        &self.addrs[i % self.addrs.len()]
    }
}

pub fn build(cfg: &Config) -> Result<Runtime, String> {
    let http = cfg
        .http
        .as_ref()
        .ok_or("config has no 'http' block; nothing to serve")?;

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

        let mut exact_locs: Vec<LocationRt> = Vec::new();
        let mut prefix_locs: Vec<LocationRt> = Vec::new();

        for loc in &s.locations {
            let action = match &loc.action {
                Action::Return { status, body } => ActionRt::Return {
                    status: *status,
                    body: body.clone(),
                },
                Action::Root { dir } => ActionRt::Static {
                    root: PathBuf::from(dir),
                    kind: StaticKind::Root,
                },
                Action::Alias { dir } => ActionRt::Static {
                    root: PathBuf::from(dir),
                    kind: StaticKind::Alias {
                        prefix: loc.path.clone(),
                    },
                },
                Action::ProxyPass { target } => ActionRt::Proxy {
                    balancer: resolve_proxy(target, &balancers),
                    set_headers: Arc::new(compile_headers(&loc.set_headers)?),
                },
            };
            let location_rt = LocationRt {
                path: loc.path.clone(),
                action,
                add_headers: Arc::new(compile_headers(&loc.add_headers)?),
            };
            if loc.kind == LocationKind::Exact {
                exact_locs.push(location_rt);
            } else {
                prefix_locs.push(location_rt);
            }
        }
        prefix_locs.sort_by(|a, b| b.path.len().cmp(&a.path.len()));

        servers.push((
            addr,
            Arc::new(ServerState {
                server_name: s.server_name.clone(),
                exact_locs,
                prefix_locs,
            }),
        ));
    }

    if servers.is_empty() {
        return Err("config has no 'server' blocks".into());
    }
    Ok(Runtime { servers })
}

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

fn compile_headers(
    items: &[(String, Template)],
) -> Result<Vec<(HeaderName, Template)>, String> {
    items
        .iter()
        .map(|(name, tmpl)| {
            HeaderName::try_from(name.as_str())
                .map(|h| (h, tmpl.clone()))
                .map_err(|e| format!("invalid header name '{name}': {e}"))
        })
        .collect()
}
