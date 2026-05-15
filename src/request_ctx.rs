//! Per-request context used by the variable engine and access logging.

use std::net::SocketAddr;

use hyper::{HeaderMap, Method, Uri};

/// Snapshot of the parts of a request needed to render templates.
///
/// Borrows everything so it can be built cheaply per request from a cloned
/// `Method`, `Uri`, and `HeaderMap`. Owned cloning happens once at the entry
/// to the handler in [`crate::server`] so that the original request body can
/// still be moved into the proxy without conflicting borrows.
pub struct RequestCtx<'a> {
    pub peer: SocketAddr,
    pub server_name: Option<&'a str>,
    pub method: &'a Method,
    pub uri: &'a Uri,
    pub headers: &'a HeaderMap,
    pub scheme: &'a str,
}

impl<'a> RequestCtx<'a> {
    /// Resolve `$host`: the request's `Host` header, or — for HTTP/2 where
    /// `:authority` is not surfaced as a `Host` header — the URI's authority,
    /// or finally the server's `server_name`.
    pub fn host(&self) -> String {
        if let Some(v) = self.headers.get("host").and_then(|v| v.to_str().ok()) {
            return v.to_string();
        }
        if let Some(authority) = self.uri.authority() {
            return authority.as_str().to_string();
        }
        self.server_name.unwrap_or("").to_string()
    }

    /// `$request_uri`: path plus query, exactly as received.
    pub fn request_uri(&self) -> &str {
        self.uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or_else(|| self.uri.path())
    }

    /// Case-insensitive header lookup as a `&str`. Returns `None` if the
    /// header is missing or contains non-ASCII bytes.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Look up a single query argument by name. Values are percent-decoded
    /// and `+` is treated as space.
    pub fn query_arg(&self, name: &str) -> Option<String> {
        let q = self.uri.query()?;
        for pair in q.split('&') {
            let (k, v) = match pair.split_once('=') {
                Some(kv) => kv,
                None => (pair, ""),
            };
            if k == name {
                return Some(percent_decode(v));
            }
        }
        None
    }

    /// Look up a single cookie value by name from the `Cookie` request header.
    pub fn cookie(&self, name: &str) -> Option<String> {
        let raw = self.header("cookie")?;
        for kv in raw.split(';') {
            let kv = kv.trim();
            if let Some((k, v)) = kv.split_once('=') {
                if k == name {
                    return Some(v.to_string());
                }
            }
        }
        None
    }
}

/// Minimal `application/x-www-form-urlencoded` decoder used for query args.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
