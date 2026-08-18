//! What each `sslmode` means on the wire (spec §6.4).
//!
//! libpq spells two independent decisions in one word, and the Postgres driver
//! keeps them apart. The driver's own mode ([`Fallback`] here) decides what
//! happens when the server declines TLS — carry on in plaintext, or fail. The
//! TLS stack decides how much of the certificate is believed once TLS is up.
//! Every mode therefore sets both dials here, and `prefer` is why they cannot
//! be collapsed into one: it is the only mode that both encrypts when it can
//! and continues when it cannot.
//!
//! The driver models three of the five modes and rejects `verify-ca` and
//! `verify-full` outright, which is why pgpushy interprets the mode itself
//! (spec §6.4). The two verifying modes reach the driver as `require`, and the
//! verification they asked for is carried by the connector instead.
//!
//! With rustls the strictest mode is the one that needs no code: `verify-full`
//! is rustls' own default, and the weaker modes are that verifier with a check
//! taken out. Each removal goes through rustls' `dangerous` API, so this module
//! is the whole of pgpushy's certificate handling — there is nowhere else to
//! look.

use crate::conn::SslMode;
use anyhow::{Context, Result, anyhow};
use postgres::config::SslMode as Fallback;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};
use std::sync::Arc;
use tokio_postgres_rustls::MakeRustlsConnect;

/// The two things a mode decides, kept apart because the driver keeps them
/// apart: it is handed the fallback, and the connector carries the checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Security {
    /// What the driver does when the server will not speak TLS.
    pub fallback: Fallback,
    /// How much of the certificate is checked, or `None` for no TLS at all.
    pub verification: Option<Verification>,
}

/// How much of the server's certificate pgpushy believes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Nothing. The traffic is encrypted and the peer is whoever answered,
    /// which is all libpq's `require` and `prefer` promise.
    Nothing,
    /// The chain, but not the hostname: libpq's `verify-ca`.
    Chain,
    /// The chain and the hostname: libpq's `verify-full`, and rustls' default.
    ChainAndHostname,
}

impl Security {
    /// The spec §6.4 table, in code.
    pub fn for_mode(mode: SslMode) -> Self {
        match mode {
            SslMode::Disable => Self {
                fallback: Fallback::Disable,
                verification: None,
            },
            SslMode::Prefer => Self {
                fallback: Fallback::Prefer,
                verification: Some(Verification::Nothing),
            },
            SslMode::Require => Self {
                fallback: Fallback::Require,
                verification: Some(Verification::Nothing),
            },
            // The driver has no mode for these two, so they arrive as
            // `require` — encryption is mandatory — and the connector supplies
            // the verification the operator actually asked for.
            SslMode::VerifyCa => Self {
                fallback: Fallback::Require,
                verification: Some(Verification::Chain),
            },
            SslMode::VerifyFull => Self {
                fallback: Fallback::Require,
                verification: Some(Verification::ChainAndHostname),
            },
        }
    }
}

/// The certificate verifier a level of checking calls for.
///
/// Separated from [`connector`] so that the choice can be asserted directly.
/// It is the one line in pgpushy where a mistake is silent: a verifier that
/// checks too little still connects, to anything at all.
fn verifier(
    verification: Verification,
    provider: &Arc<CryptoProvider>,
    mode: SslMode,
) -> Result<Arc<dyn ServerCertVerifier>> {
    Ok(match verification {
        Verification::ChainAndHostname => verifying(provider, mode)?,
        Verification::Chain => Arc::new(HostnameUnchecked(verifying(provider, mode)?)),
        Verification::Nothing => Arc::new(Unverified {
            algorithms: provider.signature_verification_algorithms,
        }),
    })
}

/// The TLS connector a mode calls for, or `None` when it wants no TLS.
///
/// `None` is not "TLS if the server offers it": paired with
/// [`Security::fallback`] of `Disable` it is `sslmode=disable`, and the driver
/// never sends the TLS request at all.
pub fn connector(mode: SslMode) -> Result<Option<MakeRustlsConnect>> {
    let Some(verification) = Security::for_mode(mode).verification else {
        return Ok(None);
    };

    // Passed explicitly rather than installed process-wide. `ureq` builds its
    // own provider for the managed backend's downloads, and rustls panics
    // rather than erring when it needs a process default that nobody
    // installed — a panic that would land on the first connection, at the
    // worst moment to discover it.
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let verifier = verifier(verification, &provider, mode)?;

    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("building the TLS configuration for the target connection")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    Ok(Some(MakeRustlsConnect::new(config)))
}

