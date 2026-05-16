//! In-memory response cache with **Vary-aware variants**.
//!
//! v0.34.0 honors the `Vary` response header. A single `proxy_cache_key`
//! may resolve to multiple stored variants, each keyed by the values of
//! the request headers the upstream said it varies on. The classic case
//! is `Vary: Accept-Encoding`, where the gzip and identity bodies are
//! both legitimate to cache but must not be served to the wrong client.
//!
//! ## Caching is still rejected (with `X-Cache: BYPASS`) when:
//!
//! - The request method is not `GET`.
//! - The response has a `Set-Cookie` header.
//! - The response has `Cache-Control: no-store`, `private`, or `no-cache`.
//! - The response status doesn't match any `proxy_cache_valid` rule.
//! - The response body exceeds [`MAX_ENTRY_BYTES`] (4 MiB).
//! - The response's `Vary` contains `*` (uncacheable per RFC 9111).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};

use crate::metrics;

pub const MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

/// One cached response with its Vary-axis fingerprint.
#[derive(Clone)]
pub struct Entry {
    pub status: u16,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Bytes,
    pub expires_at: Instant,
    /// Lowercased request-header names this entry depends on (from the
    /// upstream's `Vary` response header). Empty = the entry is valid
    /// for any request matching `cache_key`.
    pub vary_headers: Vec<String>,
    /// Pre-computed signature of `vary_headers` against the request that
    /// produced this entry. Used to match the right variant on lookup.
    pub vary_signature: String,
}

pub struct CacheStore {
    #[allow(dead_code)]
    pub name: String,
    pub max_bytes: usize,
    state: Mutex<State>,
}

struct State {
    /// `cache_key → list of variants`. Each variant is one response
    /// produced for a particular Vary-axis tuple.
    entries: HashMap<String, Vec<Entry>>,
    total_bytes: usize,
}

impl CacheStore {
    pub fn new(name: String, max_bytes: usize) -> Arc<Self> {
        Arc::new(CacheStore {
            name,
            max_bytes,
            state: Mutex::new(State {
                entries: HashMap::new(),
                total_bytes: 0,
            }),
        })
    }

    /// Look up a fresh entry matching this request. Stale variants are
    /// evicted opportunistically.
    pub fn get(&self, key: &str, req_headers: &HeaderMap) -> Option<Entry> {
        let now = Instant::now();
        let mut s = self.state.lock().ok()?;
        let mut found = None;
        let mut bytes_dropped = 0usize;
        let mut remove_key = false;
        if let Some(variants) = s.entries.get_mut(key) {
            variants.retain(|v| {
                if v.expires_at <= now {
                    bytes_dropped += entry_bytes(v);
                    false
                } else {
                    true
                }
            });
            for v in variants.iter() {
                let sig = build_signature(req_headers, &v.vary_headers);
                if sig == v.vary_signature {
                    found = Some(v.clone());
                    break;
                }
            }
            if variants.is_empty() {
                remove_key = true;
            }
        }
        if bytes_dropped > 0 {
            s.total_bytes = s.total_bytes.saturating_sub(bytes_dropped);
            metrics::record_cache_evict(bytes_dropped);
        }
        if remove_key {
            s.entries.remove(key);
        }
        if found.is_some() {
            metrics::record_cache_hit();
        } else {
            metrics::record_cache_miss();
        }
        found
    }

