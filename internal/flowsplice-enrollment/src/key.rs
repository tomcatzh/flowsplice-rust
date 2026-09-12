use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::digest;
use pkcs8::{EncryptedPrivateKeyInfo, LineEnding, PrivateKeyInfo, der::SecretDocument};
use rand_core::OsRng;
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls_pki_types::{PrivateKeyDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const ENCRYPTED_KEY_LABEL: &str = "ENCRYPTED PRIVATE KEY";
pub const MIN_PRIVATE_KEY_PASSWORD_CHARACTERS: usize = 12;

const ROTATION_JOURNAL_FILE: &str = ".flowsplice-private-key-rotation.json";
const ROTATION_JOURNAL_VERSION: u32 = 1;

#[derive(Clone, Copy)]
pub struct PrivateKeyRotationTarget<'a> {
    pub label: &'a str,
    pub path: &'a Path,
}

struct ResolvedRotationTarget {
    label: String,
    path: PathBuf,
    file_name: String,
}

struct StagedRotation {
    path: PathBuf,
    pem: Zeroizing<String>,
    original_der: Zeroizing<Vec<u8>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RotationJournal {
    version: u32,
    entries: Vec<RotationJournalEntry>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RotationJournalEntry {
    target_file: String,
    staged_file: String,
    encrypted_sha256: String,
}

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
    let data = read_private_key_file(path)?;
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
    let data = read_private_key_file(path)?;
    if data
        .windows(b"-----BEGIN ENCRYPTED PRIVATE KEY-----".len())
        .any(|window| window == b"-----BEGIN ENCRYPTED PRIVATE KEY-----")
    {
        let password =
            password.ok_or_else(|| anyhow!("encrypted private key requires a password"))?;
        let pem = std::str::from_utf8(&data).context("private key PEM is not UTF-8")?;
        let (label, document) = SecretDocument::from_pem(pem)
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
    PrivateKeyDer::from_pem_slice(&data)
        .with_context(|| format!("failed to parse private key {}", path.display()))
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
    let mut data = Vec::with_capacity(capacity);
    file.read_to_end(&mut data)
        .with_context(|| format!("failed to read private key {}", path.display()))?;
    Ok(data)
}

/// Re-encrypts a set of PKCS#8 private keys with one new password.
///
/// Every key is decrypted and staged before a small password-free recovery journal is published.
/// The staged files are then atomically renamed over the originals. All targets must be regular
/// files in one directory so an interrupted multi-file replacement can be completed on restart.
///
/// # Errors
///
/// Returns an error for invalid passwords, inconsistent target paths, decryption or encryption
/// failures, or durable file replacement failures.
pub fn rotate_private_key_passwords(
    targets: &[PrivateKeyRotationTarget<'_>],
    current_password: &str,
    new_password: &str,
) -> Result<()> {
    if current_password.is_empty() {
        bail!("current private-key password must not be empty");
    }
    if new_password.chars().count() < MIN_PRIVATE_KEY_PASSWORD_CHARACTERS {
        bail!(
            "new private-key password must contain at least {MIN_PRIVATE_KEY_PASSWORD_CHARACTERS} characters"
        );
    }
    if current_password == new_password {
        bail!("new private-key password must differ from the current password");
    }
    let (directory, resolved) = resolve_rotation_targets(targets)?;
    recover_resolved_rotation(&directory, &resolved)?;
    let (journal, staged) = stage_rotation(
        &directory,
        &resolved,
        current_password.as_bytes(),
        new_password.as_bytes(),
    )?;
    if let Err(error) = publish_rotation_journal(&directory, &journal) {
        if !directory.join(ROTATION_JOURNAL_FILE).exists() {
            remove_staged_files(&staged);
        }
        return Err(error);
    }
    apply_rotation_journal(&directory, &resolved, &journal)
}

/// Completes a previously journaled private-key password rotation, if one exists.
///
/// # Errors
///
/// Returns an error when the journal does not match the configured key set or durable replacement
/// cannot be completed.
pub fn recover_private_key_password_rotation(
    targets: &[PrivateKeyRotationTarget<'_>],
) -> Result<bool> {
    if targets.is_empty() {
        bail!("private-key password rotation requires at least one key");
    }
    let mut journal_directories = HashSet::new();
    for target in targets {
        let parent = target
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory = fs::canonicalize(parent)
            .with_context(|| format!("failed to resolve {} private-key directory", target.label))?;
        if directory.join(ROTATION_JOURNAL_FILE).exists() {
            journal_directories.insert(directory);
        }
    }
    if journal_directories.is_empty() {
        return Ok(false);
    }
    if journal_directories.len() != 1 {
        bail!("multiple private-key password rotation journals require manual recovery");
    }
    let (directory, resolved) = resolve_rotation_targets(targets)?;
    recover_resolved_rotation(&directory, &resolved)
}

fn resolve_rotation_targets(
    targets: &[PrivateKeyRotationTarget<'_>],
) -> Result<(PathBuf, Vec<ResolvedRotationTarget>)> {
    if targets.is_empty() {
        bail!("private-key password rotation requires at least one key");
    }
    let mut expected_directory = None;
    let mut file_names = HashSet::new();
    let mut resolved = Vec::with_capacity(targets.len());
    for target in targets {
        let metadata = fs::symlink_metadata(target.path)
            .with_context(|| format!("failed to inspect {} private key", target.label))?;
        if !metadata.file_type().is_file() {
            bail!("{} private key must be a regular file", target.label);
        }
        let parent = target
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory = fs::canonicalize(parent)
            .with_context(|| format!("failed to resolve {} private-key directory", target.label))?;
        if expected_directory
            .as_ref()
            .is_some_and(|expected| expected != &directory)
        {
            bail!("all rotated private keys must be stored in one directory");
        }
        expected_directory.get_or_insert_with(|| directory.clone());
        let file_name = target
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("{} private key has an invalid file name", target.label))?
            .to_owned();
        if !file_names.insert(file_name.clone()) {
            bail!("private-key rotation targets must be unique");
        }
        resolved.push(ResolvedRotationTarget {
            label: target.label.to_owned(),
            path: directory.join(&file_name),
            file_name,
        });
    }
    Ok((
        expected_directory.ok_or_else(|| anyhow!("private-key directory is missing"))?,
        resolved,
    ))
}

fn stage_rotation(
    directory: &Path,
    targets: &[ResolvedRotationTarget],
    current_password: &[u8],
    new_password: &[u8],
) -> Result<(RotationJournal, Vec<StagedRotation>)> {
    let transaction_id = Uuid::new_v4();
    let mut staged = Vec::with_capacity(targets.len());
    let result = (|| -> Result<RotationJournal> {
        for target in targets {
            let private_key = Zeroizing::new(
                load_private_key(&target.path, Some(current_password), false)
                    .with_context(|| format!("failed to decrypt {} private key", target.label))?
                    .secret_der()
                    .to_vec(),
            );
            let private_key_info = PrivateKeyInfo::try_from(private_key.as_slice())
                .with_context(|| format!("{} private key is not valid PKCS#8", target.label))?;
            let encrypted = private_key_info
                .encrypt(OsRng, new_password)
                .with_context(|| format!("failed to re-encrypt {} private key", target.label))?;
            let pem = encrypted
                .to_pem(ENCRYPTED_KEY_LABEL, LineEnding::LF)
                .with_context(|| format!("failed to encode {} private key", target.label))?;
            let staged_file = format!(".{}.flowsplice-{}.new", target.file_name, transaction_id);
            staged.push(StagedRotation {
                path: directory.join(staged_file),
                pem,
                original_der: private_key,
            });
        }
        for (target, staged_key) in targets.iter().zip(&staged) {
            write_private_file(&staged_key.path, staged_key.pem.as_bytes())
                .with_context(|| format!("failed to stage {} private key", target.label))?;
            let verified = Zeroizing::new(
                load_private_key(&staged_key.path, Some(new_password), false)
                    .with_context(|| {
                        format!("failed to verify staged {} private key", target.label)
                    })?
                    .secret_der()
                    .to_vec(),
            );
            if verified.as_slice() != staged_key.original_der.as_slice() {
                bail!("staged {} private key changed key material", target.label);
            }
        }
        let mut entries = Vec::with_capacity(targets.len());
        for (target, staged_key) in targets.iter().zip(&staged) {
            entries.push(RotationJournalEntry {
                target_file: target.file_name.clone(),
                staged_file: staged_key
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("staged private key has an invalid file name"))?
                    .to_owned(),
                encrypted_sha256: file_sha256(&staged_key.path)?,
            });
        }
        Ok(RotationJournal {
            version: ROTATION_JOURNAL_VERSION,
            entries,
        })
    })();
    match result {
        Ok(journal) => Ok((journal, staged)),
        Err(error) => {
            remove_staged_files(&staged);
            Err(error)
        }
    }
}

fn recover_resolved_rotation(directory: &Path, targets: &[ResolvedRotationTarget]) -> Result<bool> {
    let journal_path = directory.join(ROTATION_JOURNAL_FILE);
    if !journal_path.exists() {
        return Ok(false);
    }
    let journal: RotationJournal = serde_json::from_slice(
        &fs::read(&journal_path).context("failed to read private-key rotation journal")?,
    )
    .context("failed to parse private-key rotation journal")?;
    apply_rotation_journal(directory, targets, &journal)?;
    Ok(true)
}

fn publish_rotation_journal(directory: &Path, journal: &RotationJournal) -> Result<()> {
    let journal_path = directory.join(ROTATION_JOURNAL_FILE);
    if journal_path.exists() {
        bail!("private-key rotation journal already exists");
    }
    let temporary = directory.join(format!(
        ".flowsplice-private-key-rotation-{}.tmp",
        Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .context("failed to create private-key rotation journal")?;
        let encoded = serde_json::to_vec_pretty(journal)
            .context("failed to encode private-key rotation journal")?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &journal_path)
            .context("failed to publish private-key rotation journal")?;
        sync_directory(directory)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn apply_rotation_journal(
    directory: &Path,
    targets: &[ResolvedRotationTarget],
    journal: &RotationJournal,
) -> Result<()> {
    validate_rotation_journal(targets, journal)?;
    for entry in &journal.entries {
        let target = directory.join(&entry.target_file);
        let staged = directory.join(&entry.staged_file);
        if staged.exists() {
            if file_sha256(&staged)? != entry.encrypted_sha256 {
                bail!("staged private key hash does not match rotation journal");
            }
            fs::rename(&staged, &target)
                .with_context(|| format!("failed to replace private key {}", entry.target_file))?;
            sync_directory(directory)?;
        } else if file_sha256(&target)? != entry.encrypted_sha256 {
            bail!("private-key rotation cannot determine a safe recovery state");
        }
    }
    let journal_path = directory.join(ROTATION_JOURNAL_FILE);
    if journal_path.exists() {
        fs::remove_file(&journal_path).context("failed to remove private-key rotation journal")?;
        sync_directory(directory)?;
    }
    Ok(())
}

fn validate_rotation_journal(
    targets: &[ResolvedRotationTarget],
    journal: &RotationJournal,
) -> Result<()> {
    if journal.version != ROTATION_JOURNAL_VERSION || journal.entries.len() != targets.len() {
        bail!("private-key rotation journal does not match this version or key set");
    }
    let expected = targets
        .iter()
        .map(|target| target.file_name.as_str())
        .collect::<HashSet<_>>();
    let mut actual = HashSet::new();
    for entry in &journal.entries {
        if !expected.contains(entry.target_file.as_str())
            || !actual.insert(entry.target_file.as_str())
            || Path::new(&entry.staged_file)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(entry.staged_file.as_str())
            || !entry
                .staged_file
                .starts_with(&format!(".{}.flowsplice-", entry.target_file))
            || !Path::new(&entry.staged_file)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("new"))
            || entry.encrypted_sha256.len() != 64
        {
            bail!("private-key rotation journal contains an invalid entry");
        }
    }
    if actual != expected {
        bail!("private-key rotation journal targets do not match configured keys");
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(hex::encode(
        digest::digest(&digest::SHA256, &bytes).as_ref(),
    ))
}

fn sync_directory(directory: &Path) -> Result<()> {
    fs::File::open(directory)
        .with_context(|| format!("failed to open directory {}", directory.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", directory.display()))
}

fn remove_staged_files(staged: &[StagedRotation]) {
    for staged_key in staged {
        let _ = fs::remove_file(&staged_key.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn create_encrypted_key(path: &Path, password: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let generated = generate_encrypted_private_key(password)?;
        fs::write(path, generated.encrypted_pem.as_bytes())?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Zeroizing::new(generated.key_pair.serialize_der()))
    }

    #[test]
    fn encrypted_key_round_trip_and_wrong_password_rejection() -> Result<()> {
        let generated = generate_encrypted_private_key(b"correct horse battery staple")?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("travel.key");
        let mut file = fs::File::create(&path)?;
        file.write_all(generated.encrypted_pem.as_bytes())?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        assert!(is_encrypted_private_key(&path)?);
        assert!(load_private_key(&path, None, false).is_err());
        assert!(load_private_key(&path, Some(b"wrong"), false).is_err());
        assert!(load_private_key(&path, Some(b"correct horse battery staple"), false).is_ok());
        Ok(())
    }

    #[test]
    fn private_key_loader_rejects_wide_permissions_and_symlinks() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("travel.key");
        create_encrypted_key(&path, b"correct horse battery staple")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(load_private_key(&path, Some(b"correct horse battery staple"), false).is_err());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let link = directory.path().join("travel-link.key");
        symlink(&path, &link)?;
        assert!(load_private_key(&link, Some(b"correct horse battery staple"), false).is_err());
        Ok(())
    }

    #[test]
    fn password_rotation_validates_every_key_before_replacement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let management_path = directory.path().join("management.key");
        let business_path = directory.path().join("business.key");
        let management_der = create_encrypted_key(&management_path, b"old password value")?;
        let business_der = create_encrypted_key(&business_path, b"old password value")?;
        let management_before = fs::read(&management_path)?;
        let business_before = fs::read(&business_path)?;
        let targets = [
            PrivateKeyRotationTarget {
                label: "management",
                path: &management_path,
            },
            PrivateKeyRotationTarget {
                label: "business",
                path: &business_path,
            },
        ];

        assert!(
            rotate_private_key_passwords(&targets, "wrong password value", "new password value",)
                .is_err()
        );
        assert_eq!(fs::read(&management_path)?, management_before);
        assert_eq!(fs::read(&business_path)?, business_before);

        rotate_private_key_passwords(&targets, "old password value", "new password value")?;
        assert!(load_private_key(&management_path, Some(b"old password value"), false).is_err());
        assert!(load_private_key(&business_path, Some(b"old password value"), false).is_err());
        assert_eq!(
            load_private_key(&management_path, Some(b"new password value"), false)?.secret_der(),
            management_der.as_slice(),
        );
        assert_eq!(
            load_private_key(&business_path, Some(b"new password value"), false)?.secret_der(),
            business_der.as_slice(),
        );
        assert_eq!(
            fs::metadata(&management_path)?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&business_path)?.permissions().mode() & 0o777,
            0o600
        );
        assert!(!directory.path().join(ROTATION_JOURNAL_FILE).exists());
        Ok(())
    }

    #[test]
    fn interrupted_password_rotation_is_completed_from_journal() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let management_path = directory.path().join("management.key");
        let business_path = directory.path().join("business.key");
        let management_der = create_encrypted_key(&management_path, b"old password value")?;
        let business_der = create_encrypted_key(&business_path, b"old password value")?;
        let targets = [
            PrivateKeyRotationTarget {
                label: "management",
                path: &management_path,
            },
            PrivateKeyRotationTarget {
                label: "business",
                path: &business_path,
            },
        ];
        let (resolved_directory, resolved) = resolve_rotation_targets(&targets)?;
        let (journal, staged) = stage_rotation(
            &resolved_directory,
            &resolved,
            b"old password value",
            b"new password value",
        )?;
        publish_rotation_journal(&resolved_directory, &journal)?;
        fs::rename(&staged[0].path, &resolved[0].path)?;
        sync_directory(&resolved_directory)?;

        assert!(recover_private_key_password_rotation(&targets)?);
        assert_eq!(
            load_private_key(&management_path, Some(b"new password value"), false)?.secret_der(),
            management_der.as_slice(),
        );
        assert_eq!(
            load_private_key(&business_path, Some(b"new password value"), false)?.secret_der(),
            business_der.as_slice(),
        );
        assert!(!directory.path().join(ROTATION_JOURNAL_FILE).exists());
        Ok(())
    }
}
