//! TLS setup: load PEM-encoded certificate chains and private keys, build a
//! `rustls::ServerConfig` that can host multiple certificates and resolve
//! them by SNI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

/// One entry in the resolver: a parsed certificate / key pair, optionally
/// scoped to a specific SNI server name.
pub struct CertEntry {
    pub server_name: Option<String>,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Load a PEM-encoded certificate chain.
pub fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let f = std::fs::File::open(path)
        .map_err(|e| format!("cannot open certificate '{}': {e}", path.display()))?;
    let mut br = std::io::BufReader::new(f);
    let mut out = Vec::new();
    for r in rustls_pemfile::certs(&mut br) {
        let der = r.map_err(|e| {
            format!("parsing certificate '{}': {e}", path.display())
        })?;
        out.push(der);
    }
    if out.is_empty() {
        return Err(format!(
            "certificate file '{}' contains no certificates",
            path.display()
        ));
    }
    Ok(out)
}

/// Load a PEM-encoded private key (PKCS#8, PKCS#1 RSA, or SEC1).
pub fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let f = std::fs::File::open(path)
        .map_err(|e| format!("cannot open key '{}': {e}", path.display()))?;
    let mut br = std::io::BufReader::new(f);
    rustls_pemfile::private_key(&mut br)
        .map_err(|e| format!("parsing key '{}': {e}", path.display()))?
        .ok_or_else(|| format!("no private key in '{}'", path.display()))
}

/// Compile a single `CertEntry` into a `CertifiedKey`.
fn build_certified(entry: &CertEntry) -> Result<Arc<CertifiedKey>, String> {
    let chain = load_certs(&entry.cert_path)?;
    let key_der = load_key(&entry.key_path)?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key_der)
        .map_err(|e| format!("invalid key '{}': {e}", entry.key_path.display()))?;
    Ok(Arc::new(CertifiedKey::new(chain, signing)))
}

/// Build a `rustls::ServerConfig` for a listener that may host several
/// certificates keyed by SNI. The **first** entry serves as the default
/// when the client offers no SNI or an unknown name.
///
/// `protocols` selects the TLS protocol versions to allow. An empty slice
/// keeps rustls's default (TLS 1.2 + TLS 1.3).
pub fn build_server_config(
    entries: &[CertEntry],
    protocols: &[crate::config::TlsVersion],
) -> Result<Arc<rustls::ServerConfig>, String> {
    if entries.is_empty() {
        return Err("no TLS certificates configured for this listener".into());
    }
    let mut by_name: HashMap<String, Arc<CertifiedKey>> = HashMap::new();
    let mut default: Option<Arc<CertifiedKey>> = None;
    for e in entries {
        let ck = build_certified(e)?;
        if default.is_none() {
            default = Some(ck.clone());
        }
        if let Some(name) = &e.server_name {
            by_name.insert(name.to_ascii_lowercase(), ck);
        }
    }
    let default = default.expect("non-empty entries always set default");
    let resolver = Arc::new(SniResolver { by_name, default });

    let mut cfg = if protocols.is_empty() {
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver)
    } else {
        let mut versions: Vec<&'static rustls::SupportedProtocolVersion> = Vec::new();
        for p in protocols {
            match p {
                crate::config::TlsVersion::Tls12 => versions.push(&rustls::version::TLS12),
                crate::config::TlsVersion::Tls13 => versions.push(&rustls::version::TLS13),
            }
        }
        rustls::ServerConfig::builder_with_protocol_versions(&versions)
            .with_no_client_auth()
            .with_cert_resolver(resolver)
    };
    // Offer HTTP/2 first, fall back to HTTP/1.1.
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// Build a `rustls::ServerConfig` for HTTP/3 / QUIC: TLS 1.3 only,
/// ALPN = `h3`. Shares the same multi-cert SNI resolver as
/// [`build_server_config`].
pub fn build_h3_server_config(
    entries: &[CertEntry],
) -> Result<Arc<rustls::ServerConfig>, String> {
    if entries.is_empty() {
        return Err("no TLS certificates configured for this HTTP/3 listener".into());
    }
    let mut by_name: HashMap<String, Arc<CertifiedKey>> = HashMap::new();
    let mut default: Option<Arc<CertifiedKey>> = None;
    for e in entries {
        let ck = build_certified(e)?;
        if default.is_none() {
            default = Some(ck.clone());
        }
        if let Some(name) = &e.server_name {
            by_name.insert(name.to_ascii_lowercase(), ck);
        }
    }
    let default = default.expect("non-empty entries always set default");
    let resolver = Arc::new(SniResolver { by_name, default });

    // QUIC mandates TLS 1.3.
    let mut cfg = rustls::ServerConfig::builder_with_protocol_versions(&[
        &rustls::version::TLS13,
    ])
    .with_no_client_auth()
    .with_cert_resolver(resolver);
    cfg.alpn_protocols = vec![b"h3".to_vec()];
    Ok(Arc::new(cfg))
}

/// Backwards-compatible single-cert builder: equivalent to one `CertEntry`
/// with no `server_name` (acts as the default).
#[allow(dead_code)]
pub fn server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<rustls::ServerConfig>, String> {
    build_server_config(
        &[CertEntry {
            server_name: None,
            cert_path: cert_path.to_path_buf(),
            key_path: key_path.to_path_buf(),
        }],
        &[],
    )
}

/// rustls cert resolver: pick by SNI server name, fall back to default.
struct SniResolver {
    by_name: HashMap<String, Arc<CertifiedKey>>,
    default: Arc<CertifiedKey>,
}

impl std::fmt::Debug for SniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniResolver")
            .field("names", &self.by_name.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(name) = hello.server_name() {
            let key = name.to_ascii_lowercase();
            if let Some(ck) = self.by_name.get(&key) {
                return Some(ck.clone());
            }
        }
        Some(self.default.clone())
    }
}

/// Install the default crypto provider exactly once at process start.
pub fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
