//! Keep authorization cancellation outside every application/network I/O await.
use std::future::Future;

use anyhow::{Result, bail};
use flowsplice_core::authorization::unix_time_secs;

pub(crate) async fn guard<T>(
    operation: impl Future<Output = Result<T>>,
    authorization_ended: impl Future<Output = Result<()>>,
    expires_at: u64,
) -> Result<T> {
    if unix_time_secs()? >= expires_at {
        bail!("Travel credential expired");
    }
    tokio::select! {
        biased;
        result = authorization_ended => {
            result?;
            bail!("Travel authorization ended");
        }
        () = super::sleep_until_unix(expires_at) => {
            bail!("Travel credential expired");
        }
        result = operation => result,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Context, Result};
    use tokio::{
        sync::{mpsc, oneshot},
        time::timeout,
    };

    use super::guard;
    use flowsplice_core::authorization::unix_time_secs;

    struct Cleanup(Arc<AtomicBool>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    async fn blocked_io(
        started: oneshot::Sender<()>,
        resume: oneshot::Receiver<()>,
        output: mpsc::UnboundedSender<&'static str>,
        cleaned: Arc<AtomicBool>,
    ) -> Result<()> {
        let _cleanup = Cleanup(cleaned);
        started
            .send(())
            .map_err(|()| anyhow::anyhow!("start observer closed"))?;
        resume.await.context("application I/O closed")?;
        output
            .send("application output")
            .context("output observer closed")?;
        Ok(())
    }

    #[tokio::test]
    async fn authorization_end_drops_blocked_io_without_late_output() -> Result<()> {
        let (started_tx, started_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let (ended_tx, ended_rx) = oneshot::channel();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let cleaned = Arc::new(AtomicBool::new(false));
        let operation = blocked_io(started_tx, resume_rx, output_tx, Arc::clone(&cleaned));
        let task = tokio::spawn(guard(
            operation,
            async { ended_rx.await.context("authorization notifier closed") },
            unix_time_secs()? + 60,
        ));
        timeout(Duration::from_secs(1), started_rx).await??;
        assert!(!cleaned.load(Ordering::SeqCst));
        ended_tx
            .send(())
            .map_err(|()| anyhow::anyhow!("guard already ended"))?;
        let result = timeout(Duration::from_secs(1), task).await??;
        assert!(
            result
                .err()
                .context("expected guarded operation failure")?
                .to_string()
                .contains("authorization ended")
        );
        assert!(cleaned.load(Ordering::SeqCst));
        assert!(
            resume_tx.send(()).is_err(),
            "blocked I/O receiver survived cancellation"
        );
        assert_eq!(output_rx.recv().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn expired_credential_does_not_poll_application_io() -> Result<()> {
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let result = guard(
            async move {
                observed.store(true, Ordering::SeqCst);
                pending::<Result<()>>().await
            },
            pending::<Result<()>>(),
            unix_time_secs()?,
        )
        .await;
        assert!(
            result
                .err()
                .context("expected guarded operation failure")?
                .to_string()
                .contains("expired")
        );
        assert!(!polled.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn expiry_cancels_started_io_without_provider_progress() -> Result<()> {
        let (started_tx, started_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let cleaned = Arc::new(AtomicBool::new(false));
        let operation = blocked_io(started_tx, resume_rx, output_tx, Arc::clone(&cleaned));
        let task = tokio::spawn(guard(
            operation,
            pending::<Result<()>>(),
            unix_time_secs()? + 1,
        ));
        timeout(Duration::from_secs(1), started_rx).await??;
        assert!(!cleaned.load(Ordering::SeqCst));
        let result = timeout(Duration::from_secs(4), task).await??;
        assert!(
            result
                .err()
                .context("expected guarded operation failure")?
                .to_string()
                .contains("expired")
        );
        assert!(cleaned.load(Ordering::SeqCst));
        assert!(resume_tx.send(()).is_err());
        assert_eq!(output_rx.recv().await, None);
        Ok(())
    }

    #[tokio::test]
    async fn completed_io_preserves_success_and_original_error() -> Result<()> {
        let expires = unix_time_secs()? + 60;
        let value = guard(
            async { Ok(vec![0_u8, 255, 17]) },
            pending::<Result<()>>(),
            expires,
        )
        .await?;
        assert_eq!(value, vec![0, 255, 17]);
        let error = guard(
            async {
                Err::<(), _>(
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "provider sentinel").into(),
                )
            },
            pending::<Result<()>>(),
            expires,
        )
        .await
        .err()
        .context("expected guarded operation failure")?;
        let original = error
            .downcast_ref::<std::io::Error>()
            .context("provider error type was replaced")?;
        assert_eq!(original.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(original.to_string(), "provider sentinel");
        Ok(())
    }
}
