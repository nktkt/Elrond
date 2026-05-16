//! Static file serving: `root`, `alias`, `Range`, conditional GET, and
//! `Last-Modified` / `ETag` (weak).

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use hyper::{HeaderMap, Method, Response};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::debug;

use crate::app::{StaticKind, TryFilesEntryRt};
use crate::body::{full, text, ElrondBody};
use crate::http_date;
use crate::request_ctx::RequestCtx;

/// Serve `req_path` from the configured `root`/`alias`, honoring `Range`,
/// `If-None-Match`, and `HEAD` semantics. Path traversal is rejected at the
/// component level.
pub async fn serve(
    root: &Path,
    kind: &StaticKind,
    req_path: &str,
    req_headers: &HeaderMap,
    req_method: &Method,
    autoindex: bool,
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
        let index_path = full_path.join("index.html");
        match fs::metadata(&index_path).await {
            Ok(m) => {
                full_path = index_path;
                m
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if autoindex {
                    return render_directory_listing(&full_path, req_path).await;
                }
                return text(404, "404 Not Found\n");
            }
            Err(e) => {
                debug!("static: stat index error '{}': {e}", index_path.display());
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

/// Render an HTML directory listing. Dotfiles are skipped. Entries are
/// sorted directories-first, then by name.
async fn render_directory_listing(dir: &Path, uri_path: &str) -> Response<ElrondBody> {
    let mut rd = match fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return text(403, "403 Forbidden\n"),
    };
    let mut entries: Vec<(String, bool)> = Vec::new();
    loop {
        match rd.next_entry().await {
            Ok(Some(e)) => {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                entries.push((name, is_dir));
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    entries.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });

    let mut html = String::with_capacity(1024);
    html.push_str("<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>Index of ");
    push_html_escaped(&mut html, uri_path);
    html.push_str(
        "</title>\n<style>body{font:14px/1.5 system-ui,sans-serif;padding:1.5rem;max-width:60rem;margin:auto}h1{font-size:1.25rem;margin-bottom:1rem}pre{margin:0}a{text-decoration:none;color:inherit}a:hover{text-decoration:underline}</style>\n</head>\n<body>\n<h1>Index of ",
    );
    push_html_escaped(&mut html, uri_path);
    html.push_str("</h1>\n<pre>");
    if uri_path != "/" && uri_path != "" {
        html.push_str("<a href=\"../\">../</a>\n");
    }
    for (name, is_dir) in &entries {
        let display = if *is_dir { format!("{name}/") } else { name.clone() };
        let href = if *is_dir {
            format!("{}/", url_encode_segment(name))
        } else {
            url_encode_segment(name)
        };
        html.push_str("<a href=\"");
        push_attr_escaped(&mut html, &href);
        html.push_str("\">");
        push_html_escaped(&mut html, &display);
        html.push_str("</a>\n");
    }
    html.push_str("</pre>\n</body>\n</html>\n");

    Response::builder()
        .status(200)
        .header("content-type", "text/html; charset=utf-8")
        .body(full(html))
        .expect("autoindex response is well-formed")
}

fn push_html_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
}

fn push_attr_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

/// Percent-encode bytes that would interfere with URL parsing in a path
/// segment. Conservative: encodes everything outside the unreserved set.
fn url_encode_segment(s: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*b as char),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
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

/// Resolve a `try_files` location. Each non-final entry is treated as a
/// path-existence check rooted at `root`; the first one that exists is
/// served. The final entry is always honored — either as a path (served
/// regardless) or as a `=NNN` status code.
pub async fn try_files(
    root: &Path,
    entries: &[TryFilesEntryRt],
    ctx: &RequestCtx<'_>,
    req_headers: &hyper::HeaderMap,
    req_method: &Method,
) -> Response<ElrondBody> {
    if entries.is_empty() {
        return text(500, "500 Internal Server Error\n");
    }
    let last_i = entries.len() - 1;
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            TryFilesEntryRt::Status(code) => {
                // Only the last entry may be a status; we've already
                // refused others at parse time.
                if i == last_i {
                    return text(*code, format!("{} \n", *code));
                }
            }
            TryFilesEntryRt::Path(tmpl) => {
                let rendered = tmpl.render(ctx);
                let rel = rendered.trim_start_matches('/');
                let candidate = root.join(rel);
                // Reject obvious traversal via component check before
                // touching the filesystem.
                if !components_safe(Path::new(rel)) {
                    if i == last_i {
                        return text(403, "403 Forbidden\n");
                    } else {
                        continue;
                    }
                }
                match fs::metadata(&candidate).await {
                    Ok(meta) if meta.is_file() => {
                        return serve(
                            root,
                            &StaticKind::Root,
                            &rendered,
                            req_headers,
                            req_method,
                            false,
                        )
                        .await;
                    }
                    _ if i == last_i => {
                        // Last entry: serve unconditionally (typical SPA
                        // fallback `/index.html`). If it doesn't exist, the
                        // underlying serve will produce 404 / 403.
                        return serve(
                            root,
                            &StaticKind::Root,
                            &rendered,
                            req_headers,
                            req_method,
                            false,
                        )
                        .await;
                    }
                    _ => continue,
                }
            }
        }
    }
    text(404, "404 Not Found\n")
}

fn components_safe(p: &Path) -> bool {
    use std::path::Component;
    p.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
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
