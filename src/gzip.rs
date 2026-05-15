//! On-the-fly gzip compression of finite response bodies.
//!
//! Only static and `return` responses are eligible in v0.10.0 — proxied
//! response bodies stream, and v0.10.0 does not yet buffer them for
//! compression. A response is compressed when:
//!
//! 1. The client offers `gzip` in `Accept-Encoding`.
//! 2. The location (or server) has `gzip on`.
//! 3. The response status is `200` / `206` / `203` / `301` / `302`.
//! 4. The response has no `Content-Encoding` already.
//! 5. The response `Content-Type` is on the eligibility list.
//! 6. The body is non-empty.

use std::io::Write;

use bytes::Bytes;
use flate2::write::GzEncoder;
use flate2::Compression;
use http_body_util::BodyExt;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Response, StatusCode};

use crate::body::{full, ElrondBody};

/// MIME types compressed without an explicit `gzip_types` line.
const DEFAULT_TYPES: &[&str] = &[
    "text/html",
    "text/css",
    "text/plain",
    "text/javascript",
    "text/xml",
    "application/javascript",
    "application/x-javascript",
    "application/json",
    "application/xml",
    "application/atom+xml",
    "application/rss+xml",
    "image/svg+xml",
    "font/woff",
    "font/woff2",
];

/// Compress `resp` in place if every condition is met. Returns the response
/// unchanged when compression is skipped, and updates `Content-Encoding`,
/// `Content-Length`, and `Vary` when it isn't.
pub async fn maybe_compress(
    mut resp: Response<ElrondBody>,
    req_headers: &HeaderMap,
    gzip_enabled: bool,
    gzip_types: &[String],
) -> Response<ElrondBody> {
    if !gzip_enabled || !client_accepts_gzip(req_headers) {
        return resp;
    }
    if !status_eligible(resp.status()) {
        return resp;
    }
    if resp.headers().contains_key("content-encoding") {
        return resp;
    }
    if !content_type_eligible(resp.headers(), gzip_types) {
        return resp;
    }

    let (parts, body) = resp.into_parts();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            // Could not buffer body — return an empty body fallback. In
            // practice the body has already been produced by `Full<Bytes>`
            // so this branch is effectively dead for v0.10.0.
            resp = Response::from_parts(parts, full(Bytes::new()));
            return resp;
        }
    };

    // Tiny bodies aren't worth the overhead — Nginx's default cutoff is 20
    // bytes; we match that here.
    if collected.len() < 20 {
        return Response::from_parts(parts, full(collected));
    }

    let compressed = match gzip_bytes(&collected) {
        Ok(c) => c,
        Err(_) => return Response::from_parts(parts, full(collected)),
    };

    let mut new_resp = Response::from_parts(parts, full(compressed.clone()));
    let h = new_resp.headers_mut();
    h.insert(
        "content-encoding",
        HeaderValue::from_static("gzip"),
    );
    h.insert(
        "content-length",
        HeaderValue::from_str(&compressed.len().to_string())
            .expect("integer is a valid header value"),
    );
    // Append Accept-Encoding to Vary, preserving any prior Vary entries.
    let vary = match h.get("vary").and_then(|v| v.to_str().ok()) {
        Some(existing) if !existing.is_empty() => {
            if existing
                .split(',')
                .any(|p| p.trim().eq_ignore_ascii_case("accept-encoding"))
            {
                existing.to_string()
            } else {
                format!("{existing}, Accept-Encoding")
            }
        }
        _ => "Accept-Encoding".to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&vary) {
        h.insert("vary", v);
    }
    new_resp
}

fn client_accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',').any(|p| {
                let p = p.trim();
                let name = p.split(';').next().unwrap_or(p).trim();
                name.eq_ignore_ascii_case("gzip") || name == "*"
            })
        })
        .unwrap_or(false)
}

fn status_eligible(s: StatusCode) -> bool {
    matches!(s.as_u16(), 200 | 203 | 206 | 301 | 302)
}

fn content_type_eligible(headers: &HeaderMap, extra: &[String]) -> bool {
    let Some(ct) = headers.get("content-type").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let mime = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
    DEFAULT_TYPES.iter().any(|t| *t == mime) || extra.iter().any(|t| *t == mime)
}

fn gzip_bytes(input: &[u8]) -> std::io::Result<Bytes> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(input.len() / 2), Compression::default());
    encoder.write_all(input)?;
    Ok(Bytes::from(encoder.finish()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with_accept(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("accept-encoding", value.parse().unwrap());
        h
    }

    #[test]
    fn accept_detection() {
        assert!(client_accepts_gzip(&req_with_accept("gzip")));
        assert!(client_accepts_gzip(&req_with_accept("br, gzip;q=0.9")));
        assert!(client_accepts_gzip(&req_with_accept("*")));
        assert!(!client_accepts_gzip(&req_with_accept("br")));
        assert!(!client_accepts_gzip(&HeaderMap::new()));
    }

    #[test]
    fn content_type_check() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
        assert!(content_type_eligible(&h, &[]));
        let mut h2 = HeaderMap::new();
        h2.insert("content-type", "image/png".parse().unwrap());
        assert!(!content_type_eligible(&h2, &[]));
        assert!(content_type_eligible(&h2, &["image/png".into()]));
    }

    #[tokio::test]
    async fn small_body_is_not_compressed() {
        let mut hresp = HeaderMap::new();
        hresp.insert("content-type", "text/plain".parse().unwrap());
        let resp = Response::builder()
            .status(200)
            .header("content-type", "text/plain")
            .body(full("tiny"))
            .unwrap();
        let req = req_with_accept("gzip");
        let out = maybe_compress(resp, &req, true, &[]).await;
        assert!(!out.headers().contains_key("content-encoding"));
    }

    #[tokio::test]
    async fn eligible_body_is_compressed() {
        let payload =
            "<!doctype html><html><body>".to_string() + &"hello ".repeat(64) + "</body></html>";
        let resp = Response::builder()
            .status(200)
            .header("content-type", "text/html; charset=utf-8")
            .body(full(payload.clone()))
            .unwrap();
        let req = req_with_accept("gzip");
        let out = maybe_compress(resp, &req, true, &[]).await;
        let ce = out
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(ce, "gzip");
        let vary = out
            .headers()
            .get("vary")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(vary.to_lowercase().contains("accept-encoding"));
        let cl: usize = out
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap();
        assert!(cl < payload.len());
    }
}
