//! Typed configuration model produced by [`crate::config::build`].
//!
//! This is the validated, structured form of a configuration file — the
//! runtime in [`crate::app`] is built directly from it.

use std::net::SocketAddr;

#[derive(Debug, Default)]
pub struct Config {
    /// `worker_processes` — stored for reporting; v0.1.0 is single-process.
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
    pub locations: Vec<Location>,
}

#[derive(Debug)]
pub struct Location {
    /// Prefix to match against the request path.
    pub path: String,
    pub action: Action,
}

/// The content-producing directive of a `location` block.
#[derive(Debug, Clone)]
pub enum Action {
    Return { status: u16, body: String },
    ProxyPass { target: String },
    Root { dir: String },
}
