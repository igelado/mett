//! TLS for the connection: the system's trust, plus what the settings add.

use std::sync::Arc;

use mett_core::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_platform_verifier::Verifier;

/// A client configuration that trusts the system's certificate
/// authorities and `extra` (PEM). Without `verify_host`, a certificate that
/// is good but for another name is taken too (mutt's `ssl_verify_host`).
pub(crate) fn client_config(verify_host: bool, extra: &[Vec<u8>]) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Backend(format!("TLS: {e}")))?;
    let mut roots = Vec::new();
    for pem in extra {
        for cert in CertificateDer::pem_slice_iter(pem) {
            roots.push(cert.map_err(|e| Error::Backend(format!("CA certificate: {e}")))?);
        }
    }
    let inner = Verifier::new_with_extra_roots(roots, provider)
        .map_err(|e| Error::Backend(format!("TLS: {e}")))?;
    let verifier: Arc<dyn ServerCertVerifier> = if verify_host {
        Arc::new(inner)
    } else {
        Arc::new(AnyName(inner))
    };
    Ok(Arc::new(
        builder
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    ))
}

/// A verifier that checks the certificate as another does, but not its
/// name (`ssl_verify_host` off).
#[derive(Debug)]
struct AnyName(Verifier);

impl ServerCertVerifier for AnyName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
