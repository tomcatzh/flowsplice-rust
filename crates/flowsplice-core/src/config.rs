use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

/// Loads and deserializes a TOML configuration file.
///
/// # Errors
///
/// Returns an error when the file cannot be read or its contents do not match `T`.
pub fn load_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml_from_str(&data, path)
}

fn toml_from_str<T: DeserializeOwned>(data: &str, path: &Path) -> Result<T> {
    toml::from_str(data).with_context(|| format!("failed to parse config {}", path.display()))
}
