//! Runtime model: the validated [`Config`] lowered into ready-to-serve state.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyper::header::HeaderName;

use crate::config::{Action, Config, LbMethod, LocationKind};
use crate::request_ctx::RequestCtx;
use crate::template::Template;

pub type SharedState = Arc<ServerState>;
pub type HeaderList = Arc<Vec<(HeaderName, Template)>>;

pub struct Runtime {
    /// One entry per **listen address**. Multiple `server` blocks on the
    /// same address are grouped into one [`ListenerCfg`] and routed by
    /// `Host` header.
    pub listeners: Vec<ListenerCfg>,
    /// One entry per `stream` `server` block. The `bool` is `true` for
    /// UDP listeners (`listen ... udp;`), `false` for TCP.
    pub stream_servers: Vec<(SocketAddr, Arc<Balancer>, bool)>,
}

/// All the state a single HTTP listener needs.
pub struct ListenerCfg {
    pub addr: SocketAddr,
    /// All virtual hosts on this listener. The first one is the default
    /// (served when no `Host` header matches).
    pub vhosts: Vec<VirtualHost>,
    /// `Some(_)` iff this is a TLS listener. The `ServerConfig` carries
    /// the multi-cert SNI resolver.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// `Some(_)` iff at least one vhost on this address opted into
    /// HTTP/3 (via `listen ... http3;`). Built with ALPN `h3` and
    /// TLS 1.3 only. The supervisor uses it to spawn a QUIC endpoint
    /// on the same UDP port.
    pub h3_tls: Option<Arc<rustls::ServerConfig>>,
    /// Cert / key paths so the supervisor can re-read on `SIGHUP`. One
    /// entry per cert configured on this listener.
    pub tls_paths: Vec<crate::tls::CertEntry>,
}

/// One virtual host on a listener.
pub struct VirtualHost {
    pub server_name: Option<String>,
    pub state: SharedState,
}

impl ListenerCfg {
    /// Select the right `ServerState` for an incoming request based on the
    /// `Host` header (port stripped, case-insensitive). Falls back to the
    /// first vhost.
    pub fn pick_state(&self, host_header: Option<&str>) -> &SharedState {
        if let Some(h) = host_header {
            let host = h.split(':').next().unwrap_or(h).to_ascii_lowercase();
            for v in &self.vhosts {
                if let Some(name) = &v.server_name {
                    if name.to_ascii_lowercase() == host {
                        return &v.state;
                    }
                }
            }
        }
        &self.vhosts[0].state
    }
}

pub struct ServerState {
    pub server_name: Option<String>,
    /// `"http"` or `"https"` — used by the variable engine for `$scheme`.
    pub scheme: &'static str,
    /// Effective gzip-enabled state for this server.
    pub gzip: bool,
    pub gzip_types: Vec<String>,
    /// `gzip_min_length` for this server (default 20 bytes).
    pub gzip_min_length: usize,
    /// `map` declarations, evaluated once per request before any
    /// location-level templates run.
    pub maps: Arc<Vec<crate::config::MapDecl>>,
    /// `client_max_body_size`: 0 means unlimited. Defaults to 1 MiB.
    pub client_max_body_size: usize,
    exact_locs: Vec<LocationRt>,
    regex_locs: Vec<(regex::Regex, LocationRt)>,
    prefix_locs: Vec<LocationRt>,
}

