#![forbid(unsafe_code)]

use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use flowsplice_enrollment::{
    TravelEnrollmentApproval,
    issuer::{OfflineIssuerMaterial, ProtectedKey, issue_enrollment},
    key::is_encrypted_private_key,
    load_json, write_json_private,
};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(version, about = "Offline FlowSplice Travel credential issuer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Issue(IssueArgs),
}

#[derive(Args)]
struct IssueArgs {
    #[arg(long)]
    approval: PathBuf,
    #[arg(long)]
    management_ca_cert: PathBuf,
    #[arg(long)]
    management_ca_key: PathBuf,
    #[arg(long)]
    business_ca_cert: PathBuf,
    #[arg(long)]
    business_ca_key: PathBuf,
    #[arg(long)]
    travel_authority_key: PathBuf,
    #[arg(long)]
    travel_authority_public_key: String,
    #[arg(long)]
    output: PathBuf,
    #[arg(long, hide = true)]
    test_password_file: Option<PathBuf>,
    #[arg(long, hide = true)]
    allow_unencrypted_test_keys: bool,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Issue(args) => issue(&args),
    }
}

fn issue(args: &IssueArgs) -> Result<()> {
    let allow_unencrypted = allow_test_unencrypted(args.allow_unencrypted_test_keys)?;
    let test_password = load_test_password(args.test_password_file.as_deref())?;
    let test_password_ref = test_password.as_ref().map(|password| password.as_str());
    let management_password = prompt_if_needed(
        &args.management_ca_key,
        "Management CA private-key password: ",
        test_password_ref,
    )?;
    let business_password = prompt_if_needed(
        &args.business_ca_key,
        "Business CA private-key password: ",
        test_password_ref,
    )?;
    let authority_password = prompt_if_needed(
        &args.travel_authority_key,
        "Travel authorization private-key password: ",
        test_password_ref,
    )?;
    let management_password_ref = management_password
        .as_ref()
        .map(|password| password.as_str());
    let business_password_ref = business_password.as_ref().map(|password| password.as_str());
    let authority_password_ref = authority_password
        .as_ref()
        .map(|password| password.as_str());
    let approval: TravelEnrollmentApproval = load_json(&args.approval)?;
    let material = OfflineIssuerMaterial {
        management_ca_certificate: &args.management_ca_cert,
        management_ca_key: ProtectedKey {
            path: &args.management_ca_key,
            password: selected_password(test_password_ref, management_password_ref),
            allow_unencrypted,
        },
        business_ca_certificate: &args.business_ca_cert,
        business_ca_key: ProtectedKey {
            path: &args.business_ca_key,
            password: selected_password(test_password_ref, business_password_ref),
            allow_unencrypted,
        },
        travel_authority_key: ProtectedKey {
            path: &args.travel_authority_key,
            password: selected_password(test_password_ref, authority_password_ref),
            allow_unencrypted,
        },
        expected_travel_authority_public_key: &args.travel_authority_public_key,
    };
    let response = issue_enrollment(approval, &material, unix_time_secs()?)?;
    write_json_private(&args.output, &response)?;
    println!(
        "issued Travel enrollment response: {}",
        args.output.display()
    );
    Ok(())
}

fn selected_password<'a>(
    test_password: Option<&'a str>,
    prompted_password: Option<&'a str>,
) -> Option<&'a [u8]> {
    test_password.or(prompted_password).map(str::as_bytes)
}

fn prompt_if_needed(
    key_path: &Path,
    prompt: &str,
    test_password: Option<&str>,
) -> Result<Option<Zeroizing<String>>> {
    if test_password.is_some() || !is_encrypted_private_key(key_path)? {
        return Ok(None);
    }
    let password = Zeroizing::new(rpassword::prompt_password(prompt)?);
    if password.is_empty() {
        bail!("private-key password must not be empty");
    }
    Ok(Some(password))
}

fn load_test_password(path: Option<&Path>) -> Result<Option<Zeroizing<String>>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if env::var("FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE").as_deref() != Ok("1") {
        bail!("--test-password-file is disabled outside the explicit test environment");
    }
    let mut password = Zeroizing::new(
        fs::read_to_string(path)
            .with_context(|| format!("failed to read test password file {}", path.display()))?,
    );
    while password.ends_with(['\r', '\n']) {
        password.pop();
    }
    if password.is_empty() {
        bail!("test password file must not be empty");
    }
    Ok(Some(password))
}

fn allow_test_unencrypted(requested: bool) -> Result<bool> {
    if requested && env::var("FLOWSPLICE_ALLOW_UNENCRYPTED_TEST_KEYS").as_deref() != Ok("1") {
        bail!("--allow-unencrypted-test-keys is disabled outside the explicit test environment");
    }
    Ok(requested)
}

fn unix_time_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock predates the Unix epoch")?
        .as_secs())
}
