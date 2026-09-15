use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, timeout_at},
};

type Entry<T> = (Arc<()>, oneshot::Sender<T>);

pub(crate) struct PendingRequests<K, T> {
    entries: Mutex<HashMap<K, Entry<T>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestError {
    Duplicate,
    QueueClosed,
    ResponseClosed,
    TimedOut,
}

impl<K: Eq + Hash + Clone, T> PendingRequests<K, T> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn register(
        &self,
        key: K,
    ) -> Result<(Registration<'_, K, T>, oneshot::Receiver<T>), RequestError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.contains_key(&key) {
            return Err(RequestError::Duplicate);
        }
        let token = Arc::new(());
        let (sender, receiver) = oneshot::channel();
        entries.insert(key.clone(), (Arc::clone(&token), sender));
        Ok((
            Registration {
                requests: self,
                key,
                token,
            },
            receiver,
        ))
    }

    pub(crate) fn complete(&self, key: &K, value: T) -> bool {
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        if let Some((_, sender)) = entry {
            let _ = sender.send(value);
            true
        } else {
            false
        }
    }

    pub(crate) async fn request<M>(
        &self,
        key: K,
        sender: &mpsc::Sender<M>,
        message: M,
        deadline: Instant,
    ) -> Result<T, RequestError> {
        if Instant::now() >= deadline {
            return Err(RequestError::TimedOut);
        }
        let (_registration, receiver) = self.register(key)?;
        timeout_at(deadline, async {
            sender
                .send(message)
                .await
                .map_err(|_| RequestError::QueueClosed)?;
            receiver.await.map_err(|_| RequestError::ResponseClosed)
        })
        .await
        .map_err(|_| RequestError::TimedOut)?
    }
}

struct Registration<'a, K: Eq + Hash, T> {
    requests: &'a PendingRequests<K, T>,
    key: K,
    token: Arc<()>,
}
impl<K: Eq + Hash, T> Drop for Registration<'_, K, T> {
    fn drop(&mut self) {
        let mut entries = self
            .requests
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries
            .get(&self.key)
            .is_some_and(|(token, _)| Arc::ptr_eq(token, &self.token))
        {
            entries.remove(&self.key);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_does_not_register_or_enqueue() {
        let requests = PendingRequests::<u8, ()>::new();
        let (sender, mut queued) = mpsc::channel(1);
        let deadline = Instant::now();
        assert_eq!(
            requests.request(1, &sender, (), deadline).await,
            Err(RequestError::TimedOut)
        );
        assert!(requests.entries.lock().unwrap().is_empty());
        assert!(matches!(
            queued.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn queue_and_response_share_one_deadline() {
        let requests = PendingRequests::<u8, ()>::new();
        let (sender, mut queued) = mpsc::channel(1);
        sender.send(()).await.unwrap();
        let start = Instant::now();
        let request = requests.request(1, &sender, (), start + Duration::from_secs(10));
        tokio::pin!(request);
        tokio::select! { result = &mut request => panic!("unexpected {result:?}"), () = tokio::time::sleep(Duration::from_secs(6)) => {} }
        queued.recv().await.unwrap();
        assert_eq!(request.await, Err(RequestError::TimedOut));
        assert_eq!(Instant::now() - start, Duration::from_secs(10));
        assert!(requests.entries.lock().unwrap().is_empty());
        assert!(!requests.complete(&1, ()));
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_queue_times_out_and_cancellation_removes_immediately() {
        let requests = PendingRequests::<u8, ()>::new();
        let (sender, _queued) = mpsc::channel(1);
        sender.send(()).await.unwrap();
        {
            let request =
                requests.request(1, &sender, (), Instant::now() + Duration::from_secs(10));
            tokio::pin!(request);
            tokio::select! { result = &mut request => panic!("unexpected {result:?}"), () = tokio::task::yield_now() => {} }
            assert_eq!(requests.entries.lock().unwrap().len(), 1);
        }
        assert!(requests.entries.lock().unwrap().is_empty());
        assert_eq!(
            requests
                .request(1, &sender, (), Instant::now() + Duration::from_secs(10))
                .await,
            Err(RequestError::TimedOut)
        );
        assert!(requests.entries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_preserves_waiter_and_old_guard_preserves_replacement() {
        let requests = PendingRequests::new();
        let (old_guard, receiver) = requests.register(1).unwrap();
        assert!(matches!(requests.register(1), Err(RequestError::Duplicate)));
        assert!(requests.complete(&1, 7));
        assert_eq!(receiver.await.unwrap(), 7);
        let (_replacement, receiver) = requests.register(1).unwrap();
        drop(old_guard);
        assert!(requests.complete(&1, 8));
        assert_eq!(receiver.await.unwrap(), 8);
    }

    #[tokio::test]
    async fn closed_queue_and_response_cleanup() {
        let requests = PendingRequests::<u8, ()>::new();
        let (sender, queued) = mpsc::channel(1);
        drop(queued);
        assert_eq!(
            requests
                .request(1, &sender, (), Instant::now() + Duration::from_secs(10))
                .await,
            Err(RequestError::QueueClosed)
        );
        assert!(requests.entries.lock().unwrap().is_empty());
        let (sender, mut queued) = mpsc::channel(1);
        let request = requests.request(1, &sender, (), Instant::now() + Duration::from_secs(10));
        let close = async {
            queued.recv().await.unwrap();
            requests.entries.lock().unwrap().remove(&1);
        };
        let (result, ()) = tokio::join!(request, close);
        assert_eq!(result, Err(RequestError::ResponseClosed));
        assert!(requests.entries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_reply_removes_immediately() {
        let requests = PendingRequests::<u8, ()>::new();
        let (sender, mut queued) = mpsc::channel(1);
        {
            let request =
                requests.request(1, &sender, (), Instant::now() + Duration::from_secs(10));
            tokio::pin!(request);
            tokio::select! { result = &mut request => panic!("unexpected {result:?}"), _ = queued.recv() => {} }
            assert_eq!(requests.entries.lock().unwrap().len(), 1);
        }
        assert!(requests.entries.lock().unwrap().is_empty());
        assert!(!requests.complete(&1, ()));
    }
}
