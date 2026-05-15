//! Variable interpolation for configuration values.
//!
//! A `Template` is a sequence of literal text and variable references parsed
//! once at config-load time, then rendered against a [`RequestCtx`] at
//! request time. Supported syntax mirrors Nginx:
//!
//! ```text
//! "$host"          // bare variable name
//! "${request_uri}" // braced variable name
//! "$arg_id"        // query argument
//! "$http_user_agent" // request header (underscores → hyphens)
//! "$cookie_session"  // cookie value
//! ```
//!
//! Unknown variable names render as the empty string. A literal `$` followed
//! by a non-identifier character is preserved as-is.

use crate::request_ctx::RequestCtx;

#[derive(Debug, Clone)]
pub struct Template {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(String),
    Var(VarRef),
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Unknown(name) keeps the name for future diagnostics
enum VarRef {
    Host,
    RemoteAddr,
    RequestUri,
    Uri,
    RequestMethod,
    Args,
    Scheme,
    ServerName,
    Arg(String),
    Http(String),
    Cookie(String),
    Unknown(String),
}

impl Template {
    pub fn parse(input: &str) -> Template {
        let mut segments: Vec<Segment> = Vec::new();
        let mut literal = String::new();
        let mut chars = input.chars().peekable();

        while let Some(c) = chars.next() {
            if c != '$' {
                literal.push(c);
                continue;
            }

            let (name, recognized) = if matches!(chars.peek(), Some('{')) {
                chars.next();
                let mut n = String::new();
                let mut closed = false;
                while let Some(&c) = chars.peek() {
                    if c == '}' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    n.push(c);
                    chars.next();
                }
                (n, closed)
            } else {
                let mut n = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        n.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let recognized = !n.is_empty();
                (n, recognized)
            };

            if !recognized {
                literal.push('$');
                if !name.is_empty() {
                    literal.push_str(&name);
                }
                continue;
            }

            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(Segment::Var(classify(&name)));
        }

        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        Template { segments }
    }

    pub fn render(&self, ctx: &RequestCtx<'_>) -> String {
        let mut out = String::new();
        for s in &self.segments {
            match s {
                Segment::Literal(s) => out.push_str(s),
                Segment::Var(v) => v.render_into(ctx, &mut out),
            }
        }
        out
    }

    /// True if the template contains no variable references.
    #[allow(dead_code)] // public API; not all callers use it yet
    pub fn is_literal(&self) -> bool {
        self.segments.iter().all(|s| matches!(s, Segment::Literal(_)))
    }
}

fn classify(name: &str) -> VarRef {
    if let Some(rest) = name.strip_prefix("arg_") {
        return VarRef::Arg(rest.to_string());
    }
    if let Some(rest) = name.strip_prefix("http_") {
        return VarRef::Http(rest.to_string());
    }
    if let Some(rest) = name.strip_prefix("cookie_") {
        return VarRef::Cookie(rest.to_string());
    }
    match name {
        "host" => VarRef::Host,
        "remote_addr" => VarRef::RemoteAddr,
        "request_uri" => VarRef::RequestUri,
        "uri" | "document_uri" => VarRef::Uri,
        "request_method" => VarRef::RequestMethod,
        "args" | "query_string" => VarRef::Args,
        "scheme" => VarRef::Scheme,
        "server_name" => VarRef::ServerName,
        other => VarRef::Unknown(other.to_string()),
    }
}

impl VarRef {
    fn render_into(&self, ctx: &RequestCtx<'_>, out: &mut String) {
        match self {
            VarRef::Host => out.push_str(&ctx.host()),
            VarRef::RemoteAddr => out.push_str(&ctx.peer.ip().to_string()),
            VarRef::RequestUri => out.push_str(ctx.request_uri()),
            VarRef::Uri => out.push_str(ctx.uri.path()),
            VarRef::RequestMethod => out.push_str(ctx.method.as_str()),
            VarRef::Args => out.push_str(ctx.uri.query().unwrap_or("")),
            VarRef::Scheme => out.push_str(ctx.scheme),
            VarRef::ServerName => out.push_str(ctx.server_name.unwrap_or("")),
            VarRef::Arg(name) => {
                if let Some(v) = ctx.query_arg(name) {
                    out.push_str(&v);
                }
            }
            VarRef::Http(name) => {
                if let Some(v) = ctx.header(&name.replace('_', "-")) {
                    out.push_str(v);
                }
            }
            VarRef::Cookie(name) => {
                if let Some(v) = ctx.cookie(name) {
                    out.push_str(&v);
                }
            }
            VarRef::Unknown(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{HeaderMap, Method, Uri};
    use std::net::SocketAddr;

    fn ctx<'a>(
        method: &'a Method,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        peer: SocketAddr,
        server_name: Option<&'a str>,
    ) -> RequestCtx<'a> {
        RequestCtx {
            peer,
            server_name,
            method,
            uri,
            headers,
            scheme: "http",
        }
    }

    #[test]
    fn pure_literal() {
        let t = Template::parse("hello world");
        assert!(t.is_literal());
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let c = ctx(&m, &u, &h, "127.0.0.1:1".parse().unwrap(), None);
        assert_eq!(t.render(&c), "hello world");
    }

    #[test]
    fn standard_variables() {
        let t = Template::parse("$scheme://$host$request_uri");
        let m = Method::GET;
        let u: Uri = "/path?a=1".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("host", "example.test".parse().unwrap());
        let c = ctx(&m, &u, &h, "10.0.0.1:443".parse().unwrap(), Some("srv"));
        assert_eq!(t.render(&c), "http://example.test/path?a=1");
    }

    #[test]
    fn host_falls_back_to_server_name() {
        let t = Template::parse("$host");
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let c = ctx(&m, &u, &h, "127.0.0.1:1".parse().unwrap(), Some("fallback"));
        assert_eq!(t.render(&c), "fallback");
    }

    #[test]
    fn query_args() {
        let t = Template::parse("hello $arg_name!");
        let m = Method::GET;
        let u: Uri = "/?name=alice&id=2".parse().unwrap();
        let h = HeaderMap::new();
        let c = ctx(&m, &u, &h, "127.0.0.1:1".parse().unwrap(), None);
        assert_eq!(t.render(&c), "hello alice!");
    }

    #[test]
    fn http_header_underscore_mapping() {
        let t = Template::parse("$http_user_agent");
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("user-agent", "elrond-test/1".parse().unwrap());
        let c = ctx(&m, &u, &h, "127.0.0.1:1".parse().unwrap(), None);
        assert_eq!(t.render(&c), "elrond-test/1");
    }

    #[test]
    fn braced_and_unknown_var() {
        let t = Template::parse("a=${request_method} b=$nope c=$$");
        let m = Method::POST;
        let u: Uri = "/".parse().unwrap();
        let h = HeaderMap::new();
        let c = ctx(&m, &u, &h, "127.0.0.1:1".parse().unwrap(), None);
        assert_eq!(t.render(&c), "a=POST b= c=$$");
    }

    #[test]
    fn cookie_lookup() {
        let t = Template::parse("$cookie_session");
        let m = Method::GET;
        let u: Uri = "/".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("cookie", "a=1; session=abc; b=2".parse().unwrap());
        let c = ctx(&m, &u, &h, "127.0.0.1:1".parse().unwrap(), None);
        assert_eq!(t.render(&c), "abc");
    }
}
