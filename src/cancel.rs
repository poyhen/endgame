use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A lightweight, clonable token for cooperatively cancelling asynchronous work.
///
/// Cancellation is permanent: after any clone calls [`cancel`](Self::cancel),
/// every current and future waiter observes the cancelled state.
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the token as cancelled and wakes all current waiters.
    ///
    /// Calling this more than once has no additional effect.
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Waits until this token is cancelled.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }

        let notified = self.inner.notify.notified();
        tokio::pin!(notified);

        // `notify_waiters` does not retain a permit for future waiters. Register
        // before checking the flag again so cancellation cannot occur between
        // the final check and the first poll of `notified` and be missed.
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }

        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use tokio::sync::Barrier;
    use tokio::time::{Duration, timeout};

    use super::*;

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn cancel_is_shared_and_idempotent() {
        let token = CancellationToken::new();
        let clone = token.clone();

        assert!(!token.is_cancelled());
        assert!(!clone.is_cancelled());

        clone.cancel();
        token.cancel();

        assert!(token.is_cancelled());
        assert!(clone.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_returns_when_cancelled_before_waiting() {
        let token = CancellationToken::new();
        token.cancel();

        timeout(Duration::from_secs(1), token.cancelled())
            .await
            .expect("a future waiter must observe permanent cancellation");
    }

    #[tokio::test]
    async fn cancellation_wakes_every_waiter() {
        const WAITERS: usize = 8;

        let token = CancellationToken::new();
        let barrier = Arc::new(Barrier::new(WAITERS + 1));
        let mut tasks = Vec::with_capacity(WAITERS);

        for _ in 0..WAITERS {
            let token = token.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                token.cancelled().await;
            }));
        }

        barrier.wait().await;
        token.cancel();

        timeout(Duration::from_secs(1), async {
            for task in tasks {
                task.await.expect("waiter should not panic");
            }
        })
        .await
        .expect("all waiters must be woken");
    }

    #[test]
    fn waiter_completes_after_being_registered_then_cancelled() {
        let token = CancellationToken::new();
        let mut future = Box::pin(token.cancelled());
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);

        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));

        token.cancel();

        assert!(matches!(
            future.as_mut().poll(&mut context),
            Poll::Ready(())
        ));
    }
}
