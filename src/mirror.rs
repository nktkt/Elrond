//! `mirror` — fire-and-forget shadow requests.
//!
//! Each matched request is replicated to one or more shadow URLs. The
//! original request flow is **never** affected: mirrors are spawned in
//! their own tasks, given a tight timeout, and their responses are
//! discarded.
//!
//! v0.31.0 ships the simple case: only the method, the rendered URL, and
//! a small set of safe headers are mirrored — request bodies are **not**
//! buffered or replayed (that would either inflate memory on every
//! request or change the original response timing).

use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::header::HeaderMap;
use hyper::{Method, Request};
use tracing::debug;

use crate::body::ElrondBody;
use crate::metrics;
use crate::request_ctx::RequestCtx;
use crate::template::Template;

/// Short cap on the shadow round-trip so a slow mirror can't pile up.
const MIRROR_TIMEOUT: Duration = Duration::from_secs(2);

const FORWARDED_HEADERS: [&str; 3] = ["authorization", "cookie", "user-agent"];

/// Spawn one task per configured mirror. Returns immediately; never fails.
pub fn dispatch(
    mirrors: &[Template],
    ctx: &RequestCtx<'_>,
    headers: &HeaderMap,
    method: &Method,
) {
    if mirrors.is_empty() {
        return;
    }
    for tmpl in mirrors {
        let url = tmpl.render(ctx).trim().to_string();
        if url.is_empty() {
            continue;
        }
        let headers = headers.clone();
        let method = method.clone();
        tokio::spawn(async move {
            fire(&url, &headers, &method).await;
        });
    }
}

async fn fire(url: &str, headers: &HeaderMap, method: &Method) {
    metrics::record_mirror_attempt();
    let mut builder = Request::builder()
        .method(method.clone())
        .uri(url)
        .header("user-agent", "elrond-mirror");
    for name in FORWARDED_HEADERS {
        if let Some(v) = headers.get(name) {
            builder = builder.header(name, v.clone());
        }
    }
    builder = builder
        .header(
            "x-elrond-mirror",
            "1",
        );
    let req = match builder.body(empty_body()) {
        Ok(r) => r,
        Err(e) => {
            metrics::record_mirror_failure();
            debug!("mirror: bad shadow URL '{url}': {e}");
            return;
        }
    };
    let fut = crate::proxy::client().request(req);
    match tokio::time::timeout(MIRROR_TIMEOUT, fut).await {
        Ok(Ok(_resp)) => {} // discard
        Ok(Err(e)) => {
            metrics::record_mirror_failure();
            debug!("mirror: '{url}' send error: {e}");
        }
        Err(_) => {
            metrics::record_mirror_failure();
            debug!("mirror: '{url}' timed out after {MIRROR_TIMEOUT:?}");
        }
    }
}

fn empty_body() -> ElrondBody {
    Empty::<Bytes>::new()
        .map_err(|n: Infallible| match n {})
        .boxed()
}
