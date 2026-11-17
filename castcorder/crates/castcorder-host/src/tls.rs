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

/// A TLS client to a host's LAN listener that accepts any certificate. Only
/// for exchanges sealed end to end to a known host key (linking a device,
/// castcorder-host `person`): the seal authenticates the host, and the LAN
/// certificate is self-signed anyway.
pub fn unverified_client(
    host: &str,
    tcp: std::net::TcpStream,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    let name =
        rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(Arc::new(cfg), name).map_err(|e| e.to_string())?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

/// Accepts any server certificate, but still checks the handshake
/// signatures (so the TLS session itself is sound).
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
