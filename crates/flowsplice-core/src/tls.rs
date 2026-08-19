use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::digest;
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, RootCertStore, ServerConfig,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::WebPkiSupportedAlgorithms,
    server::{ParsedCertificate, WebPkiClientVerifier},
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject};
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
    pub certificate_sha256: String,
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

/// Loads one private key through the no-follow, owner/mode-checked file path.
///
/// # Errors
///
/// Returns an error when the file boundary or PEM key is invalid.
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let bytes = read_private_key_file(path)?;
    PrivateKeyDer::from_pem_slice(&bytes)
        .with_context(|| format!("failed to parse private key {}", path.display()))
}

/// Rejects symlinks, non-regular files, and group/world-readable private keys.
///
/// # Errors
///
/// Returns an error when the path cannot safely be used as a private-key file.
pub fn validate_private_key_path(path: &Path) -> Result<()> {
    read_private_key_file(path).map(|_| ())
}

fn read_private_key_file(path: &Path) -> Result<Vec<u8>> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .with_context(|| format!("failed to open private key {}", path.display()))?;
    let mut file = fs::File::from(descriptor);
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect private key {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("private key {} must be a regular file", path.display());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "private key {} must not be accessible by group or other users",
            path.display()
        );
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        bail!(
            "private key {} must be owned by the current user",
            path.display()
        );
    }
    if metadata.len() > 1024 * 1024 {
        bail!("private key {} exceeds 1 MiB", path.display());
    }
    let capacity =
        usize::try_from(metadata.len()).context("private-key size does not fit usize")?;
    let mut bytes = Vec::with_capacity(capacity);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("failed to read private key {}", path.display()))?;
    Ok(bytes)
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

#[derive(Debug)]
struct CertificateChainVerifier {
    roots: RootCertStore,
    signature_algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for CertificateChainVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        let certificate = ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &certificate,
            &self.roots,
            intermediates,
            now,
            self.signature_algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.signature_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.signature_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.signature_algorithms.supported_schemes()
    }
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
        .with_single_cert(load_certs(cert)?, load_private_key(key)?)
        .context("failed to build TLS server config")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Builds a mutual-TLS client connector backed by rustls and AWS-LC.
///
/// # Errors
///
/// Returns an error when certificate material is missing, malformed, or inconsistent.
pub fn client_connector(cert: &Path, key: &Path, server_ca: &Path) -> Result<TlsConnector> {
    client_connector_with_private_key(cert, load_private_key(key)?, server_ca)
}

/// Builds a mutual-TLS client connector from an already decrypted private key.
///
/// # Errors
///
/// Returns an error when certificate material is missing, malformed, or inconsistent.
pub fn client_connector_with_private_key(
    cert: &Path,
    key: PrivateKeyDer<'static>,
    server_ca: &Path,
) -> Result<TlsConnector> {
    let config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(load_roots(server_ca)?)
        .with_client_auth_cert(load_certs(cert)?, key)
        .context("failed to build TLS client config")?;
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Builds an mTLS client connector that validates the certificate chain but leaves endpoint
/// identity to `FlowSplice`'s URI role/id and SPKI checks after the handshake.
///
/// # Errors
///
/// Returns an error when certificate material is missing, malformed, or inconsistent.
pub fn identity_client_connector_with_private_key(
    cert: &Path,
    key: PrivateKeyDer<'static>,
    server_ca: &Path,
) -> Result<TlsConnector> {
    let roots = load_roots(server_ca)?;
    let signature_algorithms =
        rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms;
    let verifier = CertificateChainVerifier {
        roots,
        signature_algorithms,
    };
    let config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(load_certs(cert)?, key)
        .context("failed to build identity-verified TLS client config")?;
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Builds an mTLS client connector that validates the certificate chain while `FlowSplice` checks
/// the peer's URI role/id and SPKI after the handshake.
///
/// # Errors
///
/// Returns an error when certificate material is missing, malformed, or inconsistent.
pub fn identity_client_connector(
    cert: &Path,
    key: &Path,
    server_ca: &Path,
) -> Result<TlsConnector> {
    identity_client_connector_with_private_key(cert, load_private_key(key)?, server_ca)
}

/// Returns the fixed SNI placeholder used by identity-verified connectors.
///
/// The connector deliberately does not use DNS names for identity; the application validates
/// the certificate's `FlowSplice` URI identity and SPKI immediately after the handshake.
///
/// # Errors
///
/// Returns an error if the internal placeholder is not a valid TLS server name.
pub fn identity_server_name() -> Result<ServerName<'static>> {
    server_name("flowsplice.invalid")
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
        certificate_sha256: hex::encode(digest::digest(&digest::SHA256, leaf.as_ref()).as_ref()),
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
            certificate_sha256: "cd".repeat(32),
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
