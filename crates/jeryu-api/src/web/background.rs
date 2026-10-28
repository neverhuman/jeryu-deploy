//! Work a request starts and deliberately does not wait for.
//!
//! `POST /api/v1/auth/password/forgot` must answer before it looks an address
//! up, issues a link or hands a message to the host's mailer. Whether an
//! account holds the address decides how much of that work happens, so a
//! caller that waits with a stopwatch would learn from the response time what
//! the body is written never to say. The handler spawns the sending and
//! answers `202` at once (`account_email::forgot_password`).
//!
//! Detaching the work leaves nothing to wait for, which a test needs: it has
//! to read the message the request caused. So the spawn is counted, and
//! [`Background::settled`] waits for the count to fall back to zero. Nothing
//! in production reads it; the counter is two words whether or not anybody
//! waits.
//!
//! What grows here is bounded by the route that spawns: the forgot-password
//! caps are five requests an hour per client address and thirty a minute for
//! the whole route (`docs/account-email.md`), so the live count stays in the
//! tens. A panicking task still releases its count.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub(crate) struct Background {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    running: AtomicUsize,
    idle: Notify,
}

impl Background {
    /// Run `work` without waiting for it, and count it while it runs.
    pub(crate) fn spawn<F>(&self, work: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.inner.running.fetch_add(1, Ordering::SeqCst);
        let running = Running {
            inner: self.inner.clone(),
        };
        tokio::spawn(async move {
            // Held across the await so a panic inside `work` unwinds through
            // the guard and the count is released either way.
            let _running = running;
            work.await;
        });
    }

    /// Wait until no spawned work is left running.
    ///
    /// Test-only: a request answers before its work finishes, and a test that
    /// reads what the work did has to wait for it first.
    #[cfg(test)]
    pub(crate) async fn settled(&self) {
        loop {
            // Registered before the count is read, so work that finishes
            // between the two still wakes this waiter.
            let waiting = self.inner.idle.notified();
            if self.inner.running.load(Ordering::SeqCst) == 0 {
                return;
            }
            waiting.await;
        }
    }
}

/// One running task's share of the count, released on drop.
struct Running {
    inner: Arc<Inner>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.inner.running.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Background;

    #[tokio::test]
    async fn settled_waits_for_spawned_work_and_returns_when_none_is_left() {
        let background = Background::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        background.spawn(async move {
            rx.await.expect("the test releases the work");
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "the spawn must not have waited for the work"
        );
        tx.send(()).expect("release the work");
        background.settled().await;
        assert!(
            done.load(std::sync::atomic::Ordering::SeqCst),
            "settled must wait for the work to finish"
        );
        // Nothing is left, so a second wait returns at once.
        background.settled().await;
    }

    #[tokio::test]
    async fn a_panicking_task_releases_its_count() {
        let background = Background::default();
        background.spawn(async { panic!("the work failed") });
        background.settled().await;
    }
}
