use std::{io, path::Path, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::digest;
use rustls::{ClientConfig, RootCertStore, ServerConfig, server::WebPkiClientVerifier};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use x509_parser::{
    extensions::{GeneralName, ParsedExtension},
    parse_x509_certificate,
};

use crate::protocol::Role;

#[derive(Clone, Debug)]
pub struct PeerIdentity {
    pub role: Role,
    pub id: String,
    pub spki_sha256: String,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

impl PeerIdentity {
    #[must_use]
    pub const fn active_at(&self, unix_secs: u64) -> bool {
        unix_secs >= self.not_before_unix_secs && unix_secs < self.not_after_unix_secs
    }
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("failed to open certificate {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse certificate {}", path.display()))?;
    if certs.is_empty() {
        bail!(
            "certificate file {} contains no certificates",
            path.display()
        );
    }
    Ok(certs)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path)
        .with_context(|| format!("failed to parse private key {}", path.display()))
}

fn load_roots(path: &Path) -> Result<RootCertStore> {
    let certs = load_certs(path)?;
    let mut roots = RootCertStore::empty();
    let (added, ignored) = roots.add_parsable_certificates(certs);
    if added == 0 || ignored != 0 {
        bail!(
            "CA file {} added {added} certificates and ignored {ignored}",
            path.display()
        );
    }
    Ok(roots)
}

/// Builds a mutual-TLS server acceptor backed by rustls and AWS-LC.
///
/// # Errors
///
/// Returns an error when certificate material is missing, malformed, or inconsistent.
pub fn server_acceptor(cert: &Path, key: &Path, client_ca: &Path) -> Result<TlsAcceptor> {
    let verifier = WebPkiClientVerifier::builder(Arc::new(load_roots(client_ca)?))
        .build()
        .context("failed to build client certificate verifier")?;
    let config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_client_cert_verifier(verifier)
        .with_single_cert(load_certs(cert)?, load_key(key)?)
        .context("failed to build TLS server config")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Builds a mutual-TLS client connector backed by rustls and AWS-LC.
///
/// # Errors
///
/// Returns an error when certificate material is missing, malformed, or inconsistent.
pub fn client_connector(cert: &Path, key: &Path, server_ca: &Path) -> Result<TlsConnector> {
    let config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(load_roots(server_ca)?)
        .with_client_auth_cert(load_certs(cert)?, load_key(key)?)
        .context("failed to build TLS client config")?;
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Converts a configured DNS name into rustls' owned server-name type.
///
/// # Errors
///
/// Returns an error when `name` is not a valid DNS name or IP address.
pub fn server_name(name: &str) -> Result<ServerName<'static>> {
    ServerName::try_from(name.to_owned())
        .map_err(|error| anyhow!("invalid TLS server name: {error}"))
}

/// Extracts the single `FlowSplice` URI identity and SPKI pin from a peer leaf certificate.
///
/// # Errors
///
/// Returns an error when the certificate is absent, malformed, or has an invalid identity URI.
pub fn peer_identity(certs: Option<&[CertificateDer<'_>]>) -> Result<PeerIdentity> {
    let leaf = certs
        .and_then(|items| items.first())
        .ok_or_else(|| anyhow!("peer presented no certificate"))?;
    let (_, cert) = parse_x509_certificate(leaf.as_ref())
        .map_err(|error| anyhow!("failed to parse peer certificate: {error}"))?;

    let mut identity = None;
    for extension in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = extension.parsed_extension() {
            for name in &san.general_names {
                if let GeneralName::URI(uri) = name
                    && let Some(rest) = uri.strip_prefix("flowsplice://identity/")
                {
                    let mut parts = rest.split('/');
                    let role = match parts.next() {
                        Some("server") => Role::Server,
                        Some("relay") => Role::Relay,
                        Some("home") => Role::Home,
                        Some("travel") => Role::Travel,
                        _ => bail!("peer identity URI has an invalid role"),
                    };
                    let id = parts
                        .next()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| anyhow!("peer identity URI has an empty id"))?;
                    if parts.next().is_some() || identity.is_some() {
                        bail!("peer certificate must contain exactly one identity URI");
                    }
                    identity = Some((role, id.to_owned()));
                }
            }
        }
    }
    let (role, id) =
        identity.ok_or_else(|| anyhow!("peer certificate has no FlowSplice URI SAN"))?;
    let spki = cert.public_key().raw;
    let hash = digest::digest(&digest::SHA256, spki);
    let not_before_unix_secs = u64::try_from(cert.validity().not_before.timestamp())
        .context("peer certificate not-before predates the Unix epoch")?;
    let not_after_unix_secs = u64::try_from(cert.validity().not_after.timestamp())
        .context("peer certificate not-after predates the Unix epoch")?;
    Ok(PeerIdentity {
        role,
        id,
        spki_sha256: hex::encode(hash.as_ref()),
        not_before_unix_secs,
        not_after_unix_secs,
    })
}

