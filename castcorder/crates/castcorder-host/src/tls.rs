//! The self-signed certificate for the LAN page. Browsers grant the microphone
//! only in a secure context, and `http://<lan-ip>` is not one, so guests load
//! the page over HTTPS and click through the certificate warning once. The
//! certificate is made on first run and kept in the library root
//! (`host-cert.pem`, `host-key.pem`) so that click-through stays valid across
//! runs.

use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tracing::info;

/// Load the certificate from `dir`, making one first if there is none.
pub fn server_config(dir: &Path, ips: &[IpAddr]) -> io::Result<Arc<rustls::ServerConfig>> {
    let cert_path = dir.join("host-cert.pem");
    let key_path = dir.join("host-key.pem");
    if !cert_path.exists() || !key_path.exists() {
        let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        names.extend(ips.iter().map(|ip| ip.to_string()));
        let ck = rcgen::generate_simple_self_signed(names).map_err(io::Error::other)?;
        std::fs::write(&key_path, ck.signing_key.serialize_pem())?;
        std::fs::write(&cert_path, ck.cert.pem())?;
        info!(cert = %cert_path.display(), "made a self-signed certificate");
    }
    let certs = CertificateDer::pem_file_iter(&cert_path)
        .and_then(|i| i.collect::<Result<Vec<_>, _>>())
        .map_err(io::Error::other)?;
    let key = PrivateKeyDer::from_pem_file(&key_path).map_err(io::Error::other)?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;
    Ok(Arc::new(cfg))
}
