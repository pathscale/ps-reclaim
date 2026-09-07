//! Source-only reduced model: two normal registrations, repinning, a root
//! unlinker and two reclaimers. A two-bit clock reaches conservative saturation.
//! Queue stamps, orderings and the scan cutoff mirror the production protocol;
//! TLS/FLS, wildcard counters, allocation, teardown, exclusive epoch renewal
//! and returned-guard layout are NOT modeled. See docs/review-pr9.md.
#![cfg(ps_loom)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};
use loom::sync::{Arc, Mutex};
use loom::thread;
use std::collections::VecDeque;

const MASK: u64 = 3;

struct Retired {
    sequence: u64,
    epoch: u64,
    payload: bool,
}

struct Garbage {
    next: u64,
    entries: VecDeque<Retired>,
}

struct Domain {
    epoch: AtomicU64,
    next_slot: AtomicUsize,
    pins: [AtomicU64; 2],
    root: AtomicUsize,
    value: UnsafeCell<usize>,
    garbage: Mutex<Garbage>,
}

// SAFETY: accesses to value are intended to be ordered by the modeled pin/
// reclaim protocol. Loom's checked cell audits that obligation; using an
// atomic payload here would hide missing happens-before edges on reclamation.
unsafe impl Sync for Domain {}

impl Domain {
    fn new() -> Self {
        Self {
            epoch: AtomicU64::new(2),
            next_slot: AtomicUsize::new(0),
            pins: [AtomicU64::new(0), AtomicU64::new(0)],
            root: AtomicUsize::new(1),
            value: UnsafeCell::new(42),
            garbage: Mutex::new(Garbage {
                next: 1,
                // Seed a scan even when the actual unlink has not happened.
                entries: VecDeque::from([Retired {
                    sequence: 0,
                    epoch: 2,
                    payload: false,
                }]),
            }),
        }
    }

    fn read_twice(&self) {
        let slot = self.next_slot.fetch_add(1, Ordering::Relaxed);
        for _ in 0..2 {
            let now = self.epoch.load(Ordering::Relaxed);
            self.pins[slot].store(((now & MASK) << 1) | 1, Ordering::Release);
            fence(Ordering::SeqCst);
            if self.root.load(Ordering::Acquire) != 0 {
                // SAFETY: modeled pin must exclude reclaiming writes. Loom's
                // checked cell reports conflicting accesses if it does not.
                self.value.with(|ptr| assert_eq!(unsafe { *ptr }, 42));
            }
            self.pins[slot].store(0, Ordering::Release);
        }
    }

    fn unlink(&self) {
        self.root.store(0, Ordering::Release);
        let now = self.epoch.load(Ordering::Relaxed);
        let mut garbage = self.garbage.lock().unwrap();
        let sequence = garbage.next;
        garbage.next += 1;
        garbage.entries.push_back(Retired {
            sequence,
            epoch: now,
            payload: true,
        });
    }

    fn advance(&self) {
        let cutoff = {
            let garbage = self.garbage.lock().unwrap();
            if garbage.entries.is_empty() {
                return;
            }
            garbage.next
        };
        fence(Ordering::SeqCst);
        let now = self.epoch.load(Ordering::Relaxed);
        let mut oldest = now + 1;
        let leased = self.next_slot.load(Ordering::Relaxed);
        for pin in &self.pins[..leased] {
            let packed = pin.load(Ordering::Acquire);
            if packed != 0 {
                let decoded = packed >> 1;
                oldest = oldest.min(decoded);
            }
        }
        let expired = {
            let mut garbage = self.garbage.lock().unwrap();
            let mut expired = None;
            for _ in 0..garbage.entries.len() {
                let item = garbage.entries.pop_front().unwrap();
                if item.sequence < cutoff && item.epoch < oldest {
                    expired = Some(item);
                    break;
                }
                garbage.entries.push_back(item);
            }
            expired
        };
        if expired.is_some_and(|item| item.payload) {
            // SAFETY: only the single payload retirement writes this cell,
            // after the modeled scan determined that no old reader remains.
            self.value.with_mut(|ptr| unsafe { *ptr = 0 });
        }
        let _ = self.epoch.compare_exchange(
            now,
            (now + 1).min(MASK),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

#[test]
fn registration_repin_and_two_reclaimers() {
    let mut model = loom::model::Builder::new();
    // Deliberately bounded smoke model, not an exhaustive proof. Environment
    // settings can increase coverage without editing the protocol.
    if model.preemption_bound.is_none() {
        model.preemption_bound = Some(2);
    }
    if model.max_permutations.is_none() {
        model.max_permutations = Some(20_000);
    }
    model.check(|| {
        let domain = Arc::new(Domain::new());
        let mut workers = Vec::new();
        for _ in 0..2 {
            let domain = Arc::clone(&domain);
            workers.push(thread::spawn(move || domain.read_twice()));
        }
        let writer = Arc::clone(&domain);
        workers.push(thread::spawn(move || {
            writer.unlink();
            for _ in 0..3 {
                writer.advance();
            }
        }));
        let reclaimer = Arc::clone(&domain);
        workers.push(thread::spawn(move || {
            for _ in 0..3 {
                reclaimer.advance();
            }
        }));
        for worker in workers {
            worker.join().unwrap();
        }
        for _ in 0..2 {
            domain.advance();
        }
        assert!(domain.garbage.lock().unwrap().entries.is_empty());
        domain.value.with(|ptr| assert_eq!(unsafe { *ptr }, 0));
    });
}

// Isolate the store/load requirement. No unpin is allowed in this litmus:
// observing both the old root and the idle participant is always forbidden.
fn fence_litmus(reader_fence: bool, scanner_fence: bool) {
    loom::model(move || {
        let pin = Arc::new(AtomicUsize::new(0));
        let root = Arc::new(AtomicUsize::new(1));
        let (reader_pin, reader_root) = (Arc::clone(&pin), Arc::clone(&root));
        let reader = thread::spawn(move || {
            reader_pin.store(1, Ordering::Release);
            if reader_fence {
                fence(Ordering::SeqCst);
            }
            reader_root.load(Ordering::Acquire) == 1
        });
        let scanner = thread::spawn(move || {
            root.store(0, Ordering::Release);
            if scanner_fence {
                fence(Ordering::SeqCst);
            }
            pin.load(Ordering::Acquire) == 0
        });
        let old_root = reader.join().unwrap();
        let missed_pin = scanner.join().unwrap();
        assert!(
            !(old_root && missed_pin),
            "both missed the opposing publication"
        );
    });
}

#[test]
fn paired_fences_prevent_store_buffering() {
    fence_litmus(true, true);
}

#[test]
#[should_panic(expected = "both missed the opposing publication")]
fn removing_reader_fence_is_detected() {
    fence_litmus(false, true);
}

#[test]
#[should_panic(expected = "both missed the opposing publication")]
fn removing_scanner_fence_is_detected() {
    fence_litmus(true, false);
}
