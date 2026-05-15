//! Typed configuration model produced by [`crate::config::build`].

use std::net::SocketAddr;

use crate::template::Template;

#[derive(Debug, Default)]
pub struct Config {
    pub worker_processes: Option<String>,
    pub pid: Option<String>,
    pub error_log: Option<String>,
    pub http: Option<Http>,
}

#[derive(Debug, Default)]
pub struct Http {
    pub access_log: Option<String>,
    pub upstreams: Vec<Upstream>,
    pub servers: Vec<Server>,
}

#[derive(Debug)]
pub struct Upstream {
    pub name: String,
    pub servers: Vec<UpstreamServer>,
}

#[derive(Debug)]
pub struct UpstreamServer {
    pub addr: String,
    pub weight: u32,
}

#[derive(Debug, Default)]
pub struct Server {
    pub listen: Option<SocketAddr>,
    pub server_name: Option<String>,
    /// Server-level `root`. Cascades into locations with no content directive.
    pub root: Option<String>,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationKind {
    /// `location = /path` — exact equality, highest priority.
    Exact,
    /// `location /path` — longest-prefix-wins.
    Prefix,
}

#[derive(Debug)]
pub struct Location {
    pub kind: LocationKind,
    pub path: String,
    pub action: Action,
    /// Headers to set on the upstream request (`proxy_set_header`).
    pub set_headers: Vec<(String, Template)>,
    /// Headers to set on the outgoing response (`add_header`).
    pub add_headers: Vec<(String, Template)>,
}

#[derive(Debug, Clone)]
pub enum Action {
    Return { status: u16, body: Template },
    ProxyPass { target: String },
    Root { dir: String },
    Alias { dir: String },
}
