//! Resolve installed paths without changing the signed installation's original bytes.
use super::Config;
use anyhow::Result;
use flowsplice_core::config::{load_toml, resolve_path};
use std::path::{Component, Path, PathBuf};

fn paths(config: &mut Config) -> [(&mut PathBuf, &'static str); 10] {
    [
        (
            &mut config.deployment_root_public_key,
            "cert/deployment-root.pub",
        ),
        (&mut config.deployment_trust, "cert/deployment-trust.json"),
        (&mut config.management_cert, "cert/travel-management.crt"),
        (&mut config.management_key, "cert/travel-management.key"),
        (&mut config.management_ca, "cert/management-ca.crt"),
        (&mut config.business_cert, "cert/travel-business.crt"),
        (&mut config.business_key, "cert/travel-business.key"),
        (&mut config.business_ca, "cert/business-ca.crt"),
        (&mut config.state_store, "state/travel-state.redb"),
        (&mut config.enrollment_work_dir, "state/enrollment"),
    ]
}

pub(super) fn load(config_path: &Path) -> Result<Config> {
    let mut config: Config = load_toml(config_path)?;
    let directory = config_path.parent().unwrap_or_else(|| Path::new("."));
    let private_install = config_path
        .file_name()
        .is_some_and(|name| name == "travelagent.toml")
        && [
            flowsplice_enrollment::business::BUSINESS_BINDING_FILE,
            super::service_class::BINDING_FILE,
            super::business::INSTALL_JOURNAL_FILE,
        ]
        .iter()
        .any(|name| directory.join(name).is_file());
    let old_root = config
        .deployment_root_public_key
        .parent()
        .and_then(Path::parent)
        .map(Path::to_owned);
    let rebase = private_install
        && old_root.is_some_and(|root| {
            root.is_absolute()
                && root
                    .components()
                    .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
                && root.components().collect::<PathBuf>().as_os_str() == root.as_os_str()
                && paths(&mut config)
                    .iter()
                    .all(|(path, suffix)| path.as_os_str() == root.join(suffix).as_os_str())
        });
    for (path, suffix) in paths(&mut config) {
        *path = if rebase {
            directory.join(suffix)
        } else {
            resolve_path(config_path, path)
        };
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::fs;

    fn config_text(root: &Path) -> Result<String> {
        let mut config: Config = toml::from_str(
            r#"
            id = "test-travel"
            seed_relays = []
            homes = []
            deployment_root_public_key = ""
            deployment_trust = ""
            management_cert = ""
            management_key = ""
            management_ca = ""
            business_cert = ""
            business_key = ""
            business_ca = ""
            state_store = ""
            enrollment_work_dir = ""
            ui_listen = "127.0.0.1:0"
        "#,
        )?;
        let names = [
            "deployment_root_public_key",
            "deployment_trust",
            "management_cert",
            "management_key",
            "management_ca",
            "business_cert",
            "business_key",
            "business_ca",
            "state_store",
            "enrollment_work_dir",
        ];
        let mut value = toml::Table::new();
        value.insert("id".into(), "test-travel".into());
        value.insert("seed_relays".into(), toml::Value::Array(vec![]));
        value.insert("homes".into(), toml::Value::Array(vec![]));
        value.insert("ui_listen".into(), "127.0.0.1:0".into());
        for (name, (_, suffix)) in names.into_iter().zip(paths(&mut config)) {
            value.insert(
                name.into(),
                root.join(suffix)
                    .to_str()
                    .context("test path is not UTF-8")?
                    .into(),
            );
        }
        Ok(toml::to_string(&value)?)
    }

    #[test]
    fn relocated_generated_private_install_prefers_current_root_without_rewriting() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let old = temp.path().join("old container still exists");
        let current = temp.path().join("new container with spaces");
        fs::create_dir_all(&old)?;
        fs::create_dir_all(&current)?;
        let config_path = current.join("travelagent.toml");
        let text = config_text(&old)?;
        fs::write(&config_path, &text)?;
        for marker in [
            flowsplice_enrollment::business::BUSINESS_BINDING_FILE,
            super::super::service_class::BINDING_FILE,
            super::super::business::INSTALL_JOURNAL_FILE,
        ] {
            let marker_path = current.join(marker);
            fs::write(&marker_path, b"unchanged binding or journal bytes")?;
            let mut loaded = load(&config_path)?;
            for (path, suffix) in paths(&mut loaded) {
                assert_eq!(*path, current.join(suffix));
            }
            assert_eq!(fs::read(&config_path)?, text.as_bytes());
            assert_eq!(
                fs::read(&marker_path)?,
                b"unchanged binding or journal bytes"
            );
            fs::remove_file(marker_path)?;
        }
        // An ordinary Travel configuration is never implicitly relocated.
        let mut loaded = load(&config_path)?;
        for (path, suffix) in paths(&mut loaded) {
            assert_eq!(*path, old.join(suffix));
        }
        Ok(())
    }

    #[test]
    fn relative_paths_follow_config_directory_not_process_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("travelagent.toml");
        fs::write(&config_path, config_text(Path::new(""))?)?;
        assert_ne!(std::env::current_dir()?, temp.path());
        let mut loaded = load(&config_path)?;
        for (path, suffix) in paths(&mut loaded) {
            assert_eq!(*path, temp.path().join(suffix));
        }
        Ok(())
    }

    #[test]
    fn custom_mixed_and_noninstallation_paths_are_preserved() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let old = temp.path().join("old");
        fs::write(
            temp.path()
                .join(flowsplice_enrollment::business::BUSINESS_BINDING_FILE),
            b"marker",
        )?;
        for (filename, replacement) in [
            (
                "travelagent.toml",
                Some(temp.path().join("custom/travel-business.key")),
            ),
            (
                "travelagent.toml",
                Some(PathBuf::from("custom/travel-business.key")),
            ),
            (
                "travelagent.toml",
                Some(old.join("cert/../cert/travel-business.key")),
            ),
            ("general.toml", None),
        ] {
            let mut value: toml::Table = toml::from_str(&config_text(&old)?)?;
            if let Some(path) = &replacement {
                value.insert(
                    "business_key".into(),
                    path.to_str().context("test path is not UTF-8")?.into(),
                );
            }
            let config_path = temp.path().join(filename);
            fs::write(&config_path, toml::to_string(&value)?)?;
            let mut loaded = load(&config_path)?;
            assert_eq!(
                loaded.business_key,
                resolve_path(
                    &config_path,
                    replacement
                        .as_deref()
                        .unwrap_or(&old.join("cert/travel-business.key"))
                )
            );
            for (path, suffix) in paths(&mut loaded) {
                if suffix != "cert/travel-business.key" {
                    assert_eq!(*path, old.join(suffix));
                }
            }
        }
        Ok(())
    }
}
