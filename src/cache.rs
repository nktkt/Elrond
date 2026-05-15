//! In-memory response cache for `proxy_cache`.
//!
//! v0.11.0 is an honest MVP — fully in-memory, single-zone, with strict
//! safety guards in front of every insertion. The goal is to land caching
//! correctly *first* and then grow toward Nginx feature parity.
//!
//! ## Caching is rejected (with `X-Cache: BYPASS`) when any of these hold
//!
//! - The request method is not `GET`.
//! - The response has a `Set-Cookie` header.
//! - The response has any `Vary` header (we don't yet compute keyed
//!   variants).
//! - The response has `Cache-Control: no-store`, `private`, or `no-cache`.
//! - The response status doesn't match any `proxy_cache_valid` rule.
//! - The response body exceeds [`MAX_ENTRY_BYTES`] (4 MiB).
//!
//! ## What's still missing on purpose
//!
//! Vary-aware variants, `stale-while-revalidate`, conditional revalidation,
//! cache locking (one fill at a time per key), disk persistence, cache
//! purge endpoint, range-aware caching. They are roadmap items.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};

use crate::metrics;

/// Per-entry hard ceiling. Anything larger streams through without caching.
pub const MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

/// Cached response.
#[derive(Clone)]
pub struct Entry {
    pub status: u16,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Bytes,
    pub expires_at: Instant,
}

/// A `proxy_cache_path … keys_zone=NAME:SIZE` zone, runtime form. The store
/// is small enough to be `Mutex<HashMap<_, _>>`; we'll graduate to sharded
/// or concurrent maps once the API surface is settled.
pub struct CacheStore {
    /// Zone name (used for diagnostics; kept here for future logging /
    /// multi-zone routing).
    #[allow(dead_code)]
    pub name: String,
    pub max_bytes: usize,
    state: Mutex<State>,
}

struct State {
    entries: HashMap<String, Entry>,
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

    /// Look up a fresh entry. Expired entries are evicted opportunistically.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let now = Instant::now();
        let mut s = self.state.lock().ok()?;
        if let Some(entry) = s.entries.get(key) {
            if entry.expires_at > now {
                metrics::record_cache_hit();
                return Some(entry.clone());
            } else {
                // Stale — remove and account.
                if let Some(removed) = s.entries.remove(key) {
                    let bytes_removed = entry_bytes(&removed);
                    s.total_bytes = s.total_bytes.saturating_sub(bytes_removed);
                    metrics::record_cache_evict(bytes_removed);
                }
            }
        }
        metrics::record_cache_miss();
        None
    }

    /// Insert an entry, evicting older ones if necessary to honor
    /// `max_bytes`. Naïve FIFO-on-overflow eviction is enough for v0.11.0.
    pub fn put(&self, key: String, entry: Entry) {
        let size = entry_bytes(&entry);
        if size > self.max_bytes {
            // Even the requested entry can't fit; refuse silently.
            return;
        }
        let Ok(mut s) = self.state.lock() else { return };

        // Evict until we have room.
        while s.total_bytes + size > self.max_bytes {
            let drop_key = s
                .entries
                .iter()
                .min_by_key(|(_, e)| e.expires_at)
                .map(|(k, _)| k.clone());
            match drop_key {
                Some(k) => {
                    if let Some(removed) = s.entries.remove(&k) {
                        let bytes_removed = entry_bytes(&removed);
                        s.total_bytes = s.total_bytes.saturating_sub(bytes_removed);
                        metrics::record_cache_evict(bytes_removed);
                    } else {
                        break;
                    }
                }
                None => break,
            }
        }

        if let Some(old) = s.entries.insert(key, entry) {
            let bytes_old = entry_bytes(&old);
            s.total_bytes = s.total_bytes.saturating_sub(bytes_old);
        }
        s.total_bytes += size;
        metrics::set_cache_bytes(s.total_bytes as u64);
        metrics::set_cache_entries(s.entries.len() as u64);
    }
}

fn entry_bytes(e: &Entry) -> usize {
    let header_bytes: usize = e
        .headers
        .iter()
        .map(|(n, v)| n.as_str().len() + v.as_bytes().len())
        .sum();
    e.body.len() + header_bytes
}

/// Outcome of consulting safety rules for an outgoing response.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheDecision {
    /// Cache this response with the given TTL.
    Store(Duration),
    /// Don't cache, with a reason for diagnostics.
    Bypass(&'static str),
}

/// Decide whether a response is cacheable. Implements the safety guards
/// listed at the module docs.
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
    if headers.get("vary").is_some() {
        return CacheDecision::Bypass("response has Vary");
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

    #[test]
    fn non_get_is_bypassed() {
        let d = decide_caching(&Method::POST, &HeaderMap::new(), 200, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Bypass(_)));
    }

    #[test]
    fn set_cookie_blocks_caching() {
        let mut h = HeaderMap::new();
        h.insert("set-cookie", "sid=abc".parse().unwrap());
        let d = decide_caching(&Method::GET, &h, 200, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Bypass(s) if s.contains("Set-Cookie")));
    }

    #[test]
    fn vary_blocks_caching() {
        let mut h = HeaderMap::new();
        h.insert("vary", "Accept-Encoding".parse().unwrap());
        let d = decide_caching(&Method::GET, &h, 200, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Bypass(s) if s.contains("Vary")));
    }

    #[test]
    fn cc_no_store_blocks_caching() {
        let mut h = HeaderMap::new();
        h.insert("cache-control", "no-store".parse().unwrap());
        let d = decide_caching(&Method::GET, &h, 200, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Bypass(_)));
    }

    #[test]
    fn missing_valid_rule_bypasses() {
        let d = decide_caching(&Method::GET, &HeaderMap::new(), 404, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Bypass(_)));
    }

    #[test]
    fn matching_status_stores() {
        let d = decide_caching(&Method::GET, &HeaderMap::new(), 200, 10, &rules_for_200(60));
        assert!(matches!(d, CacheDecision::Store(d) if d.as_secs() == 60));
    }

    #[test]
    fn store_roundtrip_and_expiry() {
        let s = CacheStore::new("t".into(), 1024 * 1024);
        let entry = Entry {
            status: 200,
            headers: vec![],
            body: Bytes::from_static(b"hello"),
            expires_at: Instant::now() + Duration::from_secs(60),
        };
        s.put("k".into(), entry);
        let got = s.get("k").expect("hit");
        assert_eq!(&got.body[..], b"hello");
    }

    #[test]
    fn store_drops_expired_on_read() {
        let s = CacheStore::new("t".into(), 1024 * 1024);
        let entry = Entry {
            status: 200,
            headers: vec![],
            body: Bytes::from_static(b"x"),
            expires_at: Instant::now() - Duration::from_millis(1),
        };
        s.put("k".into(), entry);
        assert!(s.get("k").is_none());
    }

    #[test]
    fn store_evicts_when_full() {
        let s = CacheStore::new("t".into(), 32);
        // Each entry is ~16 bytes of body, headers negligible.
        for i in 0..6 {
            s.put(
                format!("k{i}"),
                Entry {
                    status: 200,
                    headers: vec![],
                    body: Bytes::from(vec![0u8; 16]),
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            );
        }
        // The store should have stayed under the cap.
        let st = s.state.lock().unwrap();
        assert!(st.total_bytes <= 32, "total {} > cap", st.total_bytes);
    }
}
