#![forbid(unsafe_code)]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use clap::{Parser, Subcommand};
use flowsplice_core::{
    authorization::load_json,
    deployment::{DeploymentTrust, SignedDeploymentTrust},
    init_crypto,
};
use flowsplice_enrollment::key::{
    MIN_PRIVATE_KEY_PASSWORD_CHARACTERS, generate_encrypted_private_key, load_private_key,
};
use zeroize::Zeroizing;

const ROOT_KEY_FILE: &str = "deployment-root.key";
const ROOT_PUBLIC_KEY_FILE: &str = "deployment-root.pub";

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    RootInit {
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long, hide = true)]
        test_password_file: Option<PathBuf>,
    },
    Sign {
        #[arg(long)]
        payload: PathBuf,
        #[arg(long)]
        root_key: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, hide = true)]
        test_password_file: Option<PathBuf>,
        #[arg(long, hide = true)]
        allow_unencrypted_test_key: bool,
    },
}

fn main() -> Result<()> {
    init_crypto();
    match Cli::parse().command {
        Command::RootInit {
            output_dir,
            test_password_file,
        } => root_init(&output_dir, test_password_file.as_deref()),
        Command::Sign {
            payload,
            root_key,
            output,
            test_password_file,
            allow_unencrypted_test_key,
        } => sign(
            &payload,
            &root_key,
            &output,
            test_password_file.as_deref(),
            allow_unencrypted_test_key,
        ),
    }
}

fn root_init(output_dir: &Path, test_password_file: Option<&Path>) -> Result<()> {
    if output_dir.exists() {
        bail!("deployment-root output directory already exists");
    }
    let password = password(test_password_file, true)?;
    if password.chars().count() < MIN_PRIVATE_KEY_PASSWORD_CHARACTERS {
        bail!(
            "deployment-root password must contain at least {MIN_PRIVATE_KEY_PASSWORD_CHARACTERS} characters"
        );
    }
    let generated = generate_encrypted_private_key(password.as_bytes())?;
    fs::create_dir(output_dir)?;
    fs::set_permissions(output_dir, fs::Permissions::from_mode(0o700))?;
    write_new(
        &output_dir.join(ROOT_KEY_FILE),
        generated.encrypted_pem.as_bytes(),
        0o600,
    )?;
    let public_key = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        &generated.key_pair.serialize_der(),
    )
    .map_err(|_| anyhow!("generated deployment root is not a P-256 PKCS#8 key"))?;
    write_new(
        &output_dir.join(ROOT_PUBLIC_KEY_FILE),
        format!("{}\n", hex::encode(public_key.public_key().as_ref())).as_bytes(),
        0o644,
    )?;
    println!(
        "created encrypted deployment root in {}",
        output_dir.display()
    );
    Ok(())
}

fn sign(
    payload_path: &Path,
    root_key_path: &Path,
    output: &Path,
    test_password_file: Option<&Path>,
    allow_unencrypted_test_key: bool,
) -> Result<()> {
    if output.exists() {
        bail!("refusing to replace existing signed deployment trust");
    }
    if allow_unencrypted_test_key
        && std::env::var("FLOWSPLICE_ALLOW_UNENCRYPTED_TEST_KEYS").as_deref() != Ok("1")
    {
        bail!("unencrypted deployment-root keys are disabled outside explicit tests");
    }
    let password = if allow_unencrypted_test_key {
        None
    } else {
        Some(password(test_password_file, false)?)
    };
    let private_key = Zeroizing::new(load_private_key(
        root_key_path,
        password.as_ref().map(|value| value.as_bytes()),
        allow_unencrypted_test_key,
    )?);
    let root_key =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, private_key.secret_der())
            .map_err(|_| anyhow!("deployment root is not a P-256 PKCS#8 private key"))?;
    let payload: DeploymentTrust = load_json(payload_path)?;
    let signed = SignedDeploymentTrust::sign(&payload, &root_key)?;
    let mut bytes = serde_json::to_vec_pretty(&signed)?;
    bytes.push(b'\n');
    write_new(output, &bytes, 0o644)?;
    println!("signed deployment trust {}", output.display());
    Ok(())
}

fn password(path: Option<&Path>, confirm: bool) -> Result<Zeroizing<String>> {
    if let Some(path) = path {
        if std::env::var("FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE").as_deref() != Ok("1") {
            bail!("test password files are disabled outside explicit tests");
        }
        let mut value = Zeroizing::new(fs::read_to_string(path)?);
        while value.ends_with(['\n', '\r']) {
            value.pop();
        }
        return Ok(value);
    }
    let value = Zeroizing::new(rpassword::prompt_password("Deployment-root password: ")?);
    if value.is_empty() {
        bail!("deployment-root password must not be empty");
    }
    if confirm {
        let confirmation = Zeroizing::new(rpassword::prompt_password("Confirm password: ")?);
        if value.as_bytes() != confirmation.as_bytes() {
            bail!("deployment-root passwords do not match");
        }
    }
    Ok(value)
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
