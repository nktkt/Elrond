//! Reverse proxy with idempotent-method retry across upstream peers.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tracing::{debug, warn};

use std::time::Instant;

use crate::app::{
    Balancer, HeaderList, Peer, ProxyCache, DEFAULT_PROXY_CONNECT_TIMEOUT,
    DEFAULT_PROXY_READ_TIMEOUT,
};
use crate::body::{full, text, BoxError, ElrondBody};
use crate::cache::{self, CacheDecision, Entry};
use crate::metrics;
use crate::request_ctx::RequestCtx;

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Upper bound on retry attempts (initial + retries). Even a large upstream
/// pool should not loop forever on a flaky cluster.
const MAX_ATTEMPTS: usize = 3;

fn client() -> &'static Client<HttpConnector, ElrondBody> {
    static CLIENT: OnceLock<Client<HttpConnector, ElrondBody>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(DEFAULT_PROXY_CONNECT_TIMEOUT));
        connector.set_nodelay(true);
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(connector)
    })
}

/// Forward `req` through `balancer`. For idempotent methods (`GET`, `HEAD`,
/// `OPTIONS`, `DELETE`) the request is retried on the next peer (up to
/// `MAX_ATTEMPTS` total) on connection errors and 5xx responses. For other
/// methods the request is forwarded once, since retrying after a body has
/// been streamed is not safe.
pub async fn forward(
    balancer: Arc<Balancer>,
    set_headers: HeaderList,
    cache: Option<ProxyCache>,
    read_timeout: Option<Duration>,
    req: Request<Incoming>,
    client_peer: SocketAddr,
    ctx: &RequestCtx<'_>,
) -> Response<ElrondBody> {
    let _ = (DEFAULT_PROXY_CONNECT_TIMEOUT,); // already applied via connector
    let effective_read = read_timeout.unwrap_or(DEFAULT_PROXY_READ_TIMEOUT);
    let retry_safe = matches!(
        *req.method(),
        Method::GET | Method::HEAD | Method::OPTIONS | Method::DELETE
    );

    // Try the cache before any upstream selection. Only GET is consultable.
    if let (Some(c), &Method::GET) = (&cache, req.method()) {
        let key = c.key_template.render(ctx);
        if let Some(entry) = c.store.get(&key) {
            return cached_response(entry);
        }
    }

    if !retry_safe {
        return forward_once_with_incoming(
            balancer,
            set_headers,
            cache,
            effective_read,
            req,
            client_peer,
            ctx,
        )
        .await;
    }

    let (parts, _body) = req.into_parts();
    let method = parts.method.clone();
    let max = MAX_ATTEMPTS.min(balancer.peers.len().max(1));
    let mut excluded: Vec<String> = Vec::new();
    let mut last: Option<Response<ElrondBody>> = None;

    for attempt in 0..max {
        let upstream_peer = match balancer.pick_excluding(ctx, &excluded) {
            Some(p) => p,
            None => break,
        };
        let _guard = upstream_peer.enter();
        debug!(
            "proxy: '{}' attempt {}/{} -> {}",
            balancer.name,
            attempt + 1,
            max,
            upstream_peer.addr
        );

        metrics::record_proxy_attempt();
        let req2 = build_request(&parts, empty_body());
        let attempt = forward_to_peer(&upstream_peer, &set_headers, req2, client_peer, ctx);
        match tokio::time::timeout(effective_read, attempt).await.unwrap_or_else(|_| {
            warn!("proxy: upstream '{}' read timed out after {:?}", upstream_peer.addr, effective_read);
            Err(())
        }) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (500..600).contains(&status) {
                    upstream_peer.record_failure();
                    metrics::record_proxy_failure();
                    excluded.push(upstream_peer.addr.clone());
                    last = Some(resp);
                    continue;
                }
                upstream_peer.record_success();
                return maybe_cache(resp, cache.as_ref(), &method, ctx).await;
            }
            Err(()) => {
                upstream_peer.record_failure();
                metrics::record_proxy_failure();
                excluded.push(upstream_peer.addr.clone());
                continue;
            }
        }
    }

    last.unwrap_or_else(|| text(502, "502 Bad Gateway\n"))
}

/// Single-shot forwarding path used for non-idempotent methods. The original
/// request body is forwarded as-is; no retry is attempted.
async fn forward_once_with_incoming(
    balancer: Arc<Balancer>,
    set_headers: HeaderList,
    cache: Option<ProxyCache>,
    effective_read: Duration,
    req: Request<Incoming>,
    client_peer: SocketAddr,
    ctx: &RequestCtx<'_>,
) -> Response<ElrondBody> {
    let _ = cache; // never-cache path (non-GET); kept for symmetry
    let upstream_peer = match balancer.pick(ctx) {
        Some(p) => p,
        None => {
            warn!("proxy: balancer '{}' has no available peers", balancer.name);
            return text(502, "502 Bad Gateway\n");
        }
    };
    let _guard = upstream_peer.enter();

    let (parts, body) = req.into_parts();
    let boxed = body.map_err(|e| Box::new(e) as BoxError).boxed();
    let req2 = Request::from_parts(parts, boxed);
    metrics::record_proxy_attempt();
    let attempt = forward_to_peer(&upstream_peer, &set_headers, req2, client_peer, ctx);
    match tokio::time::timeout(effective_read, attempt).await.unwrap_or_else(|_| {
        warn!(
            "proxy: upstream '{}' read timed out after {:?}",
            upstream_peer.addr, effective_read
        );
        Err(())
    }) {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (500..600).contains(&status) {
                upstream_peer.record_failure();
                metrics::record_proxy_failure();
            } else {
                upstream_peer.record_success();
            }
            resp
        }
        Err(()) => {
            upstream_peer.record_failure();
            metrics::record_proxy_failure();
            text(502, "502 Bad Gateway\n")
        }
    }
}

