//! Properties that only fail under concurrency, and the one the bounded
//! registry scan puts at risk.
//!
//! `tests/contract.rs` states the single-threaded contract. This file is the
//! other half: a domain is only useful if it is still correct when readers and
//! reclaimers run at once, and none of that is observable from one thread.
//!
//! Every test here was checked to fail against a deliberately broken domain
//! before being trusted. Where the break is easy to describe, the test says
//! what it was, because a concurrency test whose failure mode nobody has seen
//! is decoration.
//!
//! Run these under Miri as well as natively. A racy pointer-sized load is
//! atomic in practice on aarch64, so a native pass proves much less here than
//! it looks: `MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri test`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use ps_reclaim::Domain;

/// A slot acquired *after* a scan begins is still protected.
///
/// This is the property the bounded scan puts at risk and the reason it is
/// worth stating on its own. `advance` looks only at slots that have ever been
/// leased, bounded by the registry's high-water mark. A thread that has not yet
/// taken a lease therefore sits outside the scan.
///
/// That is sound because taking a lease raises the mark *before* the thread can
/// publish a pin, and `pin` publishes behind a `SeqCst` fence that `advance`
/// pairs with. So a pin this scan cannot see is a pin that did not exist when
/// the scan's fence ran, and a retirement made before that fence is not
/// reachable from it.
///
/// The test drives the race directly: a fresh thread (fresh slot, raised mark)
/// starts pinning while another thread advances in a loop, and the value the
/// reader holds must never be reclaimed while its guard is alive.
#[test]
fn a_slot_leased_during_a_scan_is_still_protected() {
    const SENTINEL: usize = 0x5AFE_5AFE;
    const POISON: usize = 0xDEAD_DEAD;

    for _ in 0..256 {
        let domain = Arc::new(Domain::new());
        let payload = Arc::new(AtomicUsize::new(SENTINEL));
        let start = Arc::new(Barrier::new(3));
        let observed_poison = Arc::new(AtomicBool::new(false));

        let reader = {
            let (domain, payload, start, observed_poison) = (
                Arc::clone(&domain),
                Arc::clone(&payload),
                Arc::clone(&start),
                Arc::clone(&observed_poison),
            );
            // A brand new thread, so its slot is leased here: after the domain
            // exists, concurrently with a reclaimer already advancing. That is
            // the ordering the bounded scan has to get right.
            thread::spawn(move || {
                start.wait();
                let guard = domain.pin();
                // Retired *after* this pin, so it must not run until the guard
                // drops, even though the scan may have been bounded before this
                // thread's slot was counted.
                let poisoned = Arc::clone(&payload);
                domain.retire(move || poisoned.store(POISON, Ordering::Release));
                for _ in 0..64 {
                    if payload.load(Ordering::Acquire) == POISON {
                        observed_poison.store(true, Ordering::SeqCst);
                    }
                    std::hint::spin_loop();
                }
                drop(guard);
            })
        };

        let reclaimer = {
            let (domain, start) = (Arc::clone(&domain), Arc::clone(&start));
            thread::spawn(move || {
                start.wait();
                for _ in 0..512 {
                    domain.advance();
                }
            })
        };

        start.wait();
        reader.join().expect("reader did not panic");
        reclaimer.join().expect("reclaimer did not panic");
        assert!(
            !observed_poison.load(Ordering::SeqCst),
            "a retirement ran while a reader that pinned before it was still inside its guard"
        );
    }
}

/// A reader inside its guard never observes a payload that has been reclaimed.
///
/// The shape that matters, and the one a weaker version of this test missed:
/// the retirement has to actually *destroy* what the reader is reading, or the
/// test asserts nothing. An earlier draft retired a closure that bumped a
/// counter and had readers look for a poison value nothing ever wrote; it
/// passed with the pin scan removed entirely, which is the most complete
/// correctness failure this crate can have.
///
/// So: readers pin, take the currently published payload, and read it. The
/// writer publishes a replacement and retires a closure that poisons the old
/// one, standing in for the free. A reader that reads a poisoned payload
/// inside its own guard is a use-after-free that happened to be survivable.
#[test]
fn a_reader_never_observes_a_reclaimed_payload() {
    const SENTINEL: usize = 0x5AFE_5AFE;
    const POISON: usize = 0xDEAD_DEAD;

    let domain = Arc::new(Domain::new());
    let published: Arc<std::sync::Mutex<Arc<AtomicUsize>>> =
        Arc::new(std::sync::Mutex::new(Arc::new(AtomicUsize::new(SENTINEL))));
    let stop = Arc::new(AtomicBool::new(false));
    let observed_poison = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (domain, published, stop, observed_poison) = (
                Arc::clone(&domain),
                Arc::clone(&published),
                Arc::clone(&stop),
                Arc::clone(&observed_poison),
            );
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let guard = domain.pin();
                    // Take the payload that is current *at pin time*. Anything
                    // retired after this must not run until the guard drops.
                    let mine = Arc::clone(&published.lock().expect("not poisoned"));
                    for _ in 0..16 {
                        if mine.load(Ordering::Acquire) == POISON {
                            observed_poison.store(true, Ordering::SeqCst);
                        }
                        std::hint::spin_loop();
                    }
                    drop(guard);
                }
            })
        })
        .collect();

    for _ in 0..2_000 {
        let old = {
            let mut slot = published.lock().expect("not poisoned");
            let old = Arc::clone(&*slot);
            *slot = Arc::new(AtomicUsize::new(SENTINEL));
            old
        };
        // Retiring the poison is the stand-in for freeing `old`. If it runs
        // while a reader that pinned before this call still holds `old`, that
        // reader sees POISON.
        domain.retire(move || old.store(POISON, Ordering::Release));
        domain.advance();
    }

    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().expect("reader did not panic");
    }
    assert!(
        !observed_poison.load(Ordering::SeqCst),
        "a reader inside its guard observed a payload that had been reclaimed"
    );
}

