use std::{
    fs,
    path::{Path, PathBuf},
};

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

/// Resolves a configured path relative to the configuration file that declares it.
///
/// Absolute configured paths are returned unchanged. This keeps deployment packages relocatable
/// without making their interpretation depend on the process working directory.
#[must_use]
pub fn resolve_path(config_path: &Path, configured_path: &Path) -> PathBuf {
    if configured_path.is_absolute() {
        return configured_path.to_owned();
    }
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(configured_path)
}

fn toml_from_str<T: DeserializeOwned>(data: &str, path: &Path) -> Result<T> {
    toml::from_str(data).with_context(|| format!("failed to parse config {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::resolve_path;
    use std::path::Path;

    #[test]
    fn configured_paths_are_relative_to_their_config_file() {
        assert_eq!(
            resolve_path(
                Path::new("/package/home-bootstrap.toml"),
                Path::new("trust/deployment-root.pub")
            ),
            Path::new("/package/trust/deployment-root.pub")
        );
        assert_eq!(
            resolve_path(
                Path::new("/package/home-bootstrap.toml"),
                Path::new("/etc/flowsplice/deployment-root.pub")
            ),
            Path::new("/etc/flowsplice/deployment-root.pub")
        );
    }
}
