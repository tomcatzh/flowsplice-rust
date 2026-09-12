//! Nonblocking Unix PTY I/O without the portable writer's drop-time input injection.
use std::{
    io::{self, Read, Write},
    os::fd::{AsRawFd, RawFd},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use filedescriptor::FileDescriptor;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use tokio::io::{Interest, unix::AsyncFd};

struct Descriptor {
    fd: RawFd,
    io: Mutex<FileDescriptor>,
}

impl AsRawFd for Descriptor {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

/// One attached terminal client; dropping it never writes terminal input.
pub struct PtyProcess {
    descriptor: AsyncFd<Descriptor>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    tty_name: Option<String>,
}

/// Spawns the supplied attached client on a nonblocking PTY.
///
/// # Errors
/// Returns allocation, reactor registration or child startup errors.
pub fn spawn(command: CommandBuilder, columns: u16, rows: u16) -> Result<Arc<PtyProcess>> {
    let pair = portable_pty::native_pty_system().openpty(size(columns, rows))?;
    let raw = pair
        .master
        .as_raw_fd()
        .context("PTY master has no Unix descriptor")?;
    let mut fd = FileDescriptor::dup(&raw)?;
    fd.set_non_blocking(true)?;
    let descriptor = AsyncFd::new(Descriptor {
        fd: fd.as_raw_fd(),
        io: Mutex::new(fd),
    })?;
    let tty_name = pair
        .master
        .tty_name()
        .map(|name| name.to_string_lossy().into_owned());
    let child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    Ok(Arc::new(PtyProcess {
        descriptor,
        master: Mutex::new(pair.master),
        child: Mutex::new(Some(child)),
        tty_name,
    }))
}

impl PtyProcess {
    /// Reads terminal output, treating Linux's closed-slave EIO as EOF.
    ///
    /// # Errors
    /// Returns reactor, descriptor or synchronization errors.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.descriptor
            .async_io(Interest::READABLE, |descriptor| {
                let result = descriptor
                    .io
                    .lock()
                    .map_err(|_| io::Error::other("PTY descriptor lock poisoned"))?
                    .read(buf);
                #[cfg(target_os = "linux")]
                if result
                    .as_ref()
                    .is_err_and(|error| error.raw_os_error() == Some(5))
                {
                    return Ok(0);
                }
                result
            })
            .await
    }

    /// Attempts a write without waiting; a full PTY returns `WouldBlock`.
    ///
    /// # Errors
    /// Returns descriptor, synchronization or readiness errors.
    pub fn try_write(&self, bytes: &[u8]) -> io::Result<usize> {
        self.descriptor.try_io(Interest::WRITABLE, |descriptor| {
            descriptor
                .io
                .lock()
                .map_err(|_| io::Error::other("PTY descriptor lock poisoned"))?
                .write(bytes)
        })
    }

    /// Waits for write readiness; `try_write` clears stale readiness on `WouldBlock`.
    ///
    /// # Errors
    /// Returns reactor errors.
    pub async fn writable(&self) -> io::Result<()> {
        let _ready = self.descriptor.writable().await?;
        Ok(())
    }

    /// Resizes the attached client's terminal.
    ///
    /// # Errors
    /// Returns PTY resize or synchronization errors.
    pub fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY master lock poisoned"))?
            .resize(size(columns, rows))
    }

    /// Returns the slave terminal name, when available.
    #[must_use]
    pub fn tty_name(&self) -> Option<String> {
        self.tty_name.clone()
    }

    /// Terminates only the attached child and reaps it, without writing any input.
    ///
    /// The dependency's kill grace period is bounded at 200ms; reaping polls for
    /// another 250ms. An exceptional delayed child is transferred once to a reaper.
    /// Call outside application ownership locks.
    ///
    /// # Errors
    /// Returns signaling, reaping, synchronization or bounded-wait timeout errors.
    pub fn close(&self) -> Result<()> {
        let child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(mut child) = child else {
            return Ok(());
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }
        let killed = child.kill();
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => {
                    std::thread::Builder::new()
                        .name("pty-child-reaper".to_owned())
                        .spawn(move || {
                            let _ = child.wait();
                        })
                        .context("failed to start PTY child reaper")?;
                    killed.context("failed to terminate PTY attached child")?;
                    result.context("failed to poll PTY child exit")?;
                    bail!(
                        "PTY attached child did not exit within bounded wait; background reaper owns child"
                    );
                }
            }
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn size(columns: u16, rows: u16) -> PtySize {
    PtySize {
        cols: columns,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}