/// Reclamation keeps making progress while readers keep arriving.
///
/// The property that rules out `seize` for this workload: a scheme that waits
/// for a moment when nobody is pinned never reaches one under continuous read
/// traffic, and its backlog grows without bound. This asserts the backlog stays
/// *bounded*, not zero: a retirement raced by a reader that pinned just before
/// it legitimately waits for that reader.
#[test]
fn reclamation_progresses_under_continuous_readers() {
    let domain = Arc::new(Domain::new());
    let stop = Arc::new(AtomicBool::new(false));
    let ran = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (domain, stop) = (Arc::clone(&domain), Arc::clone(&stop));
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let guard = domain.pin();
                    std::hint::black_box(&guard);
                    drop(guard);
                }
            })
        })
        .collect();

    const RETIREMENTS: usize = 5_000;
    for _ in 0..RETIREMENTS {
        let ran = Arc::clone(&ran);
        domain.retire(move || {
            ran.fetch_add(1, Ordering::Release);
        });
        domain.advance();
    }

    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().expect("reader did not panic");
    }
    for _ in 0..8 {
        domain.advance();
    }

    let done = ran.load(Ordering::Acquire);
    assert!(
        done >= RETIREMENTS / 2,
        "reclamation stalled under continuous readers: {done} of {RETIREMENTS} ran"
    );
    assert!(
        domain.pending() < RETIREMENTS / 2,
        "backlog grew without bound: {} still queued",
        domain.pending()
    );
}

/// More live threads than a thread has pin slots.
///
/// `PINS_PER_THREAD` is small and nesting past it falls back to a conservative
/// wildcard pin, which is correct but holds *everything*. The fallback has to
/// be safe and it has to be released, or one deeply nested reader stops the
/// domain reclaiming for the rest of the process.
#[test]
fn nested_pins_past_the_slot_count_still_release() {
    let domain = Domain::new();
    let ran = Arc::new(AtomicUsize::new(0));

    {
        let a = domain.pin();
        let b = domain.pin();
        let c = domain.pin();
        let d = domain.pin();
        let e = domain.pin();
        let f = domain.pin();
        let counter = Arc::clone(&ran);
        domain.retire(move || {
            counter.fetch_add(1, Ordering::Release);
        });
        // Held: nothing may run.
        domain.advance();
        assert_eq!(ran.load(Ordering::Acquire), 0, "reclaimed under a live pin");
        drop(f);
        drop(e);
        drop(d);
        drop(c);
        drop(b);
        drop(a);
    }

    // Released: the wildcard must be gone, or this never drains.
    for _ in 0..4 {
        domain.advance();
    }
    assert_eq!(
        ran.load(Ordering::Acquire),
        1,
        "a nested pin past the slot count did not release its wildcard"
    );
}

/// Many short-lived threads reuse slots rather than exhausting the registry.
///
/// A slot is returned on thread exit. Without that, a process spawning threads
/// in a loop walks the high-water mark up to `MAX_THREADS` and every further
/// thread shares the last slot: sound, contended, and a sudden loss of the
/// flatness the crate exists for. It also interacts with the bounded scan,
/// whose cost is that mark.
#[test]
fn short_lived_threads_return_their_slots() {
    let domain = Arc::new(Domain::new());
    for _ in 0..64 {
        let domain = Arc::clone(&domain);
        thread::spawn(move || {
            let guard = domain.pin();
            std::hint::black_box(&guard);
        })
        .join()
        .expect("thread did not panic");
    }
    // 64 sequential threads must not have consumed 64 slots.
    assert!(
        ps_reclaim::slots_in_use() < 64,
        "slots were not reused: {} handed out for 64 sequential threads",
        ps_reclaim::slots_in_use()
    );
}

/// A retirement made under a pin is not reclaimed by the pinning thread itself.
///
/// The self-hold case: a thread that pins, retires, and then advances without
/// dropping its own guard must not free what it may still be reading. It is
/// easy to get wrong by treating the current thread's slot as uninteresting.
#[test]
fn a_thread_does_not_reclaim_under_its_own_pin() {
    let domain = Domain::new();
    let ran = Arc::new(AtomicBool::new(false));

    let guard = domain.pin();
    {
        let ran = Arc::clone(&ran);
        domain.retire(move || ran.store(true, Ordering::Release));
    }
    domain.advance();
    assert!(
        !ran.load(Ordering::Acquire),
        "a thread reclaimed a retirement made under its own live pin"
    );
    drop(guard);
    domain.advance();
    assert!(
        ran.load(Ordering::Acquire),
        "not reclaimed after the guard dropped"
    );
}
