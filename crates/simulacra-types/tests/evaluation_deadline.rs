//! The cutoff of a JavaScript evaluation is published to whichever thread
//! polls it, one poll at a time, so futures interleaved on one thread each see
//! their own cutoff and nothing is left behind.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use simulacra_types::evaluation_deadline::{current, scoped, within};

type Records = Arc<Mutex<Vec<Option<Instant>>>>;

/// Two reads of the cutoff with a yield between, so another future sharing the
/// thread gets polled in the gap.
async fn read_twice_across_a_yield(records: Records) {
    records.lock().unwrap().push(current());
    tokio::task::yield_now().await;
    records.lock().unwrap().push(current());
}

#[test]
fn there_is_no_cutoff_outside_a_scope() {
    assert_eq!(current(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn a_scope_publishes_its_cutoff_to_the_future_it_wraps_and_clears_it_after() {
    let cutoff = Instant::now() + Duration::from_secs(10);

    let seen = scoped(cutoff, async { current() }).await;

    assert_eq!(seen, Some(cutoff));
    assert_eq!(current(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn interleaved_scopes_on_one_thread_each_see_their_own_cutoff() {
    let base = Instant::now();
    let t_a = base + Duration::from_secs(10);
    let t_b = base + Duration::from_secs(20);
    let (a_records, b_records, outside) =
        (Records::default(), Records::default(), Records::default());

    // join! polls a, then b, then the probe, each round. Every poll of one
    // must see its own cutoff, and the probe, which is in no scope, none.
    let a = scoped(t_a, read_twice_across_a_yield(Arc::clone(&a_records)));
    let b = scoped(t_b, read_twice_across_a_yield(Arc::clone(&b_records)));
    let probe = read_twice_across_a_yield(Arc::clone(&outside));
    tokio::join!(a, b, probe);

    assert_eq!(*a_records.lock().unwrap(), [Some(t_a), Some(t_a)]);
    assert_eq!(*b_records.lock().unwrap(), [Some(t_b), Some(t_b)]);
    assert_eq!(*outside.lock().unwrap(), [None, None]);
    assert_eq!(current(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn scopes_that_finish_out_of_order_leave_nothing_behind() {
    let base = Instant::now();
    let t_long = base + Duration::from_secs(10);
    let t_short = base + Duration::from_secs(20);
    let after_short = Records::default();

    // The short one finishes first although the long one started first; a
    // cutoff published once around the whole await would be restored in the
    // wrong order and leak.
    let long = scoped(t_long, async {
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
    });
    let short = scoped(t_short, read_twice_across_a_yield(Arc::clone(&after_short)));
    tokio::join!(long, short);

    assert_eq!(*after_short.lock().unwrap(), [Some(t_short), Some(t_short)]);
    assert_eq!(current(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn a_nested_scope_restores_the_outer_cutoff_when_it_ends() {
    let outer = Instant::now() + Duration::from_secs(10);
    let inner = outer + Duration::from_secs(5);

    let seen = scoped(outer, async {
        let before = current();
        let within = scoped(inner, async { current() }).await;
        (before, within, current())
    })
    .await;

    assert_eq!(seen, (Some(outer), Some(inner), Some(outer)));
    assert_eq!(current(), None);
}

#[test]
fn a_panicking_future_does_not_leave_its_cutoff_behind() {
    let cutoff = Instant::now() + Duration::from_secs(10);
    let mut future = Box::pin(scoped(cutoff, async {
        assert_eq!(current(), Some(cutoff));
        panic!("boom");
    }));
    let mut cx = Context::from_waker(Waker::noop());

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _: Poll<()> = Pin::as_mut(&mut future).poll(&mut cx);
    }));

    assert!(result.is_err(), "the inner panic should propagate");
    assert_eq!(current(), None);
}

#[test]
fn within_publishes_the_cutoff_to_the_closure_and_clears_it_after() {
    let cutoff = Instant::now() + Duration::from_secs(10);

    assert_eq!(current(), None);
    let seen = within(cutoff, current);

    assert_eq!(seen, Some(cutoff));
    assert_eq!(current(), None);
}

#[test]
fn within_returns_the_closures_value() {
    let cutoff = Instant::now() + Duration::from_secs(10);

    let value = within(cutoff, || vec![1, 2, 3]);

    assert_eq!(value, [1, 2, 3]);
}

#[test]
fn a_nested_within_restores_the_outer_cutoff_when_it_ends() {
    let outer = Instant::now() + Duration::from_secs(10);
    let inner = outer + Duration::from_secs(5);

    let seen = within(outer, || {
        let before = current();
        let nested = within(inner, current);
        (before, nested, current())
    });

    assert_eq!(seen, (Some(outer), Some(inner), Some(outer)));
    assert_eq!(current(), None);
}

#[test]
fn a_panic_inside_within_does_not_leave_its_cutoff_behind() {
    let cutoff = Instant::now() + Duration::from_secs(10);

    let result = catch_unwind(|| {
        within(cutoff, || {
            assert_eq!(current(), Some(cutoff));
            panic!("boom");
        })
    });

    assert!(result.is_err(), "the closure's panic should propagate");
    assert_eq!(current(), None);
}
