//! Typed configuration model produced by [`crate::config::build`].

use std::net::SocketAddr;
use std::time::Duration;

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
    pub method: LbMethod,
    pub servers: Vec<UpstreamServer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbMethod {
    /// Weighted round-robin. The default.
    RoundRobin,
    /// Send each request to the peer with the fewest in-flight requests,
    /// adjusted by weight.
    LeastConn,
    /// Hash the client IP; requests from one client stick to one peer.
    IpHash,
}

impl Default for LbMethod {
    fn default() -> Self {
        LbMethod::RoundRobin
    }
}

#[derive(Debug)]
pub struct UpstreamServer {
    pub addr: String,
    pub weight: u32,
    pub max_fails: u32,
    pub fail_timeout: Duration,
    pub backup: bool,
    pub down: bool,
}

impl Default for UpstreamServer {
    fn default() -> Self {
        UpstreamServer {
            addr: String::new(),
            weight: 1,
            // Nginx defaults: max_fails=1, fail_timeout=10s.
            max_fails: 1,
            fail_timeout: Duration::from_secs(10),
            backup: false,
            down: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Server {
    pub listen: Option<SocketAddr>,
    pub server_name: Option<String>,
    pub root: Option<String>,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationKind {
    Exact,
    Prefix,
}

#[derive(Debug)]
pub struct Location {
    pub kind: LocationKind,
    pub path: String,
    pub action: Action,
    pub set_headers: Vec<(String, Template)>,
    pub add_headers: Vec<(String, Template)>,
    /// `expires <duration>;` — applied to outgoing responses as both
    /// `Cache-Control: max-age=N` and `Expires: <date>`.
    pub expires: Option<Duration>,
}

#[derive(Debug, Clone)]
pub enum Action {
    Return { status: u16, body: Template },
    ProxyPass { target: String },
    Root { dir: String },
    Alias { dir: String },
}
