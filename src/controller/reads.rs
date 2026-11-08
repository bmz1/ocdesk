//! Read-only request ownership: the reply receiver owns demand for the result.
//! Dropping it cancels the async future at its next poll boundary. This helper
//! must not run writes, prompt admissions, approvals or sidecar startup: dropping
//! a future cannot roll back a mutation already accepted by the server.

use std::future::Future;

use anyhow::Result;
use futures::channel::oneshot;

pub(super) fn spawn_read<T, F>(
    runtime: &tokio::runtime::Handle,
    operation: &'static str,
    read: F,
) -> oneshot::Receiver<Result<T>>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let (mut tx, rx) = oneshot::channel();
    runtime.spawn(async move {
        tokio::select! {
            // If the receiver is already gone, never start obsolete I/O.
            biased;
            _ = tx.cancellation() => {
                tracing::debug!(operation, "read cancelled: reply receiver dropped");
            }
            result = read => { let _ = tx.send(result); }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dropped(Option<oneshot::Sender<()>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test]
    async fn dropping_receiver_drops_in_flight_read_and_releases_its_resources() {
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let rx = spawn_read(&tokio::runtime::Handle::current(), "test", async move {
            let _guard = Dropped(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<Result<()>>().await
        });
        started_rx.await.unwrap();
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn dropped_receiver_prevents_read_from_starting() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let guard = Dropped(Some(dropped_tx));
        // No await between creating and dropping the receiver on this runtime.
        let rx = spawn_read(&tokio::runtime::Handle::current(), "test", async move {
            let _guard = guard;
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retained_receiver_gets_success_or_actionable_failure() {
        let runtime = tokio::runtime::Handle::current();
        assert_eq!(
            spawn_read(&runtime, "test", async { Ok(42) })
                .await
                .unwrap()
                .unwrap(),
            42
        );
        let error =
            spawn_read::<(), _>(&runtime, "test", async { Err(anyhow::anyhow!("offline")) })
                .await
                .unwrap()
                .unwrap_err();
        assert_eq!(error.to_string(), "offline");
    }
}
