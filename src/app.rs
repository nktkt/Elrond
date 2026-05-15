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
    /// One entry per HTTP `server` block. The optional `rustls::ServerConfig`
    /// signals that the listener should terminate TLS.
    pub servers: Vec<(SocketAddr, SharedState, Option<Arc<rustls::ServerConfig>>)>,
    /// One entry per `stream` `server` block — TCP proxying.
    pub stream_servers: Vec<(SocketAddr, Arc<Balancer>)>,
}

pub struct ServerState {
    pub server_name: Option<String>,
    /// `"http"` or `"https"` — used by the variable engine for `$scheme`.
    pub scheme: &'static str,
    /// Effective gzip-enabled state for this server.
    pub gzip: bool,
    pub gzip_types: Vec<String>,
    exact_locs: Vec<LocationRt>,
    prefix_locs: Vec<LocationRt>,
}

impl ServerState {
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
    /// `expires` value, applied to every response from this location.
    pub expires: Option<Duration>,
    /// Per-location gzip override. `None` -> use the server-level default.
    pub gzip: Option<bool>,
}

pub enum ActionRt {
    Return {
        status: u16,
        body: Template,
    },
    Proxy {
        balancer: Arc<Balancer>,
        set_headers: HeaderList,
        cache: Option<ProxyCache>,
    },
    Static {
        root: PathBuf,
        kind: StaticKind,
    },
    /// Render Prometheus metrics inline.
    Metrics,
}

/// Per-location proxy-cache configuration, resolved at config-build time.
#[derive(Clone)]
pub struct ProxyCache {
    pub store: Arc<crate::cache::CacheStore>,
    pub key_template: Template,
    /// `(status codes (empty = any), ttl)` pairs.
    pub valid_rules: Vec<(Vec<u16>, Duration)>,
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
        balancers.insert(
            up.name.clone(),
            Arc::new(Balancer {
                name: up.name.clone(),
                method: up.method,
                peers,
                rr_counter: AtomicUsize::new(0),
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
                    ActionRt::Proxy {
                        balancer: resolve_proxy(target, &balancers),
                        set_headers: Arc::new(compile_headers(&loc.set_headers)?),
                        cache,
                    }
                }
                Action::Metrics => ActionRt::Metrics,
            };
            // Cascade: server-level `add_header` directives are applied
            // first, then location-level. Last write wins, so a location-
            // level entry overrides a server-level one with the same name.
            let mut merged_headers = s.add_headers.clone();
            merged_headers.extend(loc.add_headers.iter().cloned());
            let location_rt = LocationRt {
                path: loc.path.clone(),
                action,
                add_headers: Arc::new(compile_headers(&merged_headers)?),
                expires: loc.expires,
                gzip: loc.gzip,
            };
            if loc.kind == LocationKind::Exact {
                exact_locs.push(location_rt);
            } else {
                prefix_locs.push(location_rt);
            }
        }
        prefix_locs.sort_by(|a, b| b.path.len().cmp(&a.path.len()));

        let tls = if s.tls {
            let cert = s
                .ssl_certificate
                .as_ref()
                .ok_or("missing ssl_certificate for a TLS server")?;
            let key = s
                .ssl_certificate_key
                .as_ref()
                .ok_or("missing ssl_certificate_key for a TLS server")?;
            Some(crate::tls::server_config(
                std::path::Path::new(cert),
                std::path::Path::new(key),
            )?)
        } else {
            None
        };

        let scheme = if s.tls { "https" } else { "http" };
        servers.push((
            addr,
            Arc::new(ServerState {
                server_name: s.server_name.clone(),
                scheme,
                gzip: s.gzip.unwrap_or(false),
                gzip_types: s.gzip_types.clone(),
                exact_locs,
                prefix_locs,
            }),
            tls,
        ));
    }

    // Build stream listeners.
    let mut stream_servers: Vec<(SocketAddr, Arc<Balancer>)> = Vec::new();
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
            stream_servers.push((addr, resolve_proxy(target, &stream_balancers)));
        }
    }

    if servers.is_empty() && stream_servers.is_empty() {
        return Err("config has no 'server' blocks (http or stream)".into());
    }
    Ok(Runtime {
        servers,
        stream_servers,
    })
}

/// `proxy_pass <target>` — look up `target` as an upstream name, or treat it
/// as a single direct address. Single-address peers get sensible health
/// defaults (one failure tolerated, 10-second cooldown).
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
        }
    }

    #[test]
    fn weighted_rr_respects_weights() {
        let b = Balancer {
            name: "t".into(),
            method: LbMethod::RoundRobin,
            peers: vec![peer("a", 2), peer("b", 1)],
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
