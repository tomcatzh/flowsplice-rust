//! Runtime-owned work, including tasks waiting on application backpressure.
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use tokio::{
    sync::watch,
    task::{AbortHandle, JoinSet},
};

pub(crate) struct Tasks {
    children: Mutex<JoinSet<()>>,
    shutdown: watch::Sender<bool>,
    drain: tokio::sync::Mutex<()>,
    blocking: Arc<watch::Sender<usize>>,
}

impl Default for Tasks {
    fn default() -> Self {
        Self {
            children: Mutex::new(JoinSet::new()),
            shutdown: watch::channel(false).0,
            drain: tokio::sync::Mutex::new(()),
            blocking: Arc::new(watch::channel(0).0),
        }
    }
}

impl Tasks {
    pub(crate) fn spawn(
        &self,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<AbortHandle> {
        let mut tasks = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *self.shutdown.borrow() {
            bail!("Home runtime is stopped");
        }
        while tasks.try_join_next().is_some() {}
        let mut shutdown = self.shutdown.subscribe();
        Ok(tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {}
                () = future => {}
            }
        }))
    }

    pub(crate) fn stop(&self) {
        // Serialize shutdown with admission so no task can subscribe after the signal.
        let _tasks = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.shutdown.send_replace(true);
    }

    pub(crate) async fn stopped(&self) {
        let mut shutdown = self.shutdown.subscribe();
        while !*shutdown.borrow_and_update() {
            if shutdown.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) async fn finish(&self) {
        self.stop();
        let _drain = self.drain.lock().await;
        while std::future::poll_fn(|cx| {
            self.children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .poll_join_next(cx)
        })
        .await
        .is_some()
        {}
        let mut blocking = self.blocking.subscribe();
        let _ = blocking.wait_for(|count| *count == 0).await;
    }

    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.blocking.send_modify(|count| *count += 1);
        let guard = Blocking(Arc::clone(&self.blocking));
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            work()
        })
        .await
        .context("Home blocking operation failed")?
    }
}

struct Blocking(Arc<watch::Sender<usize>>);

impl Drop for Blocking {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

pub(crate) struct AbortOnDrop(pub(crate) AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::{AbortOnDrop, Tasks};
    use anyhow::{Context, Result};
    use std::{
        future::pending,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        sync::{Barrier, oneshot},
        time::timeout,
    };

    struct Release(Option<oneshot::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(released) = self.0.take() {
                let _ = released.send(());
            }
        }
    }

    #[tokio::test]
    async fn finish_waits_for_blocked_work_resource_release() -> Result<()> {
        let tasks = Tasks::default();
        let (started, ready) = oneshot::channel();
        let (released, mut release) = oneshot::channel();
        tasks.spawn(async move {
            let _resource = Release(Some(released));
            let _ = started.send(());
            pending::<()>().await;
        })?;
        timeout(Duration::from_secs(1), ready).await??;
        timeout(Duration::from_secs(1), tasks.finish()).await?;
        release
            .try_recv()
            .context("finish returned before releasing child resource")?;
        Ok(())
    }

    #[tokio::test]
    async fn stop_rejects_unpolled_work() -> Result<()> {
        let tasks = Tasks::default();
        tasks.stop();
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        assert!(
            tasks
                .spawn(async move {
                    observed.store(true, Ordering::SeqCst);
                })
                .is_err()
        );
        timeout(Duration::from_secs(1), tasks.finish()).await?;
        assert!(!polled.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_finish_calls_drain_without_lost_wakes() -> Result<()> {
        let tasks = Arc::new(Tasks::default());
        let (started, ready) = oneshot::channel();
        let (released, mut release) = oneshot::channel();
        tasks.spawn(async move {
            let _resource = Release(Some(released));
            let _ = started.send(());
            pending::<()>().await;
        })?;
        timeout(Duration::from_secs(1), ready).await??;
        let barrier = Arc::new(Barrier::new(5));
        let mut callers = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let tasks = Arc::clone(&tasks);
            let barrier = Arc::clone(&barrier);
            callers.spawn(async move {
                barrier.wait().await;
                tasks.finish().await;
            });
        }
        timeout(Duration::from_secs(1), async {
            barrier.wait().await;
            while let Some(result) = callers.join_next().await {
                result?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        release
            .try_recv()
            .context("concurrent finish returned before resource release")?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_parent_aborts_its_owned_child() -> Result<()> {
        let tasks = Tasks::default();
        let (started, ready) = oneshot::channel();
        let (released, release) = oneshot::channel();
        tasks.spawn(async move {
            let child = tokio::spawn(async move {
                let _resource = Release(Some(released));
                let _ = started.send(());
                pending::<()>().await;
            });
            let _child = AbortOnDrop(child.abort_handle());
            pending::<()>().await;
        })?;
        timeout(Duration::from_secs(1), ready).await??;
        timeout(Duration::from_secs(1), tasks.finish()).await?;
        timeout(Duration::from_secs(1), release).await??;
        Ok(())
    }

    #[tokio::test]
    async fn repeated_completed_work_does_not_accumulate() -> Result<()> {
        let tasks = Tasks::default();
        for _ in 0..64 {
            let handle = tasks.spawn(async {})?;
            timeout(Duration::from_secs(1), async {
                while !handle.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        }
        let (started, ready) = oneshot::channel();
        tasks.spawn(async move {
            let _ = started.send(());
            pending::<()>().await;
        })?;
        timeout(Duration::from_secs(1), ready).await??;
        let retained = tasks
            .children
            .lock()
            .map_err(|_| anyhow::anyhow!("task registry poisoned"))?
            .len();
        assert_eq!(
            retained, 1,
            "completed work accumulated alongside the active task"
        );
        timeout(Duration::from_secs(1), tasks.finish()).await?;
        Ok(())
    }
    #[tokio::test]
    async fn cancelled_blocking_caller_keeps_finish_pending_until_worker_releases() -> Result<()> {
        let tasks = Arc::new(Tasks::default());
        let worker_tasks = Arc::clone(&tasks);
        let (started, ready) = oneshot::channel();
        let (released, mut release) = oneshot::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            worker_tasks
                .blocking(move || {
                    let _resource = Release(Some(released));
                    let _ = started.send(());
                    wait.recv().context("worker release channel closed")?;
                    Ok(())
                })
                .await
        });
        timeout(Duration::from_secs(1), ready).await??;
        caller.abort();
        let outcome = timeout(Duration::from_secs(1), caller).await?;
        assert!(outcome.is_err_and(|error| error.is_cancelled()));
        let mut finishing = Box::pin(tasks.finish());
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(finishing.as_mut(), cx).is_pending(),
                "finish ignored outstanding blocking work"
            );
            std::task::Poll::Ready(())
        })
        .await;
        assert!(matches!(
            release.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        resume.send(())?;
        timeout(Duration::from_secs(1), finishing).await?;
        release
            .try_recv()
            .context("finish returned before blocking resource release")?;
        Ok(())
    }

    #[tokio::test]
    async fn blocking_error_and_panic_both_release_drain_obligation() -> Result<()> {
        let tasks = Tasks::default();
        let error = tasks
            .blocking(|| Err::<(), _>(anyhow::anyhow!("worker sentinel")))
            .await;
        assert_eq!(
            error.err().context("worker error was lost")?.to_string(),
            "worker sentinel"
        );
        let failure = tasks
            .blocking(|| -> Result<()> {
                std::panic::resume_unwind(Box::new("worker panic sentinel"))
            })
            .await;
        assert!(failure.is_err());
        timeout(Duration::from_secs(1), tasks.finish()).await?;
        Ok(())
    }
}
