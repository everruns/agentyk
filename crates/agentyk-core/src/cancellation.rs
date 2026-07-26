//! Cooperative turn cancellation.
//!
//! A [`CancellationToken`] is a cheap, cloneable, thread-safe signal: hold a
//! clone, call [`CancellationToken::cancel`] from anywhere (another task, a
//! signal handler, a UI event), and a running turn stops.
//!
//! There are two ways to observe it, and the difference matters:
//!
//! - [`CancellationToken::is_cancelled`] — a flag check at a **check point**.
//!   Cheap, synchronous, and all an executor needs between steps.
//! - [`CancellationToken::run_until_cancelled`] — race an in-flight future
//!   against the signal and **drop it** if cancellation wins. This is what
//!   makes cancelling a long tool call (a compile, a test run, a network
//!   fetch) land immediately instead of whenever the call happens to finish.
//!   Dropping the future is the cancellation: a well-written tool holds its
//!   child process with `kill_on_drop`, so the process dies with the future.
//!
//! Both are **cooperative** in the sense that nothing is preempted — a future
//! that blocks its thread instead of awaiting cannot be interrupted by
//! anything, here or elsewhere.
//!
//! This is deliberately std-only — no tokio, no futures crate — so it belongs
//! in `agentyk-core` and behaves identically in an in-process executor, a
//! durable host, or a synchronous test harness.
//!
//! ```
//! use agentyk_core::cancellation::CancellationToken;
//!
//! # async fn demo() {
//! let token = CancellationToken::new();
//! let stopper = token.clone();
//! // …from elsewhere: stopper.cancel();
//!
//! match token.run_until_cancelled(async { 41 + 1 }).await {
//!     Some(answer) => assert_eq!(answer, 42),
//!     None => println!("cancelled before it finished"),
//! }
//! # }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct Shared {
    cancelled: AtomicBool,
    /// Wakers of futures currently parked on this token. Slots are reused —
    /// a future takes one on first poll and releases it on drop — so a long
    /// turn cannot grow this vector without bound.
    wakers: Mutex<Vec<Option<Waker>>>,
}

impl Shared {
    /// Take (or refresh) a future's slot so [`CancellationToken::cancel`] can
    /// wake it.
    fn register(&self, slot: &mut Option<usize>, waker: &Waker) {
        let mut wakers = self
            .wakers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match slot {
            Some(index) => wakers[*index] = Some(waker.clone()),
            None => {
                let index = match wakers.iter().position(Option::is_none) {
                    Some(free) => free,
                    None => {
                        wakers.push(None);
                        wakers.len() - 1
                    }
                };
                wakers[index] = Some(waker.clone());
                *slot = Some(index);
            }
        }
    }

    /// Release a slot when its future resolves or is dropped.
    fn unregister(&self, slot: &mut Option<usize>) {
        if let Some(index) = slot.take() {
            let mut wakers = self
                .wakers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            wakers[index] = None;
        }
    }
}

/// A cancellation signal shared between a caller and a running turn.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<Shared>);

impl CancellationToken {
    /// A fresh, uncancelled token. Clone it to hold a handle you can cancel
    /// from elsewhere.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation, waking everything parked on this token.
    /// Idempotent.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        let parked: Vec<Waker> = {
            let mut wakers = self
                .0
                .wakers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            wakers.iter_mut().filter_map(Option::take).collect()
        };
        for waker in parked {
            waker.wake();
        }
    }

    /// Whether [`CancellationToken::cancel`] has been called on this token
    /// or any of its clones.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves as soon as the token is cancelled; pends forever otherwise.
    ///
    /// Use it to build your own race — an approval prompt that should give up
    /// when the turn is cancelled, say. For the common "run this unless
    /// cancelled" shape, prefer [`CancellationToken::run_until_cancelled`].
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            token: self,
            slot: None,
        }
    }

    /// Run `future` to completion unless the token is cancelled first.
    ///
    /// Returns `Some(output)` if it finished, `None` if cancellation won — in
    /// which case `future` is dropped where it stood. The token is checked
    /// before the first poll, so an already-cancelled token never starts the
    /// work at all.
    ///
    /// The future is boxed so this works with `async` blocks (which are not
    /// `Unpin`) without any unsafe pin projection; one allocation per raced
    /// operation is not worth an unsafe block in a crate that forbids them.
    pub fn run_until_cancelled<F: Future>(&self, future: F) -> RunUntilCancelled<'_, F> {
        RunUntilCancelled {
            token: self,
            slot: None,
            future: Box::pin(future),
        }
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Future returned by [`CancellationToken::cancelled`].
#[must_use = "a future does nothing unless awaited"]
pub struct Cancelled<'a> {
    token: &'a CancellationToken,
    slot: Option<usize>,
}

