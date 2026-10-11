//! `ReaderSlot` / `Domain::pin_reader`: the async-signal-safe read path.
//!
//! What it promises, one test each: no allocation on pin or unpin, a pin
//! taken through a slot holds retirements (also from several threads on one
//! slot, and nested as a signal handler would nest it), and a real signal
//! handler can pin through it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, OnceLock};

use ps_reclaim::{Domain, ReaderSlot};

/// Counts, per thread, allocations made while that thread sets `COUNTING`.
struct Counting;
thread_local! { static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
thread_local! { static COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.try_with(|c| c.get()).unwrap_or(false) {
            let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn retire_flag(domain: &Domain) -> Arc<AtomicBool> {
    let ran = Arc::new(AtomicBool::new(false));
    let r = ran.clone();
    domain.retire(move || r.store(true, Ordering::SeqCst));
    ran
}

fn drive(domain: &Domain) {
    for _ in 0..8 {
        domain.advance();
    }
}

/// Advances until `ran` is set, for up to ten seconds. A fixed number of
/// advances is not enough: wildcard pins are process-wide and delay
/// reclamation in every domain, and the other tests in this binary take
/// them (more than four pins on one slot) while they run in parallel.
fn drive_until(domain: &Domain, ran: &AtomicBool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ran.load(Ordering::SeqCst) {
        if std::time::Instant::now() > deadline {
            return false;
        }
        domain.advance();
        std::thread::yield_now();
    }
    true
}

#[test]
fn pin_and_unpin_through_a_slot_allocate_nothing() {
    let slot = ReaderSlot::register();
    let a = Domain::new();
    let b = Domain::new();
    COUNTING.with(|c| c.set(true));
    for _ in 0..1000 {
        let g = a.pin_reader(&slot);
        let h = b.pin_reader(&slot);
        let n: Vec<_> = (0..6).map(|_| a.pin_reader(&slot)).collect::<Vec<_>>();
        drop(n);
        drop(g);
        drop(h);
    }
    COUNTING.with(|c| c.set(false));
    // The Vec above is the only allocation: one per iteration.
    assert_eq!(
        ALLOCS.with(|n| n.get()),
        1000,
        "pin_reader or its guard allocated"
    );
}

#[test]
fn a_slot_pin_holds_an_older_retirement() {
    let slot = ReaderSlot::register();
    let domain = Domain::new();
    let guard = domain.pin_reader(&slot);
    let ran = retire_flag(&domain);
    drive(&domain);
    assert!(
        !ran.load(Ordering::SeqCst),
        "ran while a slot reader was pinned"
    );
    drop(guard);
    assert!(
        drive_until(&domain, &ran),
        "never ran after the slot reader left"
    );
}

/// Two pins on one slot, the inner dropped first, as a signal handler that
/// interrupted a reader would. The outer pin must still hold, and so must
/// the inner when the outer goes first.
#[test]
fn nested_pins_on_one_slot_each_hold() {
    let slot = ReaderSlot::register();
    let domain = Domain::new();
    for inner_first in [true, false] {
        let outer = domain.pin_reader(&slot);
        let inner = domain.pin_reader(&slot);
        let ran = retire_flag(&domain);
        let (first, second) = if inner_first {
            (inner, outer)
        } else {
            (outer, inner)
        };
        drop(first);
        drive(&domain);
        assert!(
            !ran.load(Ordering::SeqCst),
            "dropping one pin released the other (inner_first={inner_first})"
        );
        drop(second);
        assert!(drive_until(&domain, &ran));
    }
}

/// More threads than pin entries pin one shared `static`-style slot at once,
/// then all but one leave. The one left must still hold the retirement.
#[test]
fn many_threads_on_one_slot_each_hold() {
    const THREADS: usize = 8;
    let slot = Arc::new(ReaderSlot::register());
    let domain = Arc::new(Domain::new());
    for keeper in 0..THREADS {
        let pinned = Arc::new(Barrier::new(THREADS + 1));
        let release = Arc::new(Barrier::new(THREADS + 1));
        let last = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..THREADS)
            .map(|i| {
                let (slot, domain) = (slot.clone(), domain.clone());
                let (pinned, release, last) = (pinned.clone(), release.clone(), last.clone());
                std::thread::spawn(move || {
                    let g = domain.pin_reader(&slot);
                    pinned.wait();
                    release.wait();
                    if i == keeper {
                        last.wait();
                        last.wait();
                    }
                    drop(g);
                })
            })
            .collect();
        pinned.wait();
        let ran = retire_flag(&domain);
        release.wait();
        last.wait();
        // Everyone but the keeper may have dropped. Wait for them.
        while threads
            .iter()
            .enumerate()
            .any(|(i, t)| i != keeper && !t.is_finished())
        {
            std::thread::yield_now();
        }
        drive(&domain);
        assert!(
            !ran.load(Ordering::SeqCst),
            "freed under the last reader on the slot (keeper {keeper})"
        );
        last.wait();
        for t in threads {
            t.join().unwrap();
        }
        assert!(drive_until(&domain, &ran));
    }
}

#[cfg(unix)]
mod signal {
    use super::*;

    static SLOT: OnceLock<ReaderSlot> = OnceLock::new();
    static DOMAIN: OnceLock<Domain> = OnceLock::new();
    static IN_HANDLER: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn handler(_: libc::c_int) {
        // Only atomics and pin_reader: what a crash handler may do.
        let (slot, domain) = (SLOT.get().unwrap(), DOMAIN.get().unwrap());
        let g = domain.pin_reader(slot);
        IN_HANDLER.fetch_add(1, Ordering::SeqCst);
        drop(g);
    }

    /// The handler interrupts a pin held through the same slot on the same
    /// thread, pins and unpins; the interrupted pin must still hold.
    #[test]
    fn a_signal_handler_pins_through_the_slot() {
        SLOT.get_or_init(ReaderSlot::register);
        let domain = DOMAIN.get_or_init(Domain::new);
        unsafe { libc::signal(libc::SIGUSR2, handler as *const () as libc::sighandler_t) };
        let outer = domain.pin_reader(SLOT.get().unwrap());
        let ran = retire_flag(domain);
        COUNTING.with(|c| c.set(true));
        let before = ALLOCS.with(|n| n.get());
        unsafe { libc::raise(libc::SIGUSR2) };
        let after = ALLOCS.with(|n| n.get());
        COUNTING.with(|c| c.set(false));
        assert_eq!(IN_HANDLER.load(Ordering::SeqCst), 1);
        assert_eq!(after, before, "the handler allocated");
        drive(domain);
        assert!(
            !ran.load(Ordering::SeqCst),
            "the handler's unpin released the interrupted pin"
        );
        drop(outer);
        assert!(drive_until(domain, &ran));
    }
}
