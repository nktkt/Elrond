//! TLS setup: load PEM-encoded certificate chains and private keys, then
//! build a `rustls::ServerConfig` ready to wrap accepted TCP streams.

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

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

/// Load a PEM-encoded private key (PKCS#8, RSA, or SEC1).
pub fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let f = std::fs::File::open(path)
        .map_err(|e| format!("cannot open key '{}': {e}", path.display()))?;
    let mut br = std::io::BufReader::new(f);
    rustls_pemfile::private_key(&mut br)
        .map_err(|e| format!("parsing key '{}': {e}", path.display()))?
        .ok_or_else(|| format!("no private key in '{}'", path.display()))
}

/// Build a `rustls::ServerConfig` for one server block, with ALPN advertising
/// only `http/1.1` (HTTP/2 over TLS is a later phase).
pub fn server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("rustls server config: {e}"))?;
    // Offer HTTP/2 first, then fall back to HTTP/1.1. Clients that don't
    // know h2 (e.g. older curl) negotiate http/1.1.
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// Install the default crypto provider exactly once at process start.
/// rustls 0.23 requires an explicit provider; we use the `ring` backend.
pub fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Ignore the result: a duplicate install error is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