impl Future for Cancelled<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.token.is_cancelled() {
            this.token.0.unregister(&mut this.slot);
            return Poll::Ready(());
        }
        this.token.0.register(&mut this.slot, context.waker());
        // Re-check after registering: `cancel` may have run between the check
        // above and the registration, and would then have found no waker.
        if this.token.is_cancelled() {
            this.token.0.unregister(&mut this.slot);
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        self.token.0.unregister(&mut self.slot);
    }
}

/// Future returned by [`CancellationToken::run_until_cancelled`].
#[must_use = "a future does nothing unless awaited"]
pub struct RunUntilCancelled<'a, F> {
    token: &'a CancellationToken,
    slot: Option<usize>,
    future: Pin<Box<F>>,
}

impl<F: Future> Future for RunUntilCancelled<'_, F> {
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.token.is_cancelled() {
            this.token.0.unregister(&mut this.slot);
            return Poll::Ready(None);
        }
        // Register before polling: if the inner work itself triggers the
        // cancellation, the wake must find a waker to call.
        this.token.0.register(&mut this.slot, context.waker());
        if let Poll::Ready(output) = this.future.as_mut().poll(context) {
            this.token.0.unregister(&mut this.slot);
            return Poll::Ready(Some(output));
        }
        if this.token.is_cancelled() {
            this.token.0.unregister(&mut this.slot);
            return Poll::Ready(None);
        }
        Poll::Pending
    }
}

impl<F> Drop for RunUntilCancelled<'_, F> {
    fn drop(&mut self) {
        self.token.0.unregister(&mut self.slot);
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    use super::*;

    #[test]
    fn starts_uncancelled_and_is_shared_across_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(!token.is_cancelled());
        assert!(!clone.is_cancelled());

        clone.cancel();
        assert!(token.is_cancelled());
        assert!(clone.is_cancelled());
    }

    #[test]
    fn independent_tokens_do_not_share_state() {
        let a = CancellationToken::new();
        let b = CancellationToken::new();
        a.cancel();
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
    }

    #[derive(Default)]
    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll_once<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn a_finished_future_wins_when_nothing_cancels() {
        let token = CancellationToken::new();
        let mut future = pin!(token.run_until_cancelled(async { 42 }));
        assert_eq!(
            poll_once(future.as_mut(), Waker::noop()),
            Poll::Ready(Some(42))
        );
    }

    #[test]
    fn an_already_cancelled_token_never_starts_the_work() {
        let token = CancellationToken::new();
        token.cancel();
        let started = Arc::new(AtomicBool::new(false));
        let flag = started.clone();
        let mut future = pin!(token.run_until_cancelled(async move {
            flag.store(true, Ordering::SeqCst);
        }));
        assert_eq!(poll_once(future.as_mut(), Waker::noop()), Poll::Ready(None));
        assert!(!started.load(Ordering::SeqCst), "work must not have run");
    }

    #[test]
    fn cancelling_a_pending_future_wakes_it_and_resolves_to_none() {
        let token = CancellationToken::new();
        let counter = Arc::new(CountingWaker::default());
        let waker = Waker::from(counter.clone());

        let mut future = pin!(token.run_until_cancelled(std::future::pending::<u8>()));
        assert_eq!(poll_once(future.as_mut(), &waker), Poll::Pending);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);

        token.cancel();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "cancel must wake it");
        assert_eq!(poll_once(future.as_mut(), &waker), Poll::Ready(None));
    }

    #[test]
    fn the_cancelled_future_resolves_on_cancel() {
        let token = CancellationToken::new();
        let counter = Arc::new(CountingWaker::default());
        let waker = Waker::from(counter.clone());

        let mut future = pin!(token.cancelled());
        assert_eq!(poll_once(future.as_mut(), &waker), Poll::Pending);
        token.cancel();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(poll_once(future.as_mut(), &waker), Poll::Ready(()));
    }

    #[test]
    fn waker_slots_are_reused_rather_than_accumulated() {
        let token = CancellationToken::new();
        for _ in 0..100 {
            let mut future = pin!(token.run_until_cancelled(std::future::pending::<u8>()));
            assert_eq!(poll_once(future.as_mut(), Waker::noop()), Poll::Pending);
        }
        let parked = token.0.wakers.lock().unwrap().len();
        assert_eq!(parked, 1, "each dropped future must free its slot");
    }
}
