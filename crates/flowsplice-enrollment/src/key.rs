use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use pkcs8::{EncryptedPrivateKeyInfo, LineEnding, PrivateKeyInfo, der::SecretDocument};
use rand_core::OsRng;
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls_pki_types::{PrivateKeyDer, pem::PemObject};
use zeroize::Zeroizing;

pub const ENCRYPTED_KEY_LABEL: &str = "ENCRYPTED PRIVATE KEY";

pub struct GeneratedPrivateKey {
    pub key_pair: KeyPair,
    pub encrypted_pem: Zeroizing<String>,
}

/// Generates a P-256 key and encrypts its PKCS#8 representation with the password.
///
/// # Errors
///
/// Returns an error when the password is empty or key generation/encryption fails.
pub fn generate_encrypted_private_key(password: &[u8]) -> Result<GeneratedPrivateKey> {
    if password.is_empty() {
        bail!("private-key password must not be empty");
    }
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .context("failed to generate P-256 private key")?;
    let serialized = Zeroizing::new(key_pair.serialize_der());
    let private_key = PrivateKeyInfo::try_from(serialized.as_slice())
        .context("generated private key is not valid PKCS#8")?;
    let encrypted = private_key
        .encrypt(OsRng, password)
        .context("failed to encrypt generated private key")?;
    let encrypted_pem = encrypted
        .to_pem(ENCRYPTED_KEY_LABEL, LineEnding::LF)
        .context("failed to encode encrypted private key")?;
    Ok(GeneratedPrivateKey {
        key_pair,
        encrypted_pem,
    })
}

/// Returns whether a key file is an encrypted PKCS#8 PEM object.
///
/// # Errors
///
/// Returns an error when the key file cannot be read.
pub fn is_encrypted_private_key(path: &Path) -> Result<bool> {
    let data =
        fs::read(path).with_context(|| format!("failed to read private key {}", path.display()))?;
    Ok(data
        .windows(b"-----BEGIN ENCRYPTED PRIVATE KEY-----".len())
        .any(|window| window == b"-----BEGIN ENCRYPTED PRIVATE KEY-----"))
}

/// Loads an encrypted PKCS#8 key, or an explicitly permitted unencrypted test key.
///
/// # Errors
///
/// Returns an error for missing passwords, decryption failures, malformed keys, or a forbidden
/// unencrypted key.
pub fn load_private_key(
    path: &Path,
    password: Option<&[u8]>,
    allow_unencrypted: bool,
) -> Result<PrivateKeyDer<'static>> {
    if is_encrypted_private_key(path)? {
        let password =
            password.ok_or_else(|| anyhow!("encrypted private key requires a password"))?;
        let pem = fs::read_to_string(path)
            .with_context(|| format!("failed to read private key {}", path.display()))?;
        let (label, document) = SecretDocument::from_pem(&pem)
            .context("failed to parse encrypted PKCS#8 private key")?;
        if label != ENCRYPTED_KEY_LABEL {
            bail!("encrypted private key has an unexpected PEM label");
        }
        let encrypted = EncryptedPrivateKeyInfo::try_from(document.as_bytes())
            .context("failed to decode encrypted PKCS#8 private key")?;
        let decrypted = encrypted
            .decrypt(password)
            .context("failed to decrypt private key")?;
        return PrivateKeyDer::try_from(decrypted.as_bytes().to_vec())
            .map_err(|error| anyhow!("decrypted PKCS#8 private key is invalid: {error}"));
    }
    if !allow_unencrypted {
        bail!("unencrypted private keys are forbidden");
    }
    PrivateKeyDer::from_pem_file(path)
        .with_context(|| format!("failed to parse private key {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn encrypted_key_round_trip_and_wrong_password_rejection() -> Result<()> {
        let generated = generate_encrypted_private_key(b"correct horse battery staple")?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("travel.key");
        let mut file = fs::File::create(&path)?;
        file.write_all(generated.encrypted_pem.as_bytes())?;
        assert!(is_encrypted_private_key(&path)?);
        assert!(load_private_key(&path, None, false).is_err());
        assert!(load_private_key(&path, Some(b"wrong"), false).is_err());
        assert!(load_private_key(&path, Some(b"correct horse battery staple"), false).is_ok());
        Ok(())
    }
}
