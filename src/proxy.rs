//! Reverse proxy: forward a request to a balancer-selected upstream and
//! stream the response back.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Request, Response, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tracing::{debug, warn};

use crate::app::Balancer;
use crate::body::{text, BoxError, ElrondBody};

/// Hop-by-hop headers, which must not be forwarded across a proxy
/// (RFC 9110 §7.6.1). The connection itself is managed independently on each
/// side, so these are stripped in both directions.
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

/// Process-wide upstream connection pool, shared across all proxied requests.
fn client() -> &'static Client<HttpConnector, ElrondBody> {
    static CLIENT: OnceLock<Client<HttpConnector, ElrondBody>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build_http()
    })
}

/// Forward `req` to an upstream chosen by `balancer`, returning the upstream
/// response (or a `502` if the upstream cannot be reached).
pub async fn forward(
    balancer: Arc<Balancer>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Response<ElrondBody> {
    let upstream = balancer.pick().to_string();
    debug!("proxy: '{}' -> {upstream}", balancer.name);
    let (mut parts, body) = req.into_parts();

    // Rewrite the request-target to point at the chosen upstream.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let uri: Uri = match format!("http://{upstream}{path_and_query}").parse() {
        Ok(u) => u,
        Err(e) => {
            warn!("proxy: invalid upstream uri for '{upstream}': {e}");
            return text(502, "502 Bad Gateway\n");
        }
    };
    parts.uri = uri;

    for h in HOP_BY_HOP {
        parts.headers.remove(h);
    }
    add_forwarding_headers(&mut parts.headers, peer);

    let body = body.map_err(|e| Box::new(e) as BoxError).boxed();
    let outbound = Request::from_parts(parts, body);

    match client().request(outbound).await {
        Ok(resp) => {
            let (mut rparts, rbody) = resp.into_parts();
            for h in HOP_BY_HOP {
                rparts.headers.remove(h);
            }
            let rbody = rbody.map_err(|e| Box::new(e) as BoxError).boxed();
            Response::from_parts(rparts, rbody)
        }
        Err(e) => {
            warn!("proxy: upstream '{upstream}' error: {e}");
            text(502, "502 Bad Gateway\n")
        }
    }
}

/// Set `X-Real-IP` and append the client to `X-Forwarded-For`.
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