impl ServerState {
    /// Nginx-style routing precedence (with a v0.29.0 caveat):
    ///   1. Exact match (`=`).
    ///   2. Regex match (`~`, `~*`) — first match in declaration order.
    ///   3. Longest prefix.
    ///
    /// Note: `^~` is currently parsed but treated as a plain prefix
    /// (does not block regex consideration). That deviates from Nginx
    /// for configs that intermix `^~` and regex `location`s.
    pub fn route(&self, path: &str) -> Option<&LocationRt> {
        for l in &self.exact_locs {
            if l.path == path {
                return Some(l);
            }
        }
        for (re, l) in &self.regex_locs {
            if re.is_match(path) {
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
    /// `expires` value, applied to every response from this location.
    pub expires: Option<Duration>,
    /// Per-location gzip override. `None` -> use the server-level default.
    pub gzip: Option<bool>,
    /// `autoindex on;` — render a directory listing when the path is a
    /// directory and `index.html` is absent.
    pub autoindex: bool,
    /// HTTP Basic auth, loaded at config-build time.
    pub auth: Option<Arc<crate::auth::AuthBasic>>,
    /// `limit_req` enforcement for this location.
    pub limit_req: Option<crate::limit::LimitReqApply>,
    /// `limit_conn` enforcement for this location.
    pub limit_conn: Option<crate::limit::LimitConnApply>,
    /// `allow` / `deny` rules in declaration order.
    pub access_rules: Arc<Vec<crate::access::AccessRule>>,
    /// `proxy_connect_timeout` for this location. Parsed and stored;
    /// the connect timeout is currently set process-wide on the proxy
    /// client. Per-location override is a follow-up.
    #[allow(dead_code)]
    pub proxy_connect_timeout: Option<Duration>,
    /// `proxy_read_timeout` for the upstream exchange. `None` → process
    /// default (60s).
    pub proxy_read_timeout: Option<Duration>,
    /// `auth_request <url>;` — delegate authorization to an HTTP service.
    pub auth_request: Option<Template>,
    /// `mirror <url>;` — fire-and-forget shadow requests.
    pub mirrors: Arc<Vec<Template>>,
    /// `true` to verify the upstream's TLS certificate (default);
    /// `false` accepts any cert (test / staging escape hatch).
    pub proxy_ssl_verify: bool,
    /// `Some(client)` when this location has mTLS configured. The
    /// client is built once at config-build time per unique
    /// (cert, key, verify) tuple and shared across requests.
    pub proxy_client: Option<Arc<crate::proxy::ProxyClient>>,
}

/// Process defaults applied when a directive does not specify otherwise.
pub const DEFAULT_PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PROXY_READ_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_CLIENT_MAX_BODY_SIZE: usize = 1024 * 1024;

pub enum ActionRt {
    Return {
        status: u16,
        body: Template,
    },
    Proxy {
        target: ProxyTarget,
        set_headers: HeaderList,
        cache: Option<ProxyCache>,
    },
    Static {
        root: PathBuf,
        kind: StaticKind,
    },
    /// Render Prometheus metrics inline.
    Metrics,
    /// `try_files` — try each candidate in order, fall back to the last
    /// entry (which may itself be a path or a `=NNN` status).
    TryFiles {
        root: PathBuf,
        entries: Vec<TryFilesEntryRt>,
    },
}

#[derive(Clone)]
pub enum TryFilesEntryRt {
    Path(Template),
    Status(u16),
}

/// Per-location proxy-cache configuration, resolved at config-build time.
#[derive(Clone)]
pub struct ProxyCache {
    pub store: Arc<crate::cache::CacheStore>,
    pub key_template: Template,
    /// `(status codes (empty = any), ttl)` pairs.
    pub valid_rules: Vec<(Vec<u16>, Duration)>,
}

/// What `proxy_pass` resolves to at request time. Fixed targets are
/// captured once at config-build (the common case). Dynamic targets —
/// any `proxy_pass http://$something` — are rendered per request, looked
/// up first in the named-upstream map, and synthesized + cached on first
/// sight as a single-peer balancer for direct-address values.
#[derive(Clone)]
pub enum ProxyTarget {
    Fixed(Arc<Balancer>),
    Dynamic {
        template: Template,
        balancers: Arc<HashMap<String, Arc<Balancer>>>,
        /// Memoizes ephemeral balancers built for direct addresses so the
        /// state (in-flight counters, passive-health cooldown) survives
        /// across requests resolving to the same target.
        ephemeral: Arc<std::sync::RwLock<HashMap<String, Arc<Balancer>>>>,
    },
}

impl ProxyTarget {
    /// Resolve to a concrete balancer for this request. `None` is a hard
    /// failure (empty / unparseable template) and the caller should return
    /// a `502`.
    pub fn resolve(&self, ctx: &crate::request_ctx::RequestCtx<'_>) -> Option<Arc<Balancer>> {
        match self {
            ProxyTarget::Fixed(b) => Some(b.clone()),
            ProxyTarget::Dynamic {
                template,
                balancers,
                ephemeral,
            } => {
                let rendered = template.render(ctx);
                let host = rendered
                    .strip_prefix("http://")
                    .unwrap_or(&rendered)
                    .trim_end_matches('/');
                if host.is_empty() {
                    return None;
                }
                // Try the named-upstream map (which stores keys without a
                // scheme prefix). Strip a leading scheme for lookup.
                let bare = host
                    .strip_prefix("http://")
                    .or_else(|| host.strip_prefix("https://"))
                    .unwrap_or(host)
                    .trim_end_matches('/');
                if let Some(b) = balancers.get(bare) {
                    return Some(b.clone());
                }
                // Already-cached ephemeral?
                if let Some(b) = ephemeral
                    .read()
                    .ok()
                    .and_then(|m| m.get(host).cloned())
                {
                    return Some(b);
                }
                // Build a fresh single-peer balancer for this direct address
                // and cache it so subsequent requests inherit the same
                // in-flight / passive-health state.
                // Dynamic targets may carry their own scheme prefix.
                let (scheme, addr) = if let Some(s) = host.strip_prefix("https://") {
                    ("https", s.trim_end_matches('/'))
                } else if let Some(s) = host.strip_prefix("http://") {
                    ("http", s.trim_end_matches('/'))
                } else {
                    ("http", host)
                };
                let new_balancer = Arc::new(Balancer {
                    name: addr.to_string(),
                    method: LbMethod::RoundRobin,
                    peers: vec![Arc::new(Peer {
                        addr: addr.to_string(),
                        weight: 1,
                        max_fails: 1,
                        fail_timeout: Duration::from_secs(10),
                        backup: false,
                        down: false,
                        in_flight: AtomicU32::new(0),
                        consecutive_failures: AtomicU32::new(0),
                        failed_until_ms: AtomicU64::new(0),
                    })],
                    scheme,
                    rr_counter: AtomicUsize::new(0),
                });
                if let Ok(mut w) = ephemeral.write() {
                    w.insert(host.to_string(), new_balancer.clone());
                }
                Some(new_balancer)
            }
        }
    }
}

pub enum StaticKind {
    Root,
    Alias { prefix: String },
}

/// A single upstream endpoint, with both its static configuration and its
/// live health state.
pub struct Peer {
    pub addr: String,
    pub weight: u32,
    pub max_fails: u32,
    pub fail_timeout: Duration,
    pub backup: bool,
    pub down: bool,
    /// Currently in-flight requests dispatched to this peer.
    in_flight: AtomicU32,
    /// Consecutive failures since the last success. Reset on success.
    consecutive_failures: AtomicU32,
    /// Unix-epoch millis until which the peer is considered failed; `0`
    /// means healthy.
    failed_until_ms: AtomicU64,
}

impl Peer {
    /// True if this peer should be considered for picking right now.
    pub fn is_available(&self, now_ms: u64) -> bool {
        if self.down {
            return false;
        }
        let until = self.failed_until_ms.load(Ordering::Relaxed);
        until == 0 || now_ms >= until
    }

    /// Record a successful exchange — clears any pending failure cooldown.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.failed_until_ms.store(0, Ordering::Relaxed);
    }

    /// Record a failure. When consecutive failures reach `max_fails`, the
    /// peer is taken out of rotation for `fail_timeout`.
    pub fn record_failure(&self) {
        let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= self.max_fails.max(1) {
            let until = now_ms() + self.fail_timeout.as_millis() as u64;
            self.failed_until_ms.store(until, Ordering::Relaxed);
        }
    }

    /// Acquire an in-flight counter for the duration of one request. The
    /// returned guard decrements on drop.
    pub fn enter(self: &Arc<Self>) -> InflightGuard {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InflightGuard {
            peer: self.clone(),
        }
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }
}

/// RAII counter that drops the in-flight count when the request completes.
pub struct InflightGuard {
    peer: Arc<Peer>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.peer.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Balancer {
    pub name: String,
    pub method: LbMethod,
    pub peers: Vec<Arc<Peer>>,
    /// `"http"` or `"https"` — derived from the `proxy_pass` URL scheme.
    pub scheme: &'static str,
    rr_counter: AtomicUsize,
}

impl Balancer {
    /// HTTP entry point: pick using the request context (for `ip_hash`).
    pub fn pick(&self, ctx: &RequestCtx<'_>) -> Option<Arc<Peer>> {
        self.pick_inner(ctx.peer.ip(), &[])
    }

    /// HTTP retry entry point: pick excluding peers that already failed for
    /// this request.
    pub fn pick_excluding(
        &self,
        ctx: &RequestCtx<'_>,
        exclude: &[String],
    ) -> Option<Arc<Peer>> {
        self.pick_inner(ctx.peer.ip(), exclude)
    }

    /// Stream entry point: pick using only the client's IP address (no HTTP
    /// request context exists).
    pub fn pick_for_addr(&self, client_ip: IpAddr) -> Option<Arc<Peer>> {
        self.pick_inner(client_ip, &[])
    }

    fn pick_inner(
        &self,
        client_ip: IpAddr,
        exclude: &[String],
    ) -> Option<Arc<Peer>> {
        let now = now_ms();

        let primaries: Vec<&Arc<Peer>> = self
            .peers
            .iter()
            .filter(|p| {
                !p.backup && p.is_available(now) && !exclude.iter().any(|x| x == &p.addr)
            })
            .collect();
        let pool: Vec<&Arc<Peer>> = if !primaries.is_empty() {
            primaries
        } else {
            self.peers
                .iter()
                .filter(|p| {
                    p.backup && p.is_available(now) && !exclude.iter().any(|x| x == &p.addr)
                })
                .collect()
        };

        if pool.is_empty() {
            return None;
        }

        match self.method {
            LbMethod::RoundRobin => self.pick_weighted_rr(&pool),
            LbMethod::LeastConn => self.pick_least_conn(&pool),
            LbMethod::IpHash => self.pick_ip_hash(&pool, client_ip),
        }
    }

    fn pick_weighted_rr(&self, pool: &[&Arc<Peer>]) -> Option<Arc<Peer>> {
        let total: u32 = pool.iter().map(|p| p.weight.max(1)).sum();
        if total == 0 {
            return None;
        }
        let i = self.rr_counter.fetch_add(1, Ordering::Relaxed) as u32;
        let mut target = i % total;
        for p in pool {
            let w = p.weight.max(1);
            if w > target {
                return Some((*p).clone());
            }
            target -= w;
        }
        Some(pool[0].clone())
    }

    fn pick_least_conn(&self, pool: &[&Arc<Peer>]) -> Option<Arc<Peer>> {
        // Score = in_flight * 1_000_000 / weight. Lower is better.
        pool.iter()
            .min_by_key(|p| {
                let inf = p.in_flight() as u64;
                let w = p.weight.max(1) as u64;
                (inf * 1_000_000) / w
            })
            .map(|p| (*p).clone())
    }

    fn pick_ip_hash(
        &self,
        pool: &[&Arc<Peer>],
        client_ip: IpAddr,
    ) -> Option<Arc<Peer>> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        client_ip.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % pool.len();
        Some(pool[idx].clone())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn build(cfg: &Config) -> Result<Runtime, String> {
    let http = cfg
        .http
        .as_ref()
        .ok_or("config has no 'http' block; nothing to serve")?;

    // Build cache zones.
    let mut cache_zones: HashMap<String, Arc<crate::cache::CacheStore>> = HashMap::new();
    for z in &http.cache_zones {
        cache_zones.insert(
            z.name.clone(),
            crate::cache::CacheStore::new(z.name.clone(), z.max_bytes),
        );
    }
    // Build limit_req zones.
    let mut limit_req_zones: HashMap<String, Arc<crate::limit::LimitReqZone>> =
        HashMap::new();
    for z in &http.limit_req_zones {
        limit_req_zones.insert(
            z.name.clone(),
            Arc::new(crate::limit::LimitReqZone::new(
                z.name.clone(),
                z.key_template.clone(),
                z.rate_per_sec,
                z.max_entries,
            )),
        );
    }
    // Build limit_conn zones.
    let mut limit_conn_zones: HashMap<String, Arc<crate::limit::LimitConnZone>> =
        HashMap::new();
    for z in &http.limit_conn_zones {
        limit_conn_zones.insert(
            z.name.clone(),
            Arc::new(crate::limit::LimitConnZone::new(
                z.name.clone(),
                z.key_template.clone(),
                z.max_entries,
            )),
        );
    }

    let mut balancers: HashMap<String, Arc<Balancer>> = HashMap::new();
    for up in &http.upstreams {
        let peers: Vec<Arc<Peer>> = up
            .servers
            .iter()
            .map(|s| {
                Arc::new(Peer {
                    addr: s.addr.clone(),
                    weight: s.weight,
                    max_fails: s.max_fails,
                    fail_timeout: s.fail_timeout,
                    backup: s.backup,
                    down: s.down,
                    in_flight: AtomicU32::new(0),
                    consecutive_failures: AtomicU32::new(0),
                    failed_until_ms: AtomicU64::new(0),
                })
            })
            .collect();
        let balancer = Arc::new(Balancer {
            name: up.name.clone(),
            method: up.method,
            peers,
            scheme: "http", // named upstream defaults to plain HTTP
            rr_counter: AtomicUsize::new(0),
        });
        if let Some(hc) = &up.health_check {
            crate::health::start(&balancer, hc.clone());
        }
        balancers.insert(up.name.clone(), balancer);
    }
    // Shared Arc<HashMap> for dynamic `proxy_pass` resolution against the
    // named-upstream table.
    let balancers_arc: Arc<HashMap<String, Arc<Balancer>>> =
        Arc::new(balancers.clone());

    // Build a flat `(addr, ServerState, cert?)` list first; group by addr
    // afterwards so multiple `server` blocks on the same port collapse
    // into a single listener with SNI multi-cert.
    let mut staged: Vec<(SocketAddr, VirtualHost, Option<crate::tls::CertEntry>)> =
        Vec::with_capacity(http.servers.len());
    for s in &http.servers {
        let addr = s
            .listen
            .ok_or("a 'server' block is missing its 'listen' directive")?;

        let mut exact_locs: Vec<LocationRt> = Vec::new();
        let mut regex_locs: Vec<(regex::Regex, LocationRt)> = Vec::new();
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
                Action::ProxyPass { target } => {
                    let cache = if let Some(zone_name) = &loc.proxy_cache {
                        let store = cache_zones.get(zone_name).cloned().ok_or_else(|| {
                            format!(
                                "location uses proxy_cache '{}' but no proxy_cache_path \
                                 declares that zone",
                                zone_name
                            )
                        })?;
                        let key_template = loc
                            .proxy_cache_key
                            .clone()
                            .unwrap_or_else(|| Template::parse("$scheme$host$request_uri"));
                        Some(ProxyCache {
                            store,
                            key_template,
                            valid_rules: loc.proxy_cache_valid.clone(),
                        })
                    } else {
                        None
                    };
                    let proxy_target = if target.contains('$') {
                        ProxyTarget::Dynamic {
                            template: Template::parse(target),
                            balancers: balancers_arc.clone(),
                            ephemeral: Arc::new(std::sync::RwLock::new(HashMap::new())),
                        }
                    } else {
                        ProxyTarget::Fixed(resolve_proxy(target, &balancers))
                    };
                    ActionRt::Proxy {
                        target: proxy_target,
                        set_headers: Arc::new(compile_headers(&loc.set_headers)?),
                        cache,
                    }
                }
                Action::Metrics => ActionRt::Metrics,
                Action::TryFiles { root, entries } => ActionRt::TryFiles {
                    root: PathBuf::from(root),
                    entries: entries
                        .iter()
                        .map(|e| match e {
                            crate::config::TryFilesEntry::Path(t) => {
                                TryFilesEntryRt::Path(t.clone())
                            }
                            crate::config::TryFilesEntry::Status(c) => {
                                TryFilesEntryRt::Status(*c)
                            }
                        })
                        .collect(),
                },
            };
            // Cascade: server-level `add_header` directives are applied
            // first, then location-level. Last write wins, so a location-
            // level entry overrides a server-level one with the same name.
            let mut merged_headers = s.add_headers.clone();
            merged_headers.extend(loc.add_headers.iter().cloned());
            let auth = match (&loc.auth_basic_realm, &loc.auth_basic_user_file) {
                (Some(realm), Some(file)) => Some(
                    crate::auth::AuthBasic::load(std::path::Path::new(file), realm.clone())?,
                ),
                _ => None,
            };
            let limit_req = if let Some((zone_name, burst)) = &loc.limit_req {
                let zone = limit_req_zones.get(zone_name).cloned().ok_or_else(|| {
                    format!(
                        "location uses 'limit_req zone={zone_name}' but no 'limit_req_zone' declares that zone"
                    )
                })?;
                Some(crate::limit::LimitReqApply {
                    zone,
                    burst: *burst,
                })
            } else {
                None
            };
            let limit_conn = if let Some((zone_name, max_conn)) = &loc.limit_conn {
                let zone = limit_conn_zones.get(zone_name).cloned().ok_or_else(|| {
                    format!(
                        "location uses 'limit_conn zone={zone_name}' but no 'limit_conn_zone' declares that zone"
                    )
                })?;
                Some(crate::limit::LimitConnApply {
                    zone,
                    max_conn: *max_conn,
                })
            } else {
                None
            };
            let mut rules: Vec<crate::access::AccessRule> =
                Vec::with_capacity(loc.access_rules.len());
            for (is_allow, target) in &loc.access_rules {
                let t = crate::access::parse_target(target).map_err(|e| {
                    format!("location '{}': {e}", loc.path)
                })?;
                rules.push(if *is_allow {
                    crate::access::AccessRule::allow(t)
                } else {
                    crate::access::AccessRule::deny(t)
                });
            }
            let location_rt = LocationRt {
                path: loc.path.clone(),
                action,
                add_headers: Arc::new(compile_headers(&merged_headers)?),
                expires: loc.expires,
                gzip: loc.gzip,
                autoindex: loc.autoindex,
                auth,
                limit_req,
                limit_conn,
                access_rules: Arc::new(rules),
                proxy_connect_timeout: loc.proxy_connect_timeout,
                proxy_read_timeout: loc.proxy_read_timeout,
                auth_request: loc.auth_request.clone(),
                mirrors: Arc::new(loc.mirrors.clone()),
                proxy_ssl_verify: loc.proxy_ssl_verify,
                proxy_client: match (
                    &loc.proxy_ssl_certificate,
                    &loc.proxy_ssl_certificate_key,
                ) {
                    (Some(cert), Some(key)) => Some(Arc::new(
                        crate::proxy::ProxyClient::with_mtls(
                            std::path::Path::new(cert),
                            std::path::Path::new(key),
                            loc.proxy_ssl_verify,
                        )?,
                    )),
                    _ => None,
                },
            };
            match &loc.kind {
                LocationKind::Exact => exact_locs.push(location_rt),
                LocationKind::Regex {
                    pattern,
                    case_insensitive,
                } => {
                    let mut builder = regex::RegexBuilder::new(pattern);
                    builder.case_insensitive(*case_insensitive);
                    let compiled = builder.build().map_err(|e| {
                        format!(
                            "location regex '{pattern}' is invalid: {e}"
                        )
                    })?;
                    regex_locs.push((compiled, location_rt));
                }
                LocationKind::Prefix => prefix_locs.push(location_rt),
            }
        }
        prefix_locs.sort_by(|a, b| b.path.len().cmp(&a.path.len()));

        let cert_entry = if s.tls {
            let cert = s
                .ssl_certificate
                .as_ref()
                .ok_or("missing ssl_certificate for a TLS server")?;
            let key = s
                .ssl_certificate_key
                .as_ref()
                .ok_or("missing ssl_certificate_key for a TLS server")?;
            Some(crate::tls::CertEntry {
                server_name: s.server_name.clone(),
                cert_path: PathBuf::from(cert),
                key_path: PathBuf::from(key),
            })
        } else {
            None
        };

        let scheme = if s.tls { "https" } else { "http" };
        let state = Arc::new(ServerState {
            server_name: s.server_name.clone(),
            scheme,
            gzip: s.gzip.unwrap_or(false),
            gzip_types: s.gzip_types.clone(),
            gzip_min_length: s.gzip_min_length.unwrap_or(20),
            maps: Arc::new(http.maps.clone()),
            client_max_body_size: s
                .client_max_body_size
                .unwrap_or(DEFAULT_CLIENT_MAX_BODY_SIZE),
            exact_locs,
            regex_locs,
            prefix_locs,
        });
        staged.push((
            addr,
            VirtualHost {
                server_name: s.server_name.clone(),
                state,
            },
            cert_entry,
        ));
    }

    // Group by listen address.
    let mut listener_map: HashMap<SocketAddr, ListenerCfg> = HashMap::new();
    let mut order: Vec<SocketAddr> = Vec::new();
    for (addr, vhost, cert) in staged {
        let entry = listener_map.entry(addr).or_insert_with(|| {
            order.push(addr);
            ListenerCfg {
                addr,
                vhosts: Vec::new(),
                tls: None,
                h3_tls: None,
                tls_paths: Vec::new(),
            }
        });
        // Plain + TLS on the same address is ambiguous — refuse loudly.
        match (entry.tls_paths.is_empty(), cert.is_some()) {
            // Mixing: previous blocks were one mode, this block is the other.
            _ if !entry.vhosts.is_empty()
                && entry.tls_paths.is_empty() != cert.is_none() =>
            {
                return Err(format!(
                    "listen {addr}: cannot mix TLS and plain HTTP server blocks on the same address"
                ));
            }
            _ => {}
        }
        if let Some(c) = cert {
            entry.tls_paths.push(c);
        }
        entry.vhosts.push(vhost);
    }

    // Materialize TLS server configs for grouped listeners. Use the
    // strictest ssl_protocols set found across the server blocks on this
    // address (intersection); if none specify, fall back to rustls
    // defaults (TLS 1.2 + 1.3).
    let mut protocols_by_addr: HashMap<SocketAddr, Vec<crate::config::TlsVersion>> =
        HashMap::new();
    for s in &http.servers {
        if let Some(addr) = s.listen {
            if s.tls && !s.ssl_protocols.is_empty() {
                protocols_by_addr
                    .entry(addr)
                    .or_default()
                    .extend(s.ssl_protocols.iter().copied());
            }
        }
    }
    // For each listener that has any HTTP/3 vhost, remember that fact.
    let mut wants_h3: HashMap<SocketAddr, bool> = HashMap::new();
    for s in &http.servers {
        if let Some(addr) = s.listen {
            if s.http3 {
                wants_h3.insert(addr, true);
            }
        }
    }
    for cfg in listener_map.values_mut() {
        if !cfg.tls_paths.is_empty() {
            let protocols = protocols_by_addr
                .remove(&cfg.addr)
                .map(|mut v| {
                    v.sort_by_key(|p| matches!(p, crate::config::TlsVersion::Tls13));
                    v.dedup();
                    v
                })
                .unwrap_or_default();
            cfg.tls = Some(crate::tls::build_server_config(&cfg.tls_paths, &protocols)?);
            if wants_h3.get(&cfg.addr).copied().unwrap_or(false) {
                cfg.h3_tls = Some(crate::tls::build_h3_server_config(&cfg.tls_paths)?);
            }
        }
    }
    let listeners: Vec<ListenerCfg> = order
        .into_iter()
        .map(|a| listener_map.remove(&a).expect("inserted above"))
        .collect();

    // Build stream listeners.
    let mut stream_servers: Vec<(SocketAddr, Arc<Balancer>, bool)> = Vec::new();
    if let Some(stream) = &cfg.stream {
        let mut stream_balancers: HashMap<String, Arc<Balancer>> = HashMap::new();
        for up in &stream.upstreams {
            let peers: Vec<Arc<Peer>> = up
                .servers
                .iter()
                .map(|s| {
                    Arc::new(Peer {
                        addr: s.addr.clone(),
                        weight: s.weight,
                        max_fails: s.max_fails,
                        fail_timeout: s.fail_timeout,
                        backup: s.backup,
                        down: s.down,
                        in_flight: AtomicU32::new(0),
                        consecutive_failures: AtomicU32::new(0),
                        failed_until_ms: AtomicU64::new(0),
                    })
                })
                .collect();
            stream_balancers.insert(
                up.name.clone(),
                Arc::new(Balancer {
                    name: up.name.clone(),
                    method: up.method,
                    peers,
                    scheme: "http",
                    rr_counter: AtomicUsize::new(0),
                }),
            );
        }
        for s in &stream.servers {
            let addr = s
                .listen
                .ok_or("a stream 'server' is missing 'listen'")?;
            let target = s
                .proxy_pass
                .as_ref()
                .ok_or("a stream 'server' is missing 'proxy_pass'")?;
            stream_servers.push((
                addr,
                resolve_proxy(target, &stream_balancers),
                s.udp,
            ));
        }
    }

    if listeners.is_empty() && stream_servers.is_empty() {
        return Err("config has no 'server' blocks (http or stream)".into());
    }
    Ok(Runtime {
        listeners,
        stream_servers,
    })
}

/// `proxy_pass <target>` — look up `target` as an upstream name, or treat it
/// as a single direct address. Single-address peers get sensible health
/// defaults (one failure tolerated, 10-second cooldown).
///
/// Recognized schemes:
///   - `http://NAME`        — named upstream over plain HTTP.
///   - `http://HOST:PORT`   — direct address, plain HTTP.
///   - `https://HOST:PORT`  — direct address, HTTPS (TLS to the upstream).
fn resolve_proxy(
    target: &str,
    balancers: &HashMap<String, Arc<Balancer>>,
) -> Arc<Balancer> {
    let (scheme, rest) = if let Some(s) = target.strip_prefix("https://") {
        ("https", s)
    } else if let Some(s) = target.strip_prefix("http://") {
        ("http", s)
    } else {
        ("http", target)
    };
    let host = rest.trim_end_matches('/');

    if let Some(b) = balancers.get(host) {
        return b.clone();
    }

    let peer = Arc::new(Peer {
        addr: host.to_string(),
        weight: 1,
        max_fails: 1,
        fail_timeout: Duration::from_secs(10),
        backup: false,
        down: false,
        in_flight: AtomicU32::new(0),
        consecutive_failures: AtomicU32::new(0),
        failed_until_ms: AtomicU64::new(0),
    });

    Arc::new(Balancer {
        name: host.to_string(),
        method: LbMethod::RoundRobin,
        peers: vec![peer],
        scheme,
        rr_counter: AtomicUsize::new(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{HeaderMap, Method, Uri};

    fn peer(addr: &str, weight: u32) -> Arc<Peer> {
        Arc::new(Peer {
            addr: addr.into(),
            weight,
            max_fails: 1,
            fail_timeout: Duration::from_secs(10),
            backup: false,
            down: false,
            in_flight: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            failed_until_ms: AtomicU64::new(0),
        })
    }

    fn test_balancer(name: &str, method: LbMethod, peers: Vec<Arc<Peer>>) -> Balancer {
        Balancer {
            name: name.into(),
            method,
            peers,
            scheme: "http",
            rr_counter: AtomicUsize::new(0),
        }
    }

    fn empty_user_vars() -> &'static std::collections::HashMap<String, String> {
        use std::sync::OnceLock;
        static EMPTY: OnceLock<std::collections::HashMap<String, String>> =
            OnceLock::new();
        EMPTY.get_or_init(std::collections::HashMap::new)
    }

    fn mk_ctx<'a>(
        method: &'a Method,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        peer_ip: &'a str,
    ) -> RequestCtx<'a> {
        RequestCtx {
            peer: peer_ip.parse().unwrap(),
            server_name: None,
            method,
            uri,
            headers,
            scheme: "http",
            user_vars: empty_user_vars(),
        }
    }

    #[test]
    fn weighted_rr_respects_weights() {
        let b = Balancer {
            name: "t".into(),
            method: LbMethod::RoundRobin,
            peers: vec![peer("a", 2), peer("b", 1)],
            scheme: "http",
            rr_counter: AtomicUsize::new(0),
        };
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let ctx = mk_ctx(&m, &u, &h, "127.0.0.1:1");
        let mut counts = std::collections::HashMap::new();
        for _ in 0..30 {
            let p = b.pick(&ctx).unwrap();
            *counts.entry(p.addr.clone()).or_insert(0u32) += 1;
        }
        assert_eq!(counts["a"], 20);
        assert_eq!(counts["b"], 10);
    }

    #[test]
    fn ip_hash_is_stable_per_client() {
        let b = Balancer {
            name: "t".into(),
            method: LbMethod::IpHash,
            peers: vec![peer("a", 1), peer("b", 1), peer("c", 1)],
            scheme: "http",
            rr_counter: AtomicUsize::new(0),
        };
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let ctx1 = mk_ctx(&m, &u, &h, "10.0.0.1:5000");
        let first = b.pick(&ctx1).unwrap().addr.clone();
        for _ in 0..10 {
            assert_eq!(b.pick(&ctx1).unwrap().addr, first);
        }
        // Different client should be allowed to map to a different peer
        // (we don't require it; just that the same client is stable).
        let ctx2 = mk_ctx(&m, &u, &h, "10.0.0.2:5000");
        let _ = b.pick(&ctx2).unwrap();
    }

    #[test]
    fn least_conn_prefers_idle_peer() {
        let a = peer("a", 1);
        let bp = peer("b", 1);
        // Simulate two requests already in flight on `a`.
        a.in_flight.store(2, Ordering::Relaxed);
        let b = Balancer {
            name: "t".into(),
            method: LbMethod::LeastConn,
            peers: vec![a.clone(), bp.clone()],
            scheme: "http",
            rr_counter: AtomicUsize::new(0),
        };
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let ctx = mk_ctx(&m, &u, &h, "127.0.0.1:1");
        let chosen = b.pick(&ctx).unwrap();
        assert_eq!(chosen.addr, "b");
    }

    #[test]
    fn failed_peer_is_skipped_until_timeout() {
        let a = peer("a", 1);
        let bp = peer("b", 1);
        // Force `a` into a 60s cooldown by recording max_fails failures.
        a.record_failure();
        assert!(!a.is_available(now_ms() + 1));
        let b = Balancer {
            name: "t".into(),
            method: LbMethod::RoundRobin,
            peers: vec![a, bp.clone()],
            scheme: "http",
            rr_counter: AtomicUsize::new(0),
        };
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let ctx = mk_ctx(&m, &u, &h, "127.0.0.1:1");
        for _ in 0..5 {
            assert_eq!(b.pick(&ctx).unwrap().addr, "b");
        }
    }

    #[test]
    fn backup_used_only_when_primaries_unavailable() {
        let primary = peer("primary", 1);
        primary.record_failure();
        let mut backup = Peer {
            addr: "backup".into(),
            weight: 1,
            max_fails: 1,
            fail_timeout: Duration::from_secs(10),
            backup: true,
            down: false,
            in_flight: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            failed_until_ms: AtomicU64::new(0),
        };
        backup.consecutive_failures = AtomicU32::new(0);
        let backup = Arc::new(backup);
        let b = Balancer {
            name: "t".into(),
            method: LbMethod::RoundRobin,
            peers: vec![primary.clone(), backup.clone()],
            scheme: "http",
            rr_counter: AtomicUsize::new(0),
        };
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let ctx = mk_ctx(&m, &u, &h, "127.0.0.1:1");
        assert_eq!(b.pick(&ctx).unwrap().addr, "backup");
        // Primary recovers → it preferred again.
        primary.record_success();
        assert_eq!(b.pick(&ctx).unwrap().addr, "primary");
    }

    #[test]
    fn down_peer_is_never_picked() {
        let mut downed = (*peer("down", 1)).clone_for_test();
        downed.down = true;
        let downed = Arc::new(downed);
        let alive = peer("alive", 1);
        let b = Balancer {
            name: "t".into(),
            method: LbMethod::RoundRobin,
            peers: vec![downed, alive.clone()],
            scheme: "http",
            rr_counter: AtomicUsize::new(0),
        };
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let ctx = mk_ctx(&m, &u, &h, "127.0.0.1:1");
        for _ in 0..5 {
            assert_eq!(b.pick(&ctx).unwrap().addr, "alive");
        }
    }

    impl Peer {
        fn clone_for_test(&self) -> Peer {
            Peer {
                addr: self.addr.clone(),
                weight: self.weight,
                max_fails: self.max_fails,
                fail_timeout: self.fail_timeout,
                backup: self.backup,
                down: self.down,
                in_flight: AtomicU32::new(0),
                consecutive_failures: AtomicU32::new(0),
                failed_until_ms: AtomicU64::new(0),
            }
        }
    }
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
