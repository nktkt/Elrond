//! `limit_req` — request rate limiting using a token bucket per key.
//!
//! ```text
//! http {
//!     limit_req_zone $remote_addr zone=auth:10m rate=5r/s;
//!     server {
//!         location /login {
//!             limit_req zone=auth burst=10;
//!             ...
//!         }
//!     }
//! }
//! ```
//!
//! Each zone is a process-wide map from rendered key → token bucket. A
//! bucket starts full (`capacity = burst + 1`) and refills at `rate` tokens
//! per second. Each accepted request consumes one token; when the bucket
//! is dry, the request is denied with `503 Service Unavailable`.
//!
//! When the zone reaches `max_entries`, the least-recently-touched bucket
//! is evicted on the next insertion. The cap protects against a flood of
//! one-off keys eating memory; pick `zone=…:SIZE` based on roughly how
//! many distinct keys you expect — every 1 MiB allows ~16 000 entries.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use hyper::Response;

use crate::body::{text, ElrondBody};
use crate::metrics;
use crate::request_ctx::RequestCtx;
use crate::template::Template;

/// Each entry costs roughly this many bytes (key string + bucket + map
/// overhead). Used to translate `zone=NAME:SIZE` into an entry cap.
pub const APPROX_BYTES_PER_ENTRY: usize = 64;

/// Definition of one `limit_req_zone`. Multiple locations can reference
/// the same zone with different `burst` values.
pub struct LimitReqZone {
    pub name: String,
    pub key_template: Template,
    pub rate_per_sec: f64,
    pub max_entries: usize,
    state: Mutex<State>,
}

struct State {
    buckets: HashMap<String, Bucket>,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl LimitReqZone {
    pub fn new(
        name: String,
        key_template: Template,
        rate_per_sec: f64,
        max_entries: usize,
    ) -> Self {
        LimitReqZone {
            name,
            key_template,
            rate_per_sec,
            max_entries,
            state: Mutex::new(State {
                buckets: HashMap::new(),
            }),
        }
    }

    /// Return `true` to accept the request, `false` to deny it.
    pub fn allow(&self, key: &str, burst: u32) -> bool {
        let capacity = (burst as f64) + 1.0;
        let now = Instant::now();
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return true, // never block on a poisoned mutex
        };

        // Evict if we're already at capacity and this key is new.
        if !s.buckets.contains_key(key) && s.buckets.len() >= self.max_entries {
            if let Some(oldest_key) = s
                .buckets
                .iter()
                .min_by(|(_, a), (_, b)| a.last.cmp(&b.last))
                .map(|(k, _)| k.clone())
            {
                s.buckets.remove(&oldest_key);
            }
        }

        let bucket = s.buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });

        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_sec).min(capacity);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            metrics::record_limit_req_allowed();
            true
        } else {
            metrics::record_limit_req_denied();
            false
        }
    }
}

/// Resolved per-location `limit_req`: a reference to a zone plus this
/// location's chosen `burst`.
#[derive(Clone)]
pub struct LimitReqApply {
    pub zone: std::sync::Arc<LimitReqZone>,
    pub burst: u32,
}

/// Apply the limit. On allow, returns `None`. On deny, returns the
/// `503` response to send back.
pub fn enforce(
    apply: &LimitReqApply,
    ctx: &RequestCtx<'_>,
) -> Option<Response<ElrondBody>> {
    let key = apply.zone.key_template.render(ctx);
    if apply.zone.allow(&key, apply.burst) {
        None
    } else {
        Some(deny_response())
    }
}

fn deny_response() -> Response<ElrondBody> {
    text(503, "503 Service Unavailable\n")
}

/// Parse a rate spec like `5r/s`, `100r/s`, `60r/m`. Returns
/// requests-per-second.
pub fn parse_rate(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, unit) = s.split_once("r/")?;
    let n: f64 = num.parse().ok()?;
    match unit {
        "s" => Some(n),
        "m" => Some(n / 60.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(rate: f64) -> LimitReqZone {
        LimitReqZone::new(
            "t".into(),
            Template::parse("$remote_addr"),
            rate,
            128,
        )
    }

    #[test]
    fn allows_first_burst_immediately() {
        let z = zone(1.0);
        for _ in 0..5 {
            assert!(z.allow("client-a", 4));
        }
    }

    #[test]
    fn denies_after_burst_drained() {
        let z = zone(0.0001); // effectively no refill within the test
        for _ in 0..5 {
            assert!(z.allow("client-a", 4));
        }
        assert!(!z.allow("client-a", 4));
    }

    #[test]
    fn separate_keys_have_separate_buckets() {
        let z = zone(0.0001);
        for _ in 0..3 {
            assert!(z.allow("client-a", 2));
        }
        // client-a now drained
        assert!(!z.allow("client-a", 2));
        // client-b is independent
        for _ in 0..3 {
            assert!(z.allow("client-b", 2));
        }
    }

    #[test]
    fn refill_replenishes_tokens() {
        let z = zone(1000.0); // 1000/s — refills fast
        for _ in 0..5 {
            assert!(z.allow("client-a", 4));
        }
        // drained
        assert!(!z.allow("client-a", 4));
        std::thread::sleep(std::time::Duration::from_millis(20));
        // After 20ms at 1000/s, expect ~20 tokens — plenty.
        assert!(z.allow("client-a", 4));
    }

    #[test]
    fn parses_rate_specs() {
        assert_eq!(parse_rate("5r/s"), Some(5.0));
        assert_eq!(parse_rate("60r/m"), Some(1.0));
        assert_eq!(parse_rate("100r/s "), Some(100.0));
        assert_eq!(parse_rate("nonsense"), None);
        assert_eq!(parse_rate("5r/h"), None);
    }

    #[test]
    fn eviction_caps_entries() {
        let z = LimitReqZone::new(
            "t".into(),
            Template::parse("$remote_addr"),
            0.0001,
            3,
        );
        for i in 0..10 {
            let _ = z.allow(&format!("k{i}"), 0);
        }
        let len = z.state.lock().unwrap().buckets.len();
        assert!(len <= 3, "expected ≤ 3 entries, got {len}");
    }
}