    /// Store a variant. Evicts oldest entries (by `expires_at`) until the
    /// new one fits.
    pub fn put(&self, key: String, entry: Entry) {
        let size = entry_bytes(&entry);
        if size > self.max_bytes {
            return;
        }
        let Ok(mut s) = self.state.lock() else { return };

        while s.total_bytes + size > self.max_bytes {
            // Find soonest-to-expire (key, signature) without holding a
            // borrow into `s` past the lookup phase.
            let target: Option<(String, String)> = {
                let mut best: Option<(&str, &str, Instant)> = None;
                for (k, vs) in s.entries.iter() {
                    for v in vs {
                        match &best {
                            Some((_, _, t)) if *t <= v.expires_at => {}
                            _ => {
                                best = Some((
                                    k.as_str(),
                                    v.vary_signature.as_str(),
                                    v.expires_at,
                                ));
                            }
                        }
                    }
                }
                best.map(|(k, sig, _)| (k.to_string(), sig.to_string()))
            };
            let (k, sig) = match target {
                Some(t) => t,
                None => break,
            };
            // Scope the &mut borrow on s.entries so we can touch
            // s.total_bytes / s.entries.remove afterwards without
            // overlapping the lifetime.
            let (bytes_removed, empty_now) = {
                if let Some(vs) = s.entries.get_mut(&k) {
                    let mut br = 0usize;
                    if let Some(pos) =
                        vs.iter().position(|v| v.vary_signature == sig)
                    {
                        let removed = vs.remove(pos);
                        br = entry_bytes(&removed);
                    }
                    (br, vs.is_empty())
                } else {
                    (0, false)
                }
            };
            if bytes_removed > 0 {
                s.total_bytes = s.total_bytes.saturating_sub(bytes_removed);
                metrics::record_cache_evict(bytes_removed);
            }
            if empty_now {
                s.entries.remove(&k);
            }
        }

        // Same pattern: confine the &mut on s.entries to a tight scope so
        // we can update s.total_bytes after.
        let replaced_bytes = {
            let variants = s.entries.entry(key).or_default();
            let replaced = if let Some(pos) = variants
                .iter()
                .position(|v| v.vary_signature == entry.vary_signature)
            {
                let old = variants.remove(pos);
                entry_bytes(&old)
            } else {
                0
            };
            variants.push(entry);
            replaced
        };
        s.total_bytes = s.total_bytes.saturating_sub(replaced_bytes);
        s.total_bytes += size;

        let entry_count: usize = s.entries.values().map(|v| v.len()).sum();
        metrics::set_cache_bytes(s.total_bytes as u64);
        metrics::set_cache_entries(entry_count as u64);
    }
}

fn entry_bytes(e: &Entry) -> usize {
    let header_bytes: usize = e
        .headers
        .iter()
        .map(|(n, v)| n.as_str().len() + v.as_bytes().len())
        .sum();
    let vary_bytes: usize = e.vary_headers.iter().map(|s| s.len()).sum::<usize>()
        + e.vary_signature.len();
    e.body.len() + header_bytes + vary_bytes
}

/// Build a stable "vary signature" for `req_headers` over `vary_names`.
/// Each pair is `<name>=<value>` joined by `\0`. Order matches the input
/// order so the same request always renders the same signature.
pub fn build_signature(req_headers: &HeaderMap, vary_names: &[String]) -> String {
    if vary_names.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(vary_names.len() * 16);
    for name in vary_names {
        let v = req_headers
            .get(name.as_str())
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        out.push_str(name);
        out.push('=');
        out.push_str(v);
        out.push('\0');
    }
    out
}

