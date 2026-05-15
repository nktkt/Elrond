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

use crate::app::{Balancer, HeaderList, Peer};
use crate::body::{text, BoxError, ElrondBody};
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
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build_http()
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
    req: Request<Incoming>,
    client_peer: SocketAddr,
    ctx: &RequestCtx<'_>,
) -> Response<ElrondBody> {
    let retry_safe = matches!(
        *req.method(),
        Method::GET | Method::HEAD | Method::OPTIONS | Method::DELETE
    );

    if !retry_safe {
        return forward_once_with_incoming(balancer, set_headers, req, client_peer, ctx).await;
    }

    let (parts, _body) = req.into_parts();
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

        let req2 = build_request(&parts, empty_body());
        match forward_to_peer(&upstream_peer, &set_headers, req2, client_peer, ctx).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (500..600).contains(&status) {
                    upstream_peer.record_failure();
                    excluded.push(upstream_peer.addr.clone());
                    last = Some(resp);
                    continue;
                }
                upstream_peer.record_success();
                return resp;
            }
            Err(()) => {
                upstream_peer.record_failure();
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
    req: Request<Incoming>,
    client_peer: SocketAddr,
    ctx: &RequestCtx<'_>,
) -> Response<ElrondBody> {
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
    match forward_to_peer(&upstream_peer, &set_headers, req2, client_peer, ctx).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (500..600).contains(&status) {
                upstream_peer.record_failure();
            } else {
                upstream_peer.record_success();
            }
            resp
        }
        Err(()) => {
            upstream_peer.record_failure();
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