/// Applies the application identity, role, and optional SPKI allowlist checks.
///
/// # Errors
///
/// Returns an error when any configured identity constraint does not match.
pub fn require_peer(
    identity: &PeerIdentity,
    role: Role,
    expected_id: Option<&str>,
    allowed_spki: &[String],
) -> Result<()> {
    if identity.role != role {
        bail!(
            "peer role mismatch: expected {}, received {}",
            role.as_uri_part(),
            identity.role.as_uri_part()
        );
    }
    if expected_id.is_some_and(|expected| expected != identity.id) {
        bail!("peer id mismatch");
    }
    if !allowed_spki.is_empty()
        && !allowed_spki
            .iter()
            .any(|pin| pin.eq_ignore_ascii_case(&identity.spki_sha256))
    {
        bail!("peer SPKI is not allowlisted");
    }
    Ok(())
}

/// Validates a required SHA-256 SPKI allowlist at startup.
///
/// # Errors
///
/// Returns an error when the list is empty or a pin is not 32-byte hexadecimal.
pub fn validate_spki_pins(pins: &[String], label: &str) -> Result<()> {
    if pins.is_empty() {
        bail!("{label} SPKI allowlist must not be empty");
    }
    for pin in pins {
        validate_spki_pin(pin, label)?;
    }
    Ok(())
}

/// Validates one SHA-256 SPKI pin.
///
/// # Errors
///
/// Returns an error when the pin is not exactly 32 hexadecimal bytes.
pub fn validate_spki_pin(pin: &str, label: &str) -> Result<()> {
    let decoded = hex::decode(pin)
        .with_context(|| format!("{label} SPKI pin must be hexadecimal SHA-256"))?;
    if decoded.len() != 32 {
        bail!("{label} SPKI pin must contain exactly 32 bytes");
    }
    Ok(())
}

pub fn io_other(error: impl Into<anyhow::Error>) -> io::Error {
    io::Error::other(error.into())
}

#[cfg(test)]
mod tests {
    use crate::protocol::Role;

    use super::{PeerIdentity, require_peer, validate_spki_pins};

    #[test]
    fn spki_allowlists_are_required_and_strict() {
        assert!(validate_spki_pins(&[], "peer").is_err());
        assert!(validate_spki_pins(&["not-hex".to_owned()], "peer").is_err());
        assert!(validate_spki_pins(&["00".repeat(31)], "peer").is_err());
        assert!(validate_spki_pins(&["ab".repeat(32)], "peer").is_ok());
    }

    #[test]
    fn peer_authorization_requires_role_id_and_pin() {
        let identity = PeerIdentity {
            role: Role::Home,
            id: "home-1".to_owned(),
            spki_sha256: "ab".repeat(32),
            not_before_unix_secs: 1,
            not_after_unix_secs: u64::MAX,
        };
        assert!(require_peer(&identity, Role::Home, Some("home-1"), &["ab".repeat(32)]).is_ok());
        assert!(require_peer(&identity, Role::Home, Some("home-2"), &["ab".repeat(32)]).is_err());
        assert!(require_peer(&identity, Role::Home, Some("home-1"), &["cd".repeat(32)]).is_err());
        assert!(require_peer(&identity, Role::Travel, Some("home-1"), &["ab".repeat(32)]).is_err());
    }
}