/// Extract the `Vary` header(s) into a normalized lowercase list of
/// header names. Returns `Some(list)` for cacheable responses, or
/// `Some(vec_with_star)` if the response said `Vary: *` (which is then
/// caught by `decide_caching`).
pub fn parse_vary(headers: &HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    for v in headers.get_all("vary").iter() {
        if let Ok(s) = v.to_str() {
            for token in s.split(',') {
                let t = token.trim().to_ascii_lowercase();
                if !t.is_empty() {
                    out.push(t);
                }
            }
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum CacheDecision {
    Store(Duration),
    Bypass(&'static str),
}

/// Decide whether a response is cacheable.
pub fn decide_caching(
    method: &hyper::Method,
    headers: &HeaderMap,
    status: u16,
    body_len: usize,
    valid_rules: &[(Vec<u16>, Duration)],
) -> CacheDecision {
    if method != hyper::Method::GET {
        return CacheDecision::Bypass("method not GET");
    }
    if headers.get("set-cookie").is_some() {
        return CacheDecision::Bypass("response has Set-Cookie");
    }
    // Vary: * — RFC 9111 says treat as uncacheable.
    if parse_vary(headers).iter().any(|s| s == "*") {
        return CacheDecision::Bypass("response has Vary: *");
    }
    if let Some(cc) = headers.get("cache-control").and_then(|v| v.to_str().ok()) {
        let cc = cc.to_ascii_lowercase();
        for tok in cc.split(',').map(str::trim) {
            if tok == "no-store" || tok == "private" || tok == "no-cache" {
                return CacheDecision::Bypass("response Cache-Control disallows caching");
            }
        }
    }
    if body_len > MAX_ENTRY_BYTES {
        return CacheDecision::Bypass("body exceeds per-entry size limit");
    }

    let mut ttl: Option<Duration> = None;
    for (codes, d) in valid_rules {
        if codes.is_empty() || codes.contains(&status) {
            ttl = Some(*d);
        }
    }
    match ttl {
        Some(d) => CacheDecision::Store(d),
        None => CacheDecision::Bypass("no proxy_cache_valid rule matched"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Method;

    fn rules_for_200(secs: u64) -> Vec<(Vec<u16>, Duration)> {
        vec![(vec![200], Duration::from_secs(secs))]
    }

    fn entry(body: &[u8], vary: &[&str], req: &HeaderMap) -> Entry {
        let vary_headers: Vec<String> = vary.iter().map(|s| s.to_string()).collect();
        let sig = build_signature(req, &vary_headers);
        Entry {
            status: 200,
            headers: vec![],
            body: Bytes::copy_from_slice(body),
            expires_at: Instant::now() + Duration::from_secs(60),
            vary_headers,
            vary_signature: sig,
        }
    }

    #[test]
    fn non_get_is_bypassed() {
        let d = decide_caching(&Method::POST, &HeaderMap::new(), 200, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Bypass(_)));
    }

    #[test]
    fn set_cookie_blocks_caching() {
        let mut h = HeaderMap::new();
        h.insert("set-cookie", "sid=abc".parse().unwrap());
        assert!(matches!(
            decide_caching(&Method::GET, &h, 200, 10, &rules_for_200(60)),
            CacheDecision::Bypass(_)
        ));
    }

    #[test]
    fn vary_star_is_bypassed() {
        let mut h = HeaderMap::new();
        h.insert("vary", "*".parse().unwrap());
        let d = decide_caching(&Method::GET, &h, 200, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Bypass(s) if s.contains("Vary")));
    }

    #[test]
    fn cc_no_store_blocks_caching() {
        let mut h = HeaderMap::new();
        h.insert("cache-control", "no-store".parse().unwrap());
        assert!(matches!(
            decide_caching(&Method::GET, &h, 200, 10, &rules_for_200(60)),
            CacheDecision::Bypass(_)
        ));
    }

    #[test]
    fn missing_valid_rule_bypasses() {
        assert!(matches!(
            decide_caching(&Method::GET, &HeaderMap::new(), 404, 10, &rules_for_200(60)),
            CacheDecision::Bypass(_)
        ));
    }

    #[test]
    fn matching_status_stores() {
        assert!(matches!(
            decide_caching(&Method::GET, &HeaderMap::new(), 200, 10, &rules_for_200(60)),
            CacheDecision::Store(d) if d.as_secs() == 60
        ));
    }

    #[test]
    fn vary_variants_kept_separately() {
        let s = CacheStore::new("t".into(), 1024 * 1024);
        let mut req_gz = HeaderMap::new();
        req_gz.insert("accept-encoding", "gzip".parse().unwrap());
        let mut req_id = HeaderMap::new();
        req_id.insert("accept-encoding", "identity".parse().unwrap());

        s.put(
            "k".into(),
            entry(b"GZIPPED", &["accept-encoding"], &req_gz),
        );
        s.put(
            "k".into(),
            entry(b"PLAIN", &["accept-encoding"], &req_id),
        );

        let gz = s.get("k", &req_gz).expect("hit gz");
        assert_eq!(&gz.body[..], b"GZIPPED");
        let id = s.get("k", &req_id).expect("hit identity");
        assert_eq!(&id.body[..], b"PLAIN");
    }

    #[test]
    fn vary_request_without_matching_variant_misses() {
        let s = CacheStore::new("t".into(), 1024 * 1024);
        let mut req_gz = HeaderMap::new();
        req_gz.insert("accept-encoding", "gzip".parse().unwrap());
        s.put(
            "k".into(),
            entry(b"GZIPPED", &["accept-encoding"], &req_gz),
        );
        let mut req_br = HeaderMap::new();
        req_br.insert("accept-encoding", "br".parse().unwrap());
        assert!(s.get("k", &req_br).is_none());
    }

    #[test]
    fn store_drops_expired_on_read() {
        let s = CacheStore::new("t".into(), 1024 * 1024);
        let req = HeaderMap::new();
        let mut e = entry(b"x", &[], &req);
        e.expires_at = Instant::now() - Duration::from_millis(1);
        s.put("k".into(), e);
        assert!(s.get("k", &req).is_none());
    }
}
