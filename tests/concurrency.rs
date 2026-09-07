//! Properties that only fail under concurrency, and the one the bounded
//! registry scan puts at risk.
//!
//! `tests/contract.rs` states the single-threaded contract. This file is the
//! other half: a domain is only useful if it is still correct when readers and
//! reclaimers run at once, and none of that is observable from one thread.
//!
//! New and changed tests in the PR #9 follow-up are source-only and have not
//! been executed or mutation-validated. Native/Miri runs sample executions;
//! they do not exhaust the weak-memory protocol. See loom_protocol.rs too.
//!
//! Run these under Miri as well as natively. A racy pointer-sized load is
//! atomic in practice on aarch64, so a native pass proves much less here than
//! it looks: `MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri test`.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use ps_reclaim::{Domain, Handle};

mod support;
use support::RemoteReader;

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
    atomic_publication(false);
}

#[test]
fn an_explicit_reader_never_observes_a_reclaimed_payload() {
    atomic_publication(true);
}

fn atomic_publication(explicit: bool) {
    const SENTINEL: usize = 0x5AFE_5AFE;
    const POISON: usize = 0xDEAD_DEAD;
    const UPDATES: usize = if cfg!(miri) { 32 } else { 2_000 };
    let domain = Domain::new();
    // Keep storage alive through joining, so broken reclamation yields an
    // assertion rather than deliberately executing a dangling dereference.
    // The deferred poison, not Arc ownership, tests the grace period.
    let payloads: Arc<[AtomicUsize]> = (0..=UPDATES)
        .map(|_| AtomicUsize::new(SENTINEL))
        .collect::<Vec<_>>()
        .into();
    let published = AtomicPtr::new(&payloads[0] as *const AtomicUsize as *mut AtomicUsize);
    let start = Barrier::new(6); // four readers, second reclaimer, writer
    thread::scope(|scope| {
        for _ in 0..4 {
            let (domain, published, start) = (&domain, &published, &start);
            scope.spawn(move || {
                start.wait();
                let handle = if explicit { Some(Handle::new()) } else { None };
                for _ in 0..UPDATES {
                    let read = || {
                        let mine = published.load(Ordering::Acquire);
                        // SAFETY: root always names an element in payloads,
                        // whose allocation remains live until the scope joins.
                        let mine = unsafe { &*mine };
                        for _ in 0..4 {
                            assert_ne!(mine.load(Ordering::Acquire), POISON);
                            std::hint::spin_loop();
                        }
                    };
                    if let Some(handle) = &handle {
                        let guard = domain.pin_with(handle);
                        read();
                        drop(guard);
                    } else {
                        let guard = domain.pin();
                        read();
                        drop(guard);
                    }
                }
            });
        }
        let (scanning, ready) = (&domain, &start);
        scope.spawn(move || {
            ready.wait();
            for _ in 0..UPDATES * 2 {
                scanning.advance_up_to(8);
                thread::yield_now();
            }
        });
        start.wait();
        for index in 0..UPDATES {
            let guard = domain.pin();
            let next = &payloads[index + 1] as *const AtomicUsize as *mut AtomicUsize;
            let old = published.swap(next, Ordering::AcqRel);
            assert!(core::ptr::eq(old, &payloads[index]));
            let retired = Arc::clone(&payloads);
            domain.retire(move || retired[index].store(POISON, Ordering::Release));
            drop(guard);
            domain.advance_up_to(8);
        }
    });
    domain.advance();
    assert_eq!(domain.pending(), 0);
}

/// Reclamation keeps making progress while readers keep arriving.
///
/// Every handover preserves overlap. Assert progress BEFORE releasing the last
/// reader, not after stopping traffic and allowing a quiescent cleanup pass.
#[test]
fn reclamation_progresses_under_continuous_readers() {
    let domain = Arc::new(Domain::new());
    let ran = Arc::new(AtomicUsize::new(0));
    let first = RemoteReader::spawn(Arc::clone(&domain));
    let second = RemoteReader::spawn(Arc::clone(&domain));
    first.pin();
    const RETIREMENTS: usize = if cfg!(miri) { 16 } else { 256 };
    for expected in 1..=RETIREMENTS {
        let counter = Arc::clone(&ran);
        domain.retire(move || {
            counter.fetch_add(1, Ordering::Release);
        });
        second.pin();
        first.unpin();
        domain.advance();
        first.pin(); // strictly later epoch; second still holds its old pin
        second.unpin();
        domain.advance();
        assert_eq!(
            ran.load(Ordering::Acquire),
            expected,
            "stalled while readers overlapped"
        );
        assert_eq!(
            domain.pending(),
            0,
            "backlog grew during controlled traffic"
        );
    }
    first.unpin();
    domain.advance();
    assert_eq!(ran.load(Ordering::Acquire), RETIREMENTS);
}

/// A hole left by an out-of-order drop is reused without falling back to the
/// process-wide wildcard. WorkTable nests its page and index domains on every
/// select, so this is both a correctness property and a hot-path contract.
#[test]
fn out_of_order_drop_reuses_the_exact_pin_slot() {
    let held_domain = Domain::new();
    let unrelated = Domain::new();
    let a = held_domain.pin();
    let b = held_domain.pin();
    let c = held_domain.pin();
    let d = held_domain.pin();
    drop(b);
    let replacement = held_domain.pin();

    let ran = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ran);
    unrelated.retire(move || {
        counter.fetch_add(1, Ordering::Release);
    });
    for _ in 0..4 {
        unrelated.advance();
    }
    assert_eq!(
        ran.load(Ordering::Acquire),
        1,
        "reusing a free nested slot incorrectly published a wildcard pin"
    );

    drop(replacement);
    drop(d);
    drop(c);
    drop(a);
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
        ps_reclaim::slots_handed_out() < 64,
        "slots were not reused: {} handed out for 64 sequential threads",
        ps_reclaim::slots_handed_out()
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