async fn forward_to_peer(
    peer: &Arc<Peer>,
    set_headers: &[(HeaderName, crate::template::Template)],
    req: Request<ElrondBody>,
    client_peer: SocketAddr,
    ctx: &RequestCtx<'_>,
) -> Result<Response<ElrondBody>, ()> {
    let upstream = peer.addr.clone();
    let (mut parts, body) = req.into_parts();

    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let uri: Uri = match format!("http://{upstream}{pq}").parse() {
        Ok(u) => u,
        Err(e) => {
            warn!("proxy: invalid upstream uri for '{upstream}': {e}");
            return Err(());
        }
    };
    parts.uri = uri;

    for h in HOP_BY_HOP {
        parts.headers.remove(h);
    }
    add_forwarding_headers(&mut parts.headers, client_peer);
    apply_proxy_set_headers(&mut parts.headers, set_headers, ctx);

    let outbound = Request::from_parts(parts, body);
    match client().request(outbound).await {
        Ok(resp) => {
            let (mut rparts, rbody) = resp.into_parts();
            for h in HOP_BY_HOP {
                rparts.headers.remove(h);
            }
            let rbody = rbody.map_err(|e| Box::new(e) as BoxError).boxed();
            Ok(Response::from_parts(rparts, rbody))
        }
        Err(e) => {
            warn!("proxy: upstream '{upstream}' error: {e}");
            Err(())
        }
    }
}

/// Clone a `Parts` value for retry. `Parts` doesn't implement `Clone`, so we
/// rebuild it field by field from an empty template.
fn build_request(original: &Parts, body: ElrondBody) -> Request<ElrondBody> {
    let mut parts = Request::new(()).into_parts().0;
    parts.method = original.method.clone();
    parts.uri = original.uri.clone();
    parts.version = original.version;
    parts.headers = original.headers.clone();
    Request::from_parts(parts, body)
}

fn empty_body() -> ElrondBody {
    Empty::<Bytes>::new()
        .map_err(|n: Infallible| match n {})
        .boxed()
}

/// Serve a cached response. Attaches an `X-Cache: HIT` header.
fn cached_response(entry: Entry) -> Response<ElrondBody> {
    let mut b = Response::builder().status(entry.status);
    for (n, v) in &entry.headers {
        b = b.header(n, v);
    }
    b = b.header("x-cache", "HIT");
    b.body(full(entry.body))
        .expect("cached response is well-formed")
}

/// If the location has a cache and the response is cacheable, buffer the
/// body and store it. Returns the response (possibly with `X-Cache: MISS`
/// / `X-Cache: BYPASS`). Non-cached responses still get an `X-Cache` header
/// so operators can grep for misses.
async fn maybe_cache(
    resp: Response<ElrondBody>,
    cache: Option<&ProxyCache>,
    method: &Method,
    ctx: &RequestCtx<'_>,
) -> Response<ElrondBody> {
    let Some(c) = cache else {
        return resp;
    };
    if *method != Method::GET {
        return mark_cache(resp, "BYPASS");
    }

    let (parts, body) = resp.into_parts();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return Response::from_parts(parts, full(bytes::Bytes::new()));
        }
    };

    let decision = cache::decide_caching(
        method,
        &parts.headers,
        parts.status.as_u16(),
        collected.len(),
        &c.valid_rules,
    );
    match decision {
        CacheDecision::Bypass(_) => {
            metrics::record_cache_bypass();
            let resp = Response::from_parts(parts, full(collected));
            mark_cache(resp, "BYPASS")
        }
        CacheDecision::Store(ttl) => {
            let key = c.key_template.render(ctx);
            let header_pairs: Vec<(HeaderName, HeaderValue)> = parts
                .headers
                .iter()
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
            let entry = Entry {
                status: parts.status.as_u16(),
                headers: header_pairs,
                body: collected.clone(),
                expires_at: Instant::now() + ttl,
            };
            c.store.put(key, entry);
            let resp = Response::from_parts(parts, full(collected));
            mark_cache(resp, "MISS")
        }
    }
}

fn mark_cache(mut resp: Response<ElrondBody>, label: &'static str) -> Response<ElrondBody> {
    if let Ok(v) = HeaderValue::from_str(label) {
        resp.headers_mut().insert("x-cache", v);
    }
    resp
}

fn add_forwarding_headers(headers: &mut hyper::HeaderMap, peer: SocketAddr) {
    let ip = peer.ip().to_string();
    let Ok(ip_value) = HeaderValue::from_str(&ip) else {
        return;
    };
    headers.insert("x-real-ip", ip_value.clone());

    match headers.get("x-forwarded-for") {
        Some(existing) => {
            let chain = format!("{}, {ip}", existing.to_str().unwrap_or(""));
            if let Ok(v) = HeaderValue::from_str(&chain) {
                headers.insert("x-forwarded-for", v);
            }
        }
        None => {
            headers.insert("x-forwarded-for", ip_value);
        }
    }
}

fn apply_proxy_set_headers(
    headers: &mut hyper::HeaderMap,
    list: &[(HeaderName, crate::template::Template)],
    ctx: &RequestCtx<'_>,
) {
    for (name, tmpl) in list {
        let value = tmpl.render(ctx);
        if value.is_empty() {
            headers.remove(name);
        } else if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert(name, v);
        } else {
            debug!("proxy_set_header: invalid value for '{name}'");
        }
    }
}
