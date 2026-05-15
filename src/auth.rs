//! HTTP Basic authentication backed by an htpasswd-style file.
//!
//! Only **bcrypt** hashes (`$2a$`, `$2b$`, `$2y$`) are accepted. Plain-text
//! passwords, Apache's MD5-variant APR1 (`$apr1$…`), and the SHA-1 variant
//! `{SHA}…` are rejected at config-load time. Refusing weak crypto is the
//! safer default than silently accepting it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::Response;

use crate::body::{full, ElrondBody};

#[derive(Debug)]
pub struct AuthBasic {
    pub realm: String,
    pub users: HashMap<String, String>,
}

impl AuthBasic {
    /// Load and validate an htpasswd-style file. Comments (`#…`) and blank
    /// lines are ignored. Any non-bcrypt entry is reported with its line
    /// number so the operator knows which line to fix.
    pub fn load(path: &Path, realm: String) -> Result<Arc<Self>, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!("cannot read auth_basic_user_file '{}': {e}", path.display())
        })?;
        let users = parse_htpasswd(&text, path)?;
        if users.is_empty() {
            return Err(format!(
                "auth_basic_user_file '{}' is empty after stripping comments",
                path.display()
            ));
        }
        Ok(Arc::new(AuthBasic { realm, users }))
    }
}

fn parse_htpasswd(
    text: &str,
    path: &Path,
) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (user, hash) = line.split_once(':').ok_or_else(|| {
            format!(
                "{}:{line_no}: expected 'user:hash', got '{raw}'",
                path.display()
            )
        })?;
        if !is_bcrypt(hash) {
            return Err(format!(
                "{}:{line_no}: user '{user}' uses a non-bcrypt hash. Elrond \
                 only accepts bcrypt (`$2y$…`, `$2a$…`, `$2b$…`). Re-create \
                 with `htpasswd -nbB user password`.",
                path.display()
            ));
        }
        out.insert(user.to_string(), hash.to_string());
    }
    Ok(out)
}

fn is_bcrypt(hash: &str) -> bool {
    hash.starts_with("$2y$") || hash.starts_with("$2a$") || hash.starts_with("$2b$")
}

/// Decide whether the request carries valid Basic credentials. Returns:
/// - `Ok(())` to let the request through.
/// - `Err(Response)` containing a `401 Unauthorized` response with the
///   correct `WWW-Authenticate` challenge.
pub fn check(
    auth: &AuthBasic,
    headers: &HeaderMap,
) -> Result<(), Response<ElrondBody>> {
    if let Some(creds) = parse_authorization(headers) {
        if let Some((user, password)) = creds.split_once(':') {
            if let Some(hash) = auth.users.get(user) {
                if bcrypt::verify(password, hash).unwrap_or(false) {
                    return Ok(());
                }
            }
        }
    }
    Err(challenge_response(&auth.realm))
}

fn parse_authorization(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("authorization").and_then(|v| v.to_str().ok())?;
    let mut parts = value.splitn(2, ' ');
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let b64 = parts.next()?.trim();
    let bytes = BASE64_STD.decode(b64).ok()?;
    String::from_utf8(bytes).ok()
}

fn challenge_response(realm: &str) -> Response<ElrondBody> {
    let www_authenticate = format!("Basic realm=\"{}\"", escape_quoted(realm));
    let mut b = Response::builder()
        .status(401)
        .header("content-type", "text/plain; charset=utf-8");
    if let Ok(v) = HeaderValue::from_str(&www_authenticate) {
        b = b.header("www-authenticate", v);
    }
    b.body(full("401 Unauthorized\n"))
        .expect("401 challenge is well-formed")
}

fn escape_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_htpasswd(name: &str, contents: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    #[test]
    fn rejects_plaintext_entries() {
        let p = write_htpasswd(
            "elrond-auth-plain.htpasswd",
            "alice:swordfish\n",
        );
        let err = AuthBasic::load(&p, "r".into()).unwrap_err();
        assert!(err.contains("non-bcrypt"), "got: {err}");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn rejects_apr1_entries() {
        let p = write_htpasswd(
            "elrond-auth-apr1.htpasswd",
            "bob:$apr1$saltsalt$abcdefghijklmnopqrstuv\n",
        );
        let err = AuthBasic::load(&p, "r".into()).unwrap_err();
        assert!(err.contains("non-bcrypt"), "got: {err}");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn loads_bcrypt_and_verifies() {
        // Pre-computed bcrypt hash for the password "swordfish" with cost 4.
        let hash = bcrypt::hash("swordfish", 4).unwrap();
        let p = write_htpasswd(
            "elrond-auth-bcrypt.htpasswd",
            &format!("alice:{hash}\n# a comment\n\n"),
        );
        let auth = AuthBasic::load(&p, "secret".into()).unwrap();
        assert_eq!(auth.realm, "secret");
        assert_eq!(auth.users.len(), 1);

        // Build a fake Authorization header and verify.
        let mut h = HeaderMap::new();
        let creds = BASE64_STD.encode(b"alice:swordfish");
        h.insert(
            "authorization",
            format!("Basic {creds}").parse().unwrap(),
        );
        assert!(check(&auth, &h).is_ok());

        // Wrong password fails.
        let mut h2 = HeaderMap::new();
        let bad = BASE64_STD.encode(b"alice:wrong");
        h2.insert("authorization", format!("Basic {bad}").parse().unwrap());
        let resp = check(&auth, &h2).unwrap_err();
        assert_eq!(resp.status().as_u16(), 401);
        assert!(
            resp.headers()
                .get("www-authenticate")
                .map(|v| v.to_str().unwrap().contains("Basic"))
                .unwrap_or(false)
        );

        // Missing Authorization header → 401.
        assert!(check(&auth, &HeaderMap::new()).is_err());

        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn rejects_empty_file() {
        let p = write_htpasswd("elrond-auth-empty.htpasswd", "# only comments\n");
        let err = AuthBasic::load(&p, "r".into()).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
        let _ = std::fs::remove_file(p);
    }
}
