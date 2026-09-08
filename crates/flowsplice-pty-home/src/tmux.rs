//! Fixed tmux operations. Remote clients never supply commands, paths or targets.
use anyhow::{Context, Result, bail};
use portable_pty::CommandBuilder;
use serde::Deserialize;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    time::Duration,
};
use tokio::process::Command;
use uuid::Uuid;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmuxConfig {
    pub binary: PathBuf,
    pub socket: PathBuf,
    pub shell: PathBuf,
    pub working_directory: PathBuf,
}

pub struct Tmux {
    config: TmuxConfig,
    config_file: PathBuf,
    _lock: fs::File,
}
impl Tmux {
    pub async fn open(config: TmuxConfig) -> Result<Self> {
        for path in [
            &config.binary,
            &config.socket,
            &config.shell,
            &config.working_directory,
        ] {
            if !path.is_absolute() || path.to_string_lossy().chars().any(char::is_control) {
                bail!("tmux configuration requires absolute paths without control characters");
            }
        }
        if config.socket.as_os_str().len() > 90 || !config.working_directory.is_dir() {
            bail!("invalid tmux socket path or working directory");
        }
        if matches!(
            config.shell.file_name().and_then(|s| s.to_str()),
            Some("false" | "nologin")
        ) {
            bail!("configured account shell cannot run interactive sessions");
        }
        for path in [&config.binary, &config.shell] {
            let metadata = fs::metadata(path)?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                bail!("configured tmux binary and shell must be executable files");
            }
        }
        let directory = config
            .socket
            .parent()
            .context("tmux socket needs a private directory")?;
        if !directory.exists() {
            fs::create_dir_all(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            bail!("tmux directory must be owned by this account and private (0700)");
        }
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("flowsplice-home.lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("this tmux domain is already served by another Home process")?;
        let config_file = directory.join("flowsplice-tmux.conf");
        fs::write(
            &config_file,
            "set -s exit-unattached off\nset -s exit-empty on\nset -g status off\nset -g update-environment ''\nset -g remain-on-exit off\n",
        )?;
        fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600))?;
        let this = Self {
            config,
            config_file,
            _lock: lock,
        };
        let output = Command::new(&this.config.binary).arg("-V").output().await?;
        let version = String::from_utf8(output.stdout)?;
        let numeric = version
            .trim()
            .strip_prefix("tmux ")
            .context("unrecognized tmux version")?;
        let mut parts = numeric.split('.');
        let major: u32 = parts
            .next()
            .context("missing tmux major version")?
            .parse()?;
        let minor: u32 = parts
            .next()
            .context("missing tmux minor version")?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()?;
        if !output.status.success() || major < 3 || (major == 3 && minor < 3) {
            bail!("tmux 3.3 or newer is required");
        }
        Ok(this)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.config.binary);
        command
            .arg("-u")
            .arg("-S")
            .arg(&self.config.socket)
            .arg("-f")
            .arg(&self.config_file)
            .env_remove("TMUX")
            .env("TERM", "xterm-256color")
            .kill_on_drop(true);
        command
    }
    async fn execute(&self, args: &[String]) -> Result<std::process::Output> {
        tokio::time::timeout(Duration::from_secs(5), self.command().args(args).output())
            .await
            .context("tmux command timed out")?
            .context("tmux command failed")
    }
    pub async fn list(&self) -> Result<Vec<(Uuid, u64)>> {
        if !self.config.socket.exists() {
            return Ok(Vec::new());
        }
        let mut output = self
            .execute(&[
                "list-sessions".into(),
                "-F".into(),
                "#{session_name}|#{session_created}".into(),
            ])
            .await?;
        // The last shell can exit after the client connects but before it gets
        // its list reply. Re-query the daemon's actual state before reporting
        // failure; never infer an empty list from an arbitrary command error.
        for _ in 0..2 {
            if output.status.success() || !self.config.socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            output = self
                .execute(&[
                    "list-sessions".into(),
                    "-F".into(),
                    "#{session_name}|#{session_created}".into(),
                ])
                .await?;
        }
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.contains("no server running")
                || error.contains("no sessions")
                || error.contains("Connection refused")
                || !self.config.socket.exists()
            {
                return Ok(Vec::new());
            }
            bail!("could not list tmux sessions: {}", error.trim());
        }
        let mut sessions = Vec::new();
        for line in String::from_utf8(output.stdout)?.lines() {
            let Some((name, created)) = line.split_once('|') else {
                bail!("invalid tmux session metadata");
            };
            if let Some(id) = name.strip_prefix("fs-") {
                sessions.push((Uuid::parse_str(id)?, created.parse()?));
            }
        }
        if sessions.len() > 128 {
            bail!("tmux session limit exceeded");
        }
        Ok(sessions)
    }
    pub async fn new_session(&self, id: Uuid, columns: u16, rows: u16) -> Result<()> {
        let output = self
            .execute(&[
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                Self::name(id),
                "-x".into(),
                columns.to_string(),
                "-y".into(),
                rows.to_string(),
                "-c".into(),
                self.config.working_directory.to_string_lossy().into_owned(),
                format!(
                    "exec '{}'",
                    self.config.shell.to_string_lossy().replace('\'', "'\\''")
                ),
            ])
            .await?;
        if !output.status.success() {
            bail!("tmux could not create the session; refresh the list before retrying");
        }
        // tmux 3.6 can exit during initial configuration if window-size is set
        // before the first window exists. Apply the policy to the created window.
        let configured = self
            .execute(&[
                "set-window-option".into(),
                "-t".into(),
                format!("={}:", Self::name(id)),
                "window-size".into(),
                "manual".into(),
            ])
            .await?;
        if !configured.status.success() {
            bail!("session created but sizing setup failed; refresh the list before retrying");
        }
        Ok(())
    }
    pub async fn set_display_name(&self, id: Uuid, name: &str) -> Result<()> {
        flowsplice_pty_protocol::validate_session_name(name)?;
        let mut encoded = String::with_capacity(name.len() * 2);
        for byte in name.trim().as_bytes() {
            use std::fmt::Write;
            write!(&mut encoded, "{byte:02x}")?;
        }
        self.set_metadata(id, "@flowsplice-name-hex", encoded).await
    }
    pub async fn set_last_connected(&self, id: Uuid, timestamp: u64) -> Result<()> {
        self.set_metadata(id, "@flowsplice-last-connected", timestamp.to_string())
            .await
    }
    async fn set_metadata(&self, id: Uuid, option: &str, value: String) -> Result<()> {
        let output = self
            .execute(&[
                "set-option".into(),
                "-t".into(),
                Self::name(id),
                option.into(),
                value,
            ])
            .await?;
        if !output.status.success() {
            bail!(
                "could not persist tmux session metadata: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
    pub async fn metadata(&self, id: Uuid) -> Result<(String, Option<u64>)> {
        let output = self
            .execute(&[
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                Self::name(id),
                "#{@flowsplice-name-hex}|#{@flowsplice-last-connected}".into(),
            ])
            .await?;
        if !output.status.success() {
            bail!("could not read tmux session metadata");
        }
        let text = String::from_utf8(output.stdout)?;
        let (encoded, last) = text
            .trim_end()
            .split_once('|')
            .context("invalid tmux session metadata")?;
        let name = if encoded.is_empty() {
            Self::fallback_name(id)
        } else {
            if encoded.len() > 512
                || encoded.len() % 2 != 0
                || !encoded.bytes().all(|b| b.is_ascii_hexdigit())
            {
                bail!("invalid tmux session name encoding");
            }
            let bytes = (0..encoded.len())
                .step_by(2)
                .map(|offset| u8::from_str_radix(&encoded[offset..offset + 2], 16))
                .collect::<Result<Vec<_>, _>>()?;
            let name = String::from_utf8(bytes)?;
            flowsplice_pty_protocol::validate_session_name(&name)?;
            name
        };
        Ok((
            name,
            if last.is_empty() {
                None
            } else {
                Some(last.parse()?)
            },
        ))
    }
    pub fn fallback_name(id: Uuid) -> String {
        format!("终端 {}", &id.to_string()[..8])
    }
    pub fn attach_command(&self, id: Uuid) -> CommandBuilder {
        let mut command = CommandBuilder::new(&self.config.binary);
        // All supported terminal clients speak UTF-8, regardless of Home's locale.
        command.arg("-u");
        command.arg("-S");
        command.arg(&self.config.socket);
        command.arg("-f");
        command.arg(&self.config_file);
        command.args(["attach-session", "-E", "-f", "ignore-size", "-t"]);
        command.arg(format!("={}", Self::name(id)));
        command.env_remove("TMUX");
        command.env("TERM", "xterm-256color");
        command.cwd(&self.config.working_directory);
        command
    }
    pub async fn wait_attached(&self, tty: &str) -> Result<()> {
        for _ in 0..100 {
            let output = self
                .execute(&["list-clients".into(), "-F".into(), "#{client_tty}".into()])
                .await?;
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line == tty)
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        bail!("tmux attach did not become ready")
    }
    pub async fn resize(&self, id: Uuid, columns: u16, rows: u16) -> Result<()> {
        let output = self
            .execute(&[
                "resize-window".into(),
                "-t".into(),
                format!("={}:", Self::name(id)),
                "-x".into(),
                columns.to_string(),
                "-y".into(),
                rows.to_string(),
            ])
            .await?;
        if !output.status.success() {
            bail!("tmux session ended during resize");
        }
        Ok(())
    }
    fn name(id: Uuid) -> String {
        format!("fs-{id}")
    }
}
