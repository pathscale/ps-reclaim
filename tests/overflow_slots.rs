//! Exhausting the participant registry.
//!
//! Its own test binary, and therefore its own process, deliberately. The
//! registry is a process-global `OnceLock` whose high-water mark never falls,
//! so a test that fills every slot permanently changes what any later test in
//! the same binary observes. Run beside `concurrency.rs` it made
//! `short_lived_threads_return_their_slots` fail, which was the test being
//! contaminated rather than the code being wrong.

/// More simultaneously pinned threads than there are slots.
///
/// Past `MAX_THREADS` every further thread used to share slot 255 and write
/// its pin into the same `pins[0]`. The documentation called that "sound but
/// contended", on the reasoning that a shared pin only delays reclamation.
/// That is false: a pin entry is a single value, not a count. When one
/// overflow thread dropped its guard it stored `NO_DOMAIN` over a pin another
/// overflow thread still held, the reclaimer read the slot as idle, and the
/// second thread's data could be freed underneath it.
///
/// The shape matters. Threads that merely *exist* past the limit prove
/// nothing, and neither do sequential short-lived ones that recycle the slot.
/// What is needed is one overflow thread releasing while another overflow
/// thread is still pinned, with a reclaimer running in between.
#[test]
fn an_overflow_thread_releasing_does_not_unpin_another_one() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    let domain = Arc::new(ps_reclaim::Domain::new());
    // Fill every private slot, so the two threads below are forced to overflow.
    // They have to stay alive: a slot returns to the free list on thread exit.
    let fill = ps_reclaim::MAX_THREADS - 1;
    let all_seated = Arc::new(Barrier::new(fill + 1));
    let done = Arc::new(Barrier::new(fill + 1));
    let mut fillers = Vec::with_capacity(fill);
    for _ in 0..fill {
        let (domain, all_seated, done) = (domain.clone(), all_seated.clone(), done.clone());
        fillers.push(std::thread::spawn(move || {
            let g = domain.pin();
            drop(g); // claims the slot, then leaves it idle so it cannot mask the result
            all_seated.wait();
            done.wait();
        }));
    }
    all_seated.wait();

    let reclaimed = Arc::new(AtomicBool::new(false));

    // B overflows and stays pinned for the whole window.
    let b_pinned = Arc::new(Barrier::new(3));
    let b_may_go = Arc::new(Barrier::new(2));
    let b = {
        let (domain, b_pinned, b_may_go) = (domain.clone(), b_pinned.clone(), b_may_go.clone());
        std::thread::spawn(move || {
            let _guard = domain.pin();
            b_pinned.wait();
            b_may_go.wait();
        })
    };

    // A overflows onto the same slot, then releases while B is still pinned.
    let a = {
        let (domain, b_pinned) = (domain.clone(), b_pinned.clone());
        std::thread::spawn(move || {
            let guard = domain.pin();
            b_pinned.wait();
            drop(guard);
        })
    };

    b_pinned.wait();
    a.join().expect("A");

    // B is still pinned. Nothing retired now may run.
    let flag = reclaimed.clone();
    domain.retire(move || flag.store(true, Ordering::Release));
    for _ in 0..8 {
        domain.advance();
    }

    assert!(
        !reclaimed.load(Ordering::Acquire),
        "a retirement ran while an overflow thread was still pinned: its slot was cleared by another \
         thread sharing it, so the reclaimer saw no pin"
    );

    b_may_go.wait();
    b.join().expect("B");
    done.wait();
    for f in fillers {
        f.join().expect("filler");
    }
}
