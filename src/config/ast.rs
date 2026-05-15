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
    pub stream: Option<Stream>,
}

/// Top-level `stream { … }` block — TCP/UDP proxying. v0.9.0 implements TCP.
#[derive(Debug, Default)]
pub struct Stream {
    pub upstreams: Vec<Upstream>,
    pub servers: Vec<StreamServer>,
}

#[derive(Debug, Default)]
pub struct StreamServer {
    pub listen: Option<SocketAddr>,
    pub proxy_pass: Option<String>,
}

#[derive(Debug, Default)]
pub struct Http {
    pub access_log: Option<String>,
    pub upstreams: Vec<Upstream>,
    pub servers: Vec<Server>,
    /// `proxy_cache_path … keys_zone=NAME:SIZE …;` zone declarations.
    pub cache_zones: Vec<CacheZone>,
}

#[derive(Debug, Clone)]
pub struct CacheZone {
    pub name: String,
    pub max_bytes: usize,
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
    /// `true` if `listen ... ssl;` was specified. Requires `ssl_certificate`
    /// and `ssl_certificate_key`.
    pub tls: bool,
    pub ssl_certificate: Option<String>,
    pub ssl_certificate_key: Option<String>,
    /// `gzip on|off;` at server level. Cascades into locations that don't
    /// override it. `None` is treated as off.
    pub gzip: Option<bool>,
    /// Additional MIME types eligible for on-the-fly gzip.
    pub gzip_types: Vec<String>,
    /// Server-level `add_header` directives. Cascade into every location;
    /// location-level `add_header` is applied last and wins on conflicts.
    pub add_headers: Vec<(String, Template)>,
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
    /// Per-location `gzip on|off;` override. `None` falls back to the
    /// server-level setting.
    pub gzip: Option<bool>,
    /// `autoindex on|off;` — render a directory listing when the path
    /// resolves to a directory and no `index.html` is present.
    pub autoindex: bool,
    /// `auth_basic <realm>;` — HTTP Basic auth realm. Disabled when empty.
    pub auth_basic_realm: Option<String>,
    /// `auth_basic_user_file <path>;` — htpasswd-style file (bcrypt only).
    pub auth_basic_user_file: Option<String>,
    /// `proxy_cache <zone_name>;` — enables caching for this location.
    pub proxy_cache: Option<String>,
    /// `proxy_cache_key <template>;` — defaults to
    /// `$scheme$proxy_host$request_uri` when caching is enabled.
    pub proxy_cache_key: Option<Template>,
    /// One entry per `proxy_cache_valid` directive: `(status codes, ttl)`.
    /// An empty `codes` list means "any status".
    pub proxy_cache_valid: Vec<(Vec<u16>, Duration)>,
}

#[derive(Debug, Clone)]
pub enum Action {
    Return { status: u16, body: Template },
    ProxyPass { target: String },
    Root { dir: String },
    Alias { dir: String },
    /// Expose Prometheus-format metrics at this location.
    Metrics,
}
