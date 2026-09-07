//! Headless, serving-only private PTY Home.
use anyhow::{Context, Result};
use clap::Parser;
use flowsplice_home_core::{HomeRuntime, HomeRuntimeConfig};
use flowsplice_pty_home::{PtyBackend, PtyDomainConfig};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};
#[derive(Parser)]
#[command(name = "flowsplice-pty-home", version)]
struct Args {
    #[arg(long)]
    config: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    home_runtime: PathBuf,
    domains: Vec<PtyDomainConfig>,
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let config: Config = toml::from_str(&std::fs::read_to_string(args.config)?)?;
    let home_config: HomeRuntimeConfig =
        toml::from_str(&std::fs::read_to_string(config.home_runtime)?)?;
    let backend = Arc::new(PtyBackend::open(config.domains).await?);
    let home = HomeRuntime::load(home_config, backend.clone())?;
    backend.authorize(
        home.business_service_grant()
            .context("PTY Home requires an approved business service grant")?,
    )?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        result = home.run_serving() => result,
        result = tokio::signal::ctrl_c() => result.map_err(Into::into),
        _ = terminate.recv() => Ok(()),
    };
    home.shutdown().await;
    backend.shutdown().await;
    result
}
