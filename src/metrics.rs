//! Process-wide metrics, exposed in Prometheus text-exposition format.
//!
//! Counters are atomic and updated from the request/connection hot path.
//! The `/metrics` endpoint is plumbed through the configuration via the
//! `metrics;` content directive in a `location` block.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

static REQS: AtomicU64 = AtomicU64::new(0);
static RESP_1XX: AtomicU64 = AtomicU64::new(0);
static RESP_2XX: AtomicU64 = AtomicU64::new(0);
static RESP_3XX: AtomicU64 = AtomicU64::new(0);
static RESP_4XX: AtomicU64 = AtomicU64::new(0);
static RESP_5XX: AtomicU64 = AtomicU64::new(0);
static CONNS_ACCEPTED: AtomicU64 = AtomicU64::new(0);
static ACTIVE_CONNS: AtomicU64 = AtomicU64::new(0);
static PROXY_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static PROXY_FAILURES: AtomicU64 = AtomicU64::new(0);
static TLS_OK: AtomicU64 = AtomicU64::new(0);
static TLS_FAIL: AtomicU64 = AtomicU64::new(0);
static STREAM_ACCEPTED: AtomicU64 = AtomicU64::new(0);
static STREAM_ACTIVE: AtomicU64 = AtomicU64::new(0);
static STREAM_BYTES_TO_UP: AtomicU64 = AtomicU64::new(0);
static STREAM_BYTES_FROM_UP: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static CACHE_BYPASS: AtomicU64 = AtomicU64::new(0);
static CACHE_BYTES: AtomicU64 = AtomicU64::new(0);
static CACHE_ENTRIES: AtomicU64 = AtomicU64::new(0);
static CACHE_EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIMIT_REQ_ALLOWED: AtomicU64 = AtomicU64::new(0);
static LIMIT_REQ_DENIED: AtomicU64 = AtomicU64::new(0);

static START: OnceLock<Instant> = OnceLock::new();

/// Install the process-start timestamp. Safe to call multiple times.
pub fn init() {
    let _ = START.get_or_init(Instant::now);
}

/// Record one request and its response status class.
pub fn record_request(status: u16) {
    REQS.fetch_add(1, Ordering::Relaxed);
    match status / 100 {
        1 => &RESP_1XX,
        2 => &RESP_2XX,
        3 => &RESP_3XX,
        4 => &RESP_4XX,
        _ => &RESP_5XX,
    }
    .fetch_add(1, Ordering::Relaxed);
}

pub fn record_conn_accepted() {
    CONNS_ACCEPTED.fetch_add(1, Ordering::Relaxed);
}
pub fn record_proxy_attempt() {
    PROXY_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
}
pub fn record_proxy_failure() {
    PROXY_FAILURES.fetch_add(1, Ordering::Relaxed);
}
pub fn record_tls_handshake_success() {
    TLS_OK.fetch_add(1, Ordering::Relaxed);
}
pub fn record_tls_handshake_failure() {
    TLS_FAIL.fetch_add(1, Ordering::Relaxed);
}
pub fn record_stream_accepted() {
    STREAM_ACCEPTED.fetch_add(1, Ordering::Relaxed);
}
pub fn record_stream_bytes(to_upstream: u64, from_upstream: u64) {
    STREAM_BYTES_TO_UP.fetch_add(to_upstream, Ordering::Relaxed);
    STREAM_BYTES_FROM_UP.fetch_add(from_upstream, Ordering::Relaxed);
}

pub fn record_cache_hit() {
    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}
pub fn record_cache_miss() {
    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}
pub fn record_cache_bypass() {
    CACHE_BYPASS.fetch_add(1, Ordering::Relaxed);
}
pub fn record_cache_evict(bytes: usize) {
    CACHE_EVICTED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}
