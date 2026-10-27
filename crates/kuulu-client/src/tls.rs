//! TLS for self-signed servers: trust exactly the certificate whose SHA-256
//! fingerprint the invite carries, instead of a CA chain.

use quinn::rustls::{
    self, CertificateError, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::sync::Arc;

/// Name sent in SNI. Pinning makes the certificate's names irrelevant.
pub const SERVER_NAME: &str = "kuulu";

/// The alert a pin mismatch produces, so callers can tell it from other failures.
pub fn is_pin_mismatch(e: &quinn::ConnectionError) -> bool {
    let alert = rustls::AlertDescription::from(CertificateError::ApplicationVerificationFailure);
    matches!(e, quinn::ConnectionError::TransportError(t)
        if t.code == quinn::TransportErrorCode::crypto(alert.into()))
}

pub fn client_config(fingerprint: &str) -> quinn::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCert {
        fingerprint: fingerprint.to_ascii_lowercase(),
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = kuulu_proto::alpn_protocols();
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .expect("TLS 1.3 config with QUIC cipher suites");
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(kuulu_proto::client_transport_config()));
    config
}

#[derive(Debug)]
struct PinnedCert {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if kuulu_proto::cert_fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(CertificateError::ApplicationVerificationFailure.into())
        }
    }

    // The signatures still prove the server holds the pinned certificate's key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
