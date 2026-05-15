//! Static file serving for the `root` and `alias` directives.

use std::path::{Component, Path, PathBuf};

use hyper::Response;
use tokio::fs;
use tracing::debug;

use crate::app::StaticKind;
use crate::body::{full, text, ElrondBody};

/// Serve `req_path` from `root`.
///
/// - `StaticKind::Root` follows Nginx `root`: filesystem path = `root` + full
///   request URI path.
/// - `StaticKind::Alias { prefix }` follows Nginx `alias`: filesystem path =
///   `root` + (URI path - location prefix).
///
/// Requests ending in `/` (or naming a directory) fall back to `index.html`.
/// Paths containing `..` or other non-normal components are rejected `403`.
pub async fn serve(
    root: &Path,
    kind: &StaticKind,
    req_path: &str,
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

    let is_dir = req_path.ends_with('/')
        || fs::metadata(&full_path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
    if is_dir {
        full_path = full_path.join("index.html");
    }

    match fs::read(&full_path).await {
        Ok(bytes) => Response::builder()
            .status(200)
            .header("content-type", mime_for(&full_path))
            .body(full(bytes))
            .expect("a static file response is always well-formed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            text(404, "404 Not Found\n")
        }
        Err(e) => {
            debug!("static: error reading '{}': {e}", full_path.display());
            text(403, "403 Forbidden\n")
        }
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
