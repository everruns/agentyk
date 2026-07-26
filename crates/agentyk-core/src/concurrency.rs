//! Running several futures at once, without a runtime.
//!
//! An executor that dispatches a prepared tool batch needs exactly one thing
//! a runtime would give it: poll N futures until all of them finish. It does
//! **not** need spawning, timers, or work stealing — each future is already
//! driven by whatever runtime the host is on, and the batch lives entirely
//! inside one task.
//!
//! So [`join_all`] is here rather than a `futures` dependency in
//! `agentyk-core`. Concurrency is a property of *how a host executes*, which
//! is a contract concern; taking a dependency to express it would make the
//! leanest crate in the workspace carry a combinator library for one
//! function.
//!
//! ```
//! use agentyk_core::concurrency::join_all;
//!
//! # async fn demo() {
//! // One future per item — here two reads that should not wait for each other.
//! let reads = ["a.txt", "b.txt"].map(|path| async move { path.len() });
//! assert_eq!(join_all(reads).await, vec![5, 5]);
//! # }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Run every future concurrently and collect their outputs **in input
/// order**, regardless of the order they finish in.
///
/// Order matters more than it looks: a host that records one event per result
/// must produce the same sequence every run, or two replays of the same
/// session disagree. Concurrency is about wall-clock, not about letting the
/// fastest tool jump the queue.
///
/// Futures are boxed, so `async` blocks work directly and no unsafe pin
/// projection is needed — the workspace forbids unsafe, and one allocation
/// per tool call is not a cost worth an exception.
pub fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> JoinAll<F> {
    JoinAll {
        pending: futures.into_iter().map(|f| Some(Box::pin(f))).collect(),
        outputs: Vec::new(),
    }
}

/// Future returned by [`join_all`].
#[must_use = "a future does nothing unless awaited"]
pub struct JoinAll<F: Future> {
    /// `None` once the future at that index has produced its output.
    pending: Vec<Option<Pin<Box<F>>>>,
    /// Filled in index order as futures resolve.
    outputs: Vec<Option<F::Output>>,
}

/// Sound because nothing in `JoinAll` is structurally pinned: the futures
/// live behind `Pin<Box<_>>` (already `Unpin`) and the outputs are plain
/// values this type only ever moves out at the end. Without this, the
/// combinator would demand `Unpin` outputs, which almost nothing satisfies.
impl<F: Future> Unpin for JoinAll<F> {}

impl<F: Future> Future for JoinAll<F> {
    type Output = Vec<F::Output>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.outputs.is_empty() {
            this.outputs = (0..this.pending.len()).map(|_| None).collect();
        }

        let mut all_done = true;
        for (index, slot) in this.pending.iter_mut().enumerate() {
            let Some(future) = slot else {
                continue;
            };
            match future.as_mut().poll(context) {
                Poll::Ready(output) => {
                    this.outputs[index] = Some(output);
                    // Dropped here, not at the end: a finished tool's future
                    // should release whatever it holds (a child process
                    // handle, a connection) as soon as it is done, not when
                    // the slowest sibling catches up.
                    *slot = None;
                }
                Poll::Pending => all_done = false,
            }
        }

        match all_done {
            true => Poll::Ready(
                std::mem::take(&mut this.outputs)
                    .into_iter()
                    .map(|output| output.expect("every future resolved"))
                    .collect(),
            ),
            false => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::{Arc, Mutex};
    use std::task::Waker;

    use super::*;

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn outputs_come_back_in_input_order() {
        let mut future = pin!(join_all([
            std::future::ready("a"),
            std::future::ready("b"),
            std::future::ready("c"),
        ]));
        assert_eq!(poll_once(future.as_mut()), Poll::Ready(vec!["a", "b", "c"]));
    }

    #[test]
    fn an_empty_batch_is_immediately_ready() {
        let mut future = pin!(join_all(Vec::<std::future::Ready<u8>>::new()));
        assert_eq!(poll_once(future.as_mut()), Poll::Ready(Vec::new()));
    }

    /// Futures that record when they were *started*, to prove they overlap
    /// rather than running one after another.
    #[test]
    fn every_future_makes_progress_before_any_of_them_finishes() {
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let make = |name: &'static str, log: Arc<Mutex<Vec<&'static str>>>| {
            let mut polls = 0;
            std::future::poll_fn(move |_context| {
                polls += 1;
                if polls == 1 {
                    log.lock().unwrap().push(name);
                    return Poll::Pending;
                }
                Poll::Ready(name)
            })
        };

        let mut future = pin!(join_all(vec![
            make("first", log.clone()),
            make("second", log.clone()),
        ]));

        // One poll of the batch touches both futures — sequential execution
        // would have left "second" unstarted.
        assert_eq!(poll_once(future.as_mut()), Poll::Pending);
        assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);

        assert_eq!(
            poll_once(future.as_mut()),
            Poll::Ready(vec!["first", "second"])
        );
    }

    #[test]
    fn a_finished_future_is_dropped_before_its_siblings_complete() {
        let dropped = Arc::new(Mutex::new(false));

        struct Flag(Arc<Mutex<bool>>);
        impl Drop for Flag {
            fn drop(&mut self) {
                *self.0.lock().unwrap() = true;
            }
        }

        let flag = Flag(dropped.clone());
        let quick = async move {
            let _flag = flag;
            1u8
        };
        let slow = std::future::poll_fn(|_context| Poll::<u8>::Pending);

        let mut future = pin!(join_all(vec![
            Box::pin(quick) as Pin<Box<dyn Future<Output = u8>>>,
            Box::pin(slow),
        ]));
        assert_eq!(poll_once(future.as_mut()), Poll::Pending);
        assert!(
            *dropped.lock().unwrap(),
            "the resolved future must be released, not held until the batch ends"
        );
    }
}
