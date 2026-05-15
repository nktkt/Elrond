//! Reverse proxy: forward a request to a balancer-selected upstream and
//! stream the response back.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tracing::{debug, warn};

use crate::app::{Balancer, HeaderList};
use crate::body::{text, BoxError, ElrondBody};
use crate::request_ctx::RequestCtx;

/// Hop-by-hop headers, which must not be forwarded across a proxy
/// (RFC 9110 §7.6.1).
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

fn client() -> &'static Client<HttpConnector, ElrondBody> {
    static CLIENT: OnceLock<Client<HttpConnector, ElrondBody>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build_http()
    })
}

pub async fn forward(
    balancer: Arc<Balancer>,
    set_headers: HeaderList,
    req: Request<Incoming>,
    peer: SocketAddr,
    ctx: &RequestCtx<'_>,
) -> Response<ElrondBody> {
    let upstream = balancer.pick().to_string();
    debug!("proxy: '{}' -> {upstream}", balancer.name);
    let (mut parts, body) = req.into_parts();

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
    apply_proxy_set_headers(&mut parts.headers, &set_headers, ctx);

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

/// Render and apply per-location `proxy_set_header` directives. An empty
/// rendered value removes the header (matching Nginx semantics).
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