pub fn set_cache_bytes(v: u64) {
    CACHE_BYTES.store(v, Ordering::Relaxed);
}
pub fn set_cache_entries(v: u64) {
    CACHE_ENTRIES.store(v, Ordering::Relaxed);
}
pub fn record_limit_req_allowed() {
    LIMIT_REQ_ALLOWED.fetch_add(1, Ordering::Relaxed);
}
pub fn record_limit_req_denied() {
    LIMIT_REQ_DENIED.fetch_add(1, Ordering::Relaxed);
}

/// RAII guard for in-flight stream (TCP-proxy) connections.
pub struct StreamConnGuard;

impl StreamConnGuard {
    pub fn new() -> Self {
        STREAM_ACTIVE.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for StreamConnGuard {
    fn drop(&mut self) {
        STREAM_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// RAII guard that increments the active-connection gauge on construction
/// and decrements it when the connection task drops.
pub struct ConnGuard;

impl ConnGuard {
    pub fn new() -> Self {
        ACTIVE_CONNS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        ACTIVE_CONNS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Render Prometheus text-exposition output. `version` is interpolated into
/// `elrond_build_info`.
pub fn render(version: &str) -> String {
    let uptime = START.get().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
    let mut out = String::with_capacity(2048);

    writeln!(out, "# HELP elrond_build_info Build info.").ok();
    writeln!(out, "# TYPE elrond_build_info gauge").ok();
    writeln!(out, "elrond_build_info{{version=\"{version}\"}} 1").ok();

    writeln!(out, "# HELP elrond_uptime_seconds Process uptime in seconds.").ok();
    writeln!(out, "# TYPE elrond_uptime_seconds gauge").ok();
    writeln!(out, "elrond_uptime_seconds {uptime:.3}").ok();

    let counters: &[(&str, u64, &str)] = &[
        (
            "elrond_requests_total",
            REQS.load(Ordering::Relaxed),
            "Total HTTP requests handled.",
        ),
        (
            "elrond_connections_accepted_total",
            CONNS_ACCEPTED.load(Ordering::Relaxed),
            "Total connections accepted by every listener.",
        ),
        (
            "elrond_proxy_attempts_total",
            PROXY_ATTEMPTS.load(Ordering::Relaxed),
            "Total proxy attempts (including retries).",
        ),
        (
            "elrond_proxy_failures_total",
            PROXY_FAILURES.load(Ordering::Relaxed),
            "Total proxy attempts that failed at the connection level.",
        ),
        (
            "elrond_tls_handshakes_total",
            TLS_OK.load(Ordering::Relaxed),
            "Successful TLS handshakes.",
        ),
        (
            "elrond_tls_handshake_failures_total",
            TLS_FAIL.load(Ordering::Relaxed),
            "Failed TLS handshakes.",
        ),
        (
            "elrond_stream_connections_accepted_total",
            STREAM_ACCEPTED.load(Ordering::Relaxed),
            "Total TCP stream connections accepted.",
        ),
        (
            "elrond_stream_bytes_client_to_upstream_total",
            STREAM_BYTES_TO_UP.load(Ordering::Relaxed),
            "Total bytes piped from stream clients to upstreams.",
        ),
        (
            "elrond_stream_bytes_upstream_to_client_total",
            STREAM_BYTES_FROM_UP.load(Ordering::Relaxed),
            "Total bytes piped from stream upstreams back to clients.",
        ),
        (
            "elrond_cache_hits_total",
            CACHE_HITS.load(Ordering::Relaxed),
            "Proxy cache hits.",
        ),
        (
            "elrond_cache_misses_total",
            CACHE_MISSES.load(Ordering::Relaxed),
            "Proxy cache misses.",
        ),
        (
            "elrond_cache_bypass_total",
            CACHE_BYPASS.load(Ordering::Relaxed),
            "Proxy cache bypasses (safety guard tripped).",
        ),
        (
            "elrond_cache_evicted_bytes_total",
            CACHE_EVICTED_BYTES.load(Ordering::Relaxed),
            "Bytes removed from the proxy cache through eviction or expiry.",
        ),
        (
            "elrond_limit_req_allowed_total",
            LIMIT_REQ_ALLOWED.load(Ordering::Relaxed),
            "Requests allowed by limit_req.",
        ),
        (
            "elrond_limit_req_denied_total",
            LIMIT_REQ_DENIED.load(Ordering::Relaxed),
            "Requests denied (503) by limit_req.",
        ),
    ];
    for (name, value, help) in counters {
        writeln!(out, "# HELP {name} {help}").ok();
        writeln!(out, "# TYPE {name} counter").ok();
        writeln!(out, "{name} {value}").ok();
    }

    writeln!(
        out,
        "# HELP elrond_responses_total Responses by status class."
    )
    .ok();
    writeln!(out, "# TYPE elrond_responses_total counter").ok();
    for (class, slot) in [
        ("1xx", &RESP_1XX),
        ("2xx", &RESP_2XX),
        ("3xx", &RESP_3XX),
        ("4xx", &RESP_4XX),
        ("5xx", &RESP_5XX),
    ] {
        writeln!(
            out,
            "elrond_responses_total{{status_class=\"{class}\"}} {}",
            slot.load(Ordering::Relaxed)
        )
        .ok();
    }

    writeln!(
        out,
        "# HELP elrond_active_connections Currently open client connections."
    )
    .ok();
    writeln!(out, "# TYPE elrond_active_connections gauge").ok();
    writeln!(
        out,
        "elrond_active_connections {}",
        ACTIVE_CONNS.load(Ordering::Relaxed)
    )
    .ok();

    writeln!(
        out,
        "# HELP elrond_stream_active_connections Currently open stream (TCP) connections."
    )
    .ok();
    writeln!(out, "# TYPE elrond_stream_active_connections gauge").ok();
    writeln!(
        out,
        "elrond_stream_active_connections {}",
        STREAM_ACTIVE.load(Ordering::Relaxed)
    )
    .ok();

    writeln!(
        out,
        "# HELP elrond_cache_bytes Bytes currently held in the proxy cache."
    )
    .ok();
    writeln!(out, "# TYPE elrond_cache_bytes gauge").ok();
    writeln!(out, "elrond_cache_bytes {}", CACHE_BYTES.load(Ordering::Relaxed)).ok();

    writeln!(
        out,
        "# HELP elrond_cache_entries Number of entries currently in the proxy cache."
    )
    .ok();
    writeln!(out, "# TYPE elrond_cache_entries gauge").ok();
    writeln!(out, "elrond_cache_entries {}", CACHE_ENTRIES.load(Ordering::Relaxed)).ok();

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_required_metrics() {
        init();
        record_request(200);
        record_request(404);
        record_request(502);
        record_proxy_attempt();
        record_proxy_attempt();
        record_proxy_failure();
        let s = render("test-1.2.3");
        assert!(s.contains("elrond_build_info{version=\"test-1.2.3\"} 1"));
        assert!(s.contains("elrond_requests_total"));
        assert!(s.contains("elrond_responses_total{status_class=\"2xx\"}"));
        assert!(s.contains("elrond_responses_total{status_class=\"4xx\"}"));
        assert!(s.contains("elrond_responses_total{status_class=\"5xx\"}"));
        assert!(s.contains("elrond_proxy_attempts_total"));
        assert!(s.contains("elrond_proxy_failures_total"));
        assert!(s.contains("elrond_active_connections"));
        // Every "# TYPE" line should be one of the documented metric types.
        for line in s.lines().filter(|l| l.starts_with("# TYPE ")) {
            let suffix = line.rsplit(' ').next().unwrap();
            assert!(
                matches!(suffix, "counter" | "gauge" | "histogram" | "summary"),
                "unknown metric type in: {line}"
            );
        }
    }
}
