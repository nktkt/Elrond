//! Static file serving: `root`, `alias`, `Range`, conditional GET, and
//! `Last-Modified` / `ETag` (weak).

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use hyper::{HeaderMap, Method, Response};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::debug;

use crate::app::StaticKind;
use crate::body::{full, text, ElrondBody};
use crate::http_date;

/// Serve `req_path` from the configured `root`/`alias`, honoring `Range`,
/// `If-None-Match`, and `HEAD` semantics. Path traversal is rejected at the
/// component level.
pub async fn serve(
    root: &Path,
    kind: &StaticKind,
    req_path: &str,
    req_headers: &HeaderMap,
    req_method: &Method,
) -> Response<ElrondBody> {
    let rel = match kind {
        StaticKind::Root => req_path.trim_start_matches('/').to_string(),
        StaticKind::Alias { prefix } => {
            let stripped = req_path.strip_prefix(prefix.as_str()).unwrap_or(req_path);
            stripped.trim_start_matches('/').to_string()
        }
    };

    let rel_path = PathBuf::from(&rel);
    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                debug!("static: rejected suspicious path '{req_path}'");
                return text(403, "403 Forbidden\n");
            }
        }
    }

    let mut full_path = root.join(&rel_path);

    let meta = match fs::metadata(&full_path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return text(404, "404 Not Found\n");
        }
        Err(e) => {
            debug!("static: stat error '{}': {e}", full_path.display());
            return text(403, "403 Forbidden\n");
        }
    };
    let meta = if meta.is_dir() {
        full_path = full_path.join("index.html");
        match fs::metadata(&full_path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return text(404, "404 Not Found\n");
            }
            Err(e) => {
                debug!("static: stat index error '{}': {e}", full_path.display());
                return text(403, "403 Forbidden\n");
            }
        }
    } else {
        meta
    };

    let size = meta.len();
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let etag = mtime_secs.map(|s| format!("W/\"{size:x}-{s:x}\""));
    let ctype = mime_for(&full_path);
    let head_only = req_method == Method::HEAD;

    // Conditional revalidation via ETag.
    if let Some(et) = &etag {
        if let Some(v) = req_headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
            if v.split(',').map(str::trim).any(|tok| tok == et || tok == "*") {
                return not_modified(et, mtime_secs);
            }
        }
    }

    // Range: handle a single `bytes=START-END` range. Multipart ranges and
    // multi-range responses are deferred.
    if let Some(spec) = req_headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|r| parse_single_range(r, size))
    {
        let (start, end) = spec;
        let length = end - start + 1;
        let body_bytes = if head_only {
            Bytes::new()
        } else {
            match read_range(&full_path, start, length).await {
                Ok(b) => b,
                Err(e) => {
                    debug!("static: range read '{}': {e}", full_path.display());
                    return text(500, "500 Internal Server Error\n");
                }
            }
        };
        let mut b = Response::builder()
            .status(206)
            .header("content-type", ctype)
            .header("content-length", length)
            .header("accept-ranges", "bytes")
            .header("content-range", format!("bytes {start}-{end}/{size}"));
        if let Some(et) = &etag {
            b = b.header("etag", et);
        }
        if let Some(m) = mtime_secs {
            b = b.header("last-modified", http_date::format(m));
        }
        return b
            .body(full(body_bytes))
            .expect("range response is well-formed");
    } else if let Some(v) = req_headers.get("range").and_then(|v| v.to_str().ok()) {
        // Range header present but unsatisfiable.
        if !v.starts_with("bytes=") {
            // ignore — not a byte range we understand
        } else {
            return Response::builder()
                .status(416)
                .header("content-type", "text/plain; charset=utf-8")
                .header("content-range", format!("bytes */{size}"))
                .body(full("416 Range Not Satisfiable\n"))
                .expect("416 response is well-formed");
        }
    }

    // Full body.
    let body_bytes = if head_only {
        Bytes::new()
    } else {
        match fs::read(&full_path).await {
            Ok(b) => Bytes::from(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return text(404, "404 Not Found\n");
            }
            Err(e) => {
                debug!("static: read '{}': {e}", full_path.display());
                return text(403, "403 Forbidden\n");
            }
        }
    };

    let mut b = Response::builder()
        .status(200)
        .header("content-type", ctype)
        .header("accept-ranges", "bytes");
    if head_only {
        b = b.header("content-length", size);
    }
    if let Some(et) = &etag {
        b = b.header("etag", et);
    }
    if let Some(m) = mtime_secs {
        b = b.header("last-modified", http_date::format(m));
    }
    b.body(full(body_bytes))
        .expect("a static file response is always well-formed")
}

async fn read_range(
    path: &Path,
    start: u64,
    length: u64,
) -> std::io::Result<Bytes> {
    use std::io::SeekFrom;
    let mut f = fs::File::open(path).await?;
    f.seek(SeekFrom::Start(start)).await?;
    let cap = usize::try_from(length).unwrap_or(usize::MAX);
    let mut buf = vec![0u8; cap];
    f.read_exact(&mut buf).await?;
    Ok(Bytes::from(buf))
}

fn not_modified(etag: &str, mtime_secs: Option<u64>) -> Response<ElrondBody> {
    let mut b = Response::builder().status(304).header("etag", etag);
    if let Some(m) = mtime_secs {
        b = b.header("last-modified", http_date::format(m));
    }
    b.body(full(Bytes::new()))
        .expect("304 response is well-formed")
}

/// Parse a single-range request header (`bytes=start-end`, `bytes=start-`,
/// `bytes=-suffix`). Returns the inclusive `(start, end)` byte indices, or
/// `None` if the spec is invalid or unsatisfiable.
fn parse_single_range(raw: &str, size: u64) -> Option<(u64, u64)> {
    let rest = raw.strip_prefix("bytes=")?;
    if rest.contains(',') {
        // Multi-range — deferred; treat as unsatisfiable for v0.5.0.
        return None;
    }
    let (a, b) = rest.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if size == 0 {
        return None;
    }
    if a.is_empty() {
        // suffix range: last N bytes
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        let n = n.min(size);
        Some((size - n, size - 1))
    } else if b.is_empty() {
        let start: u64 = a.parse().ok()?;
        if start >= size {
            return None;
        }
        Some((start, size - 1))
    } else {
        let start: u64 = a.parse().ok()?;
        let end: u64 = b.parse().ok()?;
        if start > end || start >= size {
            return None;
        }
        let end = end.min(size - 1);
        Some((start, end))
    }
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" | "text" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::parse_single_range;

    #[test]
    fn full_range() {
        assert_eq!(parse_single_range("bytes=0-9", 100), Some((0, 9)));
    }
    #[test]
    fn open_end() {
        assert_eq!(parse_single_range("bytes=50-", 100), Some((50, 99)));
    }
    #[test]
    fn suffix() {
        assert_eq!(parse_single_range("bytes=-20", 100), Some((80, 99)));
    }
    #[test]
    fn clamp_end() {
        assert_eq!(parse_single_range("bytes=10-999", 100), Some((10, 99)));
    }
    #[test]
    fn out_of_range_rejected() {
        assert!(parse_single_range("bytes=200-300", 100).is_none());
    }
    #[test]
    fn multi_range_rejected_for_now() {
        assert!(parse_single_range("bytes=0-9,20-29", 100).is_none());
    }
    #[test]
    fn invalid_prefix_rejected() {
        assert!(parse_single_range("rows=0-9", 100).is_none());
    }
}
