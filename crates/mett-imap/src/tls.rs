//! TLS for the connection: the system's trust, plus what the settings add.

use std::sync::Arc;

use mett_core::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_platform_verifier::Verifier;

/// A client configuration that trusts the system's certificate
/// authorities and `extra` (PEM), or, without `verify`, anything.
pub(crate) fn client_config(verify: bool, extra: &[Vec<u8>]) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Backend(format!("TLS: {e}")))?;
    let config = if verify {
        let mut roots = Vec::new();
        for pem in extra {
            for cert in CertificateDer::pem_slice_iter(pem) {
                roots.push(cert.map_err(|e| Error::Backend(format!("CA certificate: {e}")))?);
            }
        }
        let verifier = Verifier::new_with_extra_roots(roots, provider)
            .map_err(|e| Error::Backend(format!("TLS: {e}")))?;
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Anything(provider)))
    };
    Ok(Arc::new(config.with_no_client_auth()))
}

/// A verifier that takes any certificate (`ssl_verify_host` off).
#[derive(Debug)]
struct Anything(Arc<CryptoProvider>);

impl ServerCertVerifier for Anything {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