/// rustls' default verifier: chain and hostname, over the platform trust store.
///
/// The two weaker modes are built from this one rather than beside it, so a
/// mistake in either is a check visibly removed rather than a check
/// accidentally never written.
fn verifying(provider: &Arc<CryptoProvider>, mode: SslMode) -> Result<Arc<WebPkiServerVerifier>> {
    WebPkiServerVerifier::builder_with_provider(Arc::new(trust_store(mode)?), provider.clone())
        .build()
        .with_context(|| {
            format!(
                "building the certificate verifier for sslmode={}",
                mode.as_str()
            )
        })
}

/// The trust anchors the verifying modes check against: the platform's own.
///
/// Not the bundled Mozilla root list that `ureq` uses for its downloads. That
/// bundle is the public web PKI, and a Postgres target is usually private
/// infrastructure behind a private or corporate CA that no public bundle
/// contains — so `verify-full` against one would be impossible rather than
/// merely inconvenient, since pgpushy takes no root-certificate setting.
///
/// The platform store is also where pgschema's Go TLS stack looks, and both
/// honor `SSL_CERT_FILE` and `SSL_CERT_DIR`. pgschema runs in a working
/// directory pgpushy chooses, so a relative one would name a different file
/// for each; `conn.rs` resolves both before the child sees them, which is what
/// makes the two connections spec §6.3 pairs up accept the same certificates.
fn trust_store(mode: SslMode) -> Result<RootCertStore> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _unusable) = roots.add_parsable_certificates(found.certs);

    if added == 0 {
        return Err(no_trust_anchors(mode, &found.errors));
    }

    Ok(roots)
}

/// Why a verifying mode cannot proceed, and what to do about it.
///
/// Every reason the loader reported is listed: on a machine where the store is
/// in an unexpected place there is usually more than one path it looked at.
fn no_trust_anchors(mode: SslMode, errors: &[rustls_native_certs::Error]) -> anyhow::Error {
    let reasons = errors
        .iter()
        .map(|error| format!("\n  {error}"))
        .collect::<String>();
    anyhow!(
        "sslmode={} needs a certificate authority to check the target against, \
         and the system trust store holds none{reasons}\n\
         \n\
         pgpushy verifies against the platform trust store, which is where pgschema \
         looks too. Install the CA that signed the target's certificate there — or \
         point SSL_CERT_FILE at it, which both honor. If this target does not need to \
         prove who it is, write sslmode = \"require\", which encrypts without checking \
         the certificate.",
        mode.as_str(),
    )
}

