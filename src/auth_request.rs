//! `auth_request` — delegate authorization to an HTTP service.
//!
//! The directive value is a template; once rendered, it is treated as an
//! absolute URL the subrequest will `GET`. Common client headers
//! (`Authorization`, `Cookie`) are forwarded; we add `X-Original-URI` and
//! `X-Original-Method` so the auth service knows what the user tried to
//! do.
//!
//! A `2xx` response lets the original request proceed. Anything else is
//! returned to the client as-is (preserving the status), except that the
//! body is replaced with a short marker so we don't leak the auth
//! service's internal response.

use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Method, Request};

use crate::body::{text, ElrondBody};
use crate::request_ctx::RequestCtx;
use crate::template::Template;

/// How long to wait on the auth subrequest before treating it as a hard
/// failure. Keep this short — the user is staring at a loading spinner.
const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Headers forwarded from the original request to the auth service.
const FORWARDED_HEADERS: [&str; 3] = ["authorization", "cookie", "user-agent"];

/// Decide whether the request is allowed.
///
/// `Ok(())` → original request proceeds.
/// `Err(response)` → return this response to the client (`401`/`403`/`5xx`).
pub async fn check(
    url_template: &Template,
    ctx: &RequestCtx<'_>,
    client_headers: &HeaderMap,
) -> Result<(), hyper::Response<ElrondBody>> {
    let url_raw = url_template.render(ctx);
    let url_trimmed = url_raw.trim();
    if url_trimmed.is_empty() {
        return Err(text(500, "auth_request: empty URL\n"));
    }

    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(url_trimmed)
        .header("user-agent", "elrond-auth-request");

    for name in FORWARDED_HEADERS {
        if let Some(v) = client_headers.get(name) {
            builder = builder.header(name, v.clone());
        }
    }
    builder = builder
        .header(
            "x-original-uri",
            HeaderValue::from_str(ctx.request_uri())
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        )
        .header(
            "x-original-method",
            HeaderValue::from_str(ctx.method.as_str())
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        );

    let req = match builder.body(empty_body()) {
        Ok(r) => r,
        Err(e) => {
            return Err(text(
                500,
                format!("auth_request: bad subrequest URL '{url_trimmed}': {e}\n"),
            ));
        }
    };

    let fut = crate::proxy::client().request(req);
    let outcome = tokio::time::timeout(AUTH_REQUEST_TIMEOUT, fut).await;
    match outcome {
        Ok(Ok(resp)) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(text(
                    status,
                    format!("{status} (auth_request denied)\n"),
                ))
            }
        }
        Ok(Err(_)) => Err(text(
            500,
            "auth_request: subrequest connect / send error\n",
        )),
        Err(_) => Err(text(
            504,
            "auth_request: subrequest timed out\n",
        )),
    }
}

fn empty_body() -> ElrondBody {
    Empty::<Bytes>::new()
        .map_err(|n: Infallible| match n {})
        .boxed()
}
