#![cfg(ps_loom)]
//! The small reader/reclaimer handshake model inherited from PR #7.
//! Exploration is limited by Loom's model and any configured bounds. Changes
//! in the PR #9 follow-up are source-only, not executed validation.
//!
//! This models the protocol, not the crate. Retrofitting loom onto ps-reclaim
//! means routing every atomic through one module and then dealing with `static`
//! atomics, which loom cannot build in a `const` and cannot reset between
//! iterations. The question worth answering needs none of that: are the two
//! fences, one in `pin` and one in `advance`, enough to stop a reclaimer
//! freeing an object a reader can still reach?
//!
//! That is exactly what a weak memory model attacks, which is why it matters on
//! aarch64 and hides on x86.
use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering, fence};
use loom::thread;

const NO_DOMAIN: usize = usize::MAX;

struct Domain {
    epoch: AtomicUsize,
    pin: AtomicUsize,
    published: AtomicUsize,
    object: AtomicUsize,
}

impl Domain {
    fn new() -> Self {
        Domain {
            epoch: AtomicUsize::new(1),
            pin: AtomicUsize::new(NO_DOMAIN),
            published: AtomicUsize::new(1),
            object: AtomicUsize::new(1),
        }
    }
}

/// Both fences are parameters so the model can be shown to fail without them.
/// A check that cannot fail is worse than none.
fn run(pin_fence: bool, scan_fence: bool) {
    let d = Arc::new(Domain::new());

    let reader = {
        let d = Arc::clone(&d);
        thread::spawn(move || {
            let e = d.epoch.load(Ordering::Relaxed);
            d.pin.store(e, Ordering::Release);
            if pin_fence {
                fence(Ordering::SeqCst);
            }
            if d.published.load(Ordering::Acquire) == 1 {
                assert_eq!(
                    d.object.load(Ordering::Relaxed),
                    1,
                    "read an object that had already been reclaimed"
                );
            }
            d.pin.store(NO_DOMAIN, Ordering::Release);
        })
    };

    let writer = {
        let d = Arc::clone(&d);
        thread::spawn(move || {
            d.published.store(0, Ordering::Release);
            let retired_at = d.epoch.load(Ordering::Relaxed);
            if scan_fence {
                fence(Ordering::SeqCst);
            }
            let pinned = d.pin.load(Ordering::Acquire);
            if pinned == NO_DOMAIN || pinned > retired_at {
                d.object.store(0, Ordering::Relaxed);
            }
        })
    };

    reader.join().unwrap();
    writer.join().unwrap();
}

#[test]
fn both_fences_present_is_safe() {
    loom::model(|| run(true, true));
}

#[test]
#[should_panic(expected = "already been reclaimed")]
fn without_the_pin_fence_a_live_object_is_reclaimed() {
    loom::model(|| run(false, true));
}

#[test]
#[should_panic(expected = "already been reclaimed")]
fn without_the_scan_fence_a_live_object_is_reclaimed() {
    loom::model(|| run(true, false));
}