/// `verify-ca`: the chain is checked, the hostname is not.
///
/// rustls checks the chain first and the name last, so a name error is the one
/// thing that can be dropped here without dropping anything else with it.
#[derive(Debug)]
struct HostnameUnchecked(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for HostnameUnchecked {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(TlsError::InvalidCertificate(
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
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// `require` and `prefer`: encryption, and no claim about who is on the far end.
///
/// The certificate is accepted whatever it says — no chain, no name, no expiry.
/// The handshake signature is still checked, because that is what ties the
/// session to the key in the certificate the server presented; without it the
/// server would not even have to hold the key it offered, and the channel
/// binding pgpushy's driver derives from that certificate would mean nothing.
#[derive(Debug)]
struct Unverified {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec §6.4 table, asserted a row at a time. Both dials matter: a mode
    /// with the right verification and the wrong fallback still connects in
    /// plaintext against a server that declines TLS.
    #[test]
    fn every_mode_sets_both_dials() {
        let rows = [
            (SslMode::Disable, Fallback::Disable, None),
            (
                SslMode::Prefer,
                Fallback::Prefer,
                Some(Verification::Nothing),
            ),
            (
                SslMode::Require,
                Fallback::Require,
                Some(Verification::Nothing),
            ),
            (
                SslMode::VerifyCa,
                Fallback::Require,
                Some(Verification::Chain),
            ),
            (
                SslMode::VerifyFull,
                Fallback::Require,
                Some(Verification::ChainAndHostname),
            ),
        ];

        for (mode, fallback, verification) in rows {
            let security = Security::for_mode(mode);
            assert_eq!(
                security,
                Security {
                    fallback,
                    verification
                },
                "sslmode={}",
                mode.as_str(),
            );
        }
    }

    /// `prefer` encrypts when it can. Collapsing it into `require` would break
    /// every server that does not offer TLS; collapsing it into `disable`
    /// would send the password in the clear to one that does.
    #[test]
    fn prefer_is_neither_disable_nor_require() {
        let prefer = Security::for_mode(SslMode::Prefer);
        assert_eq!(prefer.fallback, Fallback::Prefer);
        assert_eq!(prefer.verification, Some(Verification::Nothing));
    }

    #[test]
    fn disable_gets_no_connector_at_all() {
        assert!(connector(SslMode::Disable).unwrap().is_none());
    }

    /// The unverifying modes need no trust anchors, so they build anywhere —
    /// including on a machine with no certificate store at all.
    #[test]
    fn the_unverifying_modes_build_a_connector() {
        assert!(connector(SslMode::Prefer).unwrap().is_some());
        assert!(connector(SslMode::Require).unwrap().is_some());
    }

    /// Skipped rather than failed where the platform has no trust store: the
    /// claim is about what pgpushy builds from one, not about the machine.
    #[test]
    fn the_verifying_modes_build_a_connector_from_the_platform_store() {
        if trust_store(SslMode::VerifyFull).is_err() {
            return;
        }
        assert!(connector(SslMode::VerifyCa).unwrap().is_some());
        assert!(connector(SslMode::VerifyFull).unwrap().is_some());
    }

    /// A store with no anchors cannot verify anything, so the mode is refused
    /// rather than quietly downgraded to one that trusts every certificate.
    #[test]
    fn no_trust_anchors_names_the_mode_and_the_way_out() {
        let refusal = no_trust_anchors(SslMode::VerifyCa, &[]).to_string();
        assert!(refusal.contains("sslmode=verify-ca"), "{refusal}");
        assert!(refusal.contains("SSL_CERT_FILE"), "{refusal}");
        assert!(refusal.contains("require"), "{refusal}");
    }
}

#[cfg(test)]
mod verifier_tests {
    use super::*;

    /// Which verifier a mode gets, asserted on behaviour rather than on type.
    ///
    /// The discriminator is a certificate that is not a certificate: anything
    /// that checks a chain must reject 32 zero bytes, and anything that checks
    /// nothing must accept them. That separates the verifying modes from the
    /// permissive ones without a fixture, a trust store, or a server — so the
    /// wiring is pinned everywhere CI runs, not only where TLS can be spoken.
    ///
    /// Without this, `verify-ca` could be reduced to trusting anything and
    /// every other test would still pass.
    fn accepts_nonsense(mode: SslMode) -> bool {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verification = Security::for_mode(mode)
            .verification
            .expect("mode wants TLS");
        let Ok(verifier) = verifier(verification, &provider, mode) else {
            // Building a verifying verifier needs a trust store, which a
            // machine may not have. It refused, which is not acceptance.
            return false;
        };
        verifier
            .verify_server_cert(
                &CertificateDer::from(vec![0u8; 32]),
                &[],
                &ServerName::try_from("db.example").expect("a valid name"),
                &[],
                UnixTime::now(),
            )
            .is_ok()
    }

    #[test]
    fn the_permissive_modes_accept_anything_at_all() {
        assert!(accepts_nonsense(SslMode::Prefer));
        assert!(accepts_nonsense(SslMode::Require));
    }

    #[test]
    fn the_verifying_modes_reject_a_certificate_that_is_not_one() {
        assert!(
            !accepts_nonsense(SslMode::VerifyCa),
            "verify-ca relaxes the hostname check and nothing else"
        );
        assert!(!accepts_nonsense(SslMode::VerifyFull));
    }
}
