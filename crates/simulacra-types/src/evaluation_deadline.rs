//! The instant the JavaScript evaluation running on this thread is cut off.
//! The engine cannot interrupt a host call in flight, so a slow host (a
//! filesystem upload or commit) reads this and stops starting work the
//! engine would already have interrupted.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

thread_local! {
    static CUTOFF: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The cutoff of the evaluation on this thread, if one is running.
pub fn current() -> Option<Instant> {
    CUTOFF.with(Cell::get)
}

/// Runs `future` with `cutoff` published on whichever thread polls it, for
/// the duration of each poll only. Publishing once around an await would let
/// another future interleaved on the same thread see this cutoff, or leave it
/// behind when the two finish out of order.
pub fn scoped<F: Future>(cutoff: Instant, future: F) -> Scoped<F> {
    Scoped { cutoff, future }
}

/// Runs `f` with `cutoff` published on this thread, for work that belongs to
/// an evaluation but runs on a thread of its own, such as module prefetch.
pub fn within<R>(cutoff: Instant, f: impl FnOnce() -> R) -> R {
    let previous = CUTOFF.with(|c| c.replace(Some(cutoff)));
    let _restore = Restore(previous);
    f()
}

pin_project_lite::pin_project! {
    pub struct Scoped<F> {
        cutoff: Instant,
        #[pin]
        future: F,
    }
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        let previous = CUTOFF.with(|c| c.replace(Some(*this.cutoff)));
        let _restore = Restore(previous);
        this.future.poll(cx)
    }
}

/// Restores the previous cutoff even if the inner poll panics.
struct Restore(Option<Instant>);

impl Drop for Restore {
    fn drop(&mut self) {
        CUTOFF.with(|c| c.set(self.0));
    }
}
