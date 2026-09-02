//! Process-wide participant registry.
//!
//! Threads register once and publish *which domains* they are pinned in. The
//! registry is shared across every domain rather than owned by each, because
//! there is a domain per table and a per-domain array would cost megabytes at
//! a thousand tables.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// See [`crate::MAX_THREADS`].
pub(crate) const MAX_THREADS: usize = 256;

/// See [`crate::MAX_NESTED_PINS`].
pub(crate) const PINS_PER_THREAD: usize = 4;

/// Empty pin entry. Domain ids start at 1, so zero is unambiguous.
pub(crate) const NO_DOMAIN: u64 = 0;

/// Bits of a packed entry given to the domain id, leaving 40 for the epoch.
pub(crate) const DOMAIN_BITS: u32 = 24;
pub(crate) const DOMAIN_MASK: u64 = (1 << DOMAIN_BITS) - 1;

/// One thread's pins, on its own cache line so no reader invalidates another's.
#[repr(align(128))]
pub(crate) struct Participant {
    /// Packed `(epoch << DOMAIN_BITS) | domain_id`, or [`NO_DOMAIN`]. Packed
    /// so publishing a pin is one store rather than two.
    pub(crate) pins: [AtomicU64; PINS_PER_THREAD],
    /// Non-zero when this thread ran out of entries and must be treated as
    /// pinned in every domain.
    pub(crate) wildcard: AtomicU64,
}

impl Participant {
    const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            pins: [ZERO; PINS_PER_THREAD],
            wildcard: ZERO,
        }
    }
}

pub(crate) struct Registry {
    pub(crate) slots: Vec<Participant>,
    /// Indices handed back by exited threads, reused rather than growing
    /// without bound in a program that spawns many short-lived threads.
    free: Mutex<Vec<usize>>,
    next: AtomicUsize,
}

impl Registry {
    pub(crate) fn get() -> &'static Registry {
        static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
        REGISTRY.get_or_init(|| Registry {
            slots: (0..MAX_THREADS).map(|_| Participant::new()).collect(),
            free: Mutex::new(Vec::new()),
            next: AtomicUsize::new(0),
        })
    }

    fn acquire(&self) -> usize {
        if let Some(idx) = self.free.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            return idx;
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed);
        idx.min(MAX_THREADS - 1)
    }

    fn release(&self, idx: usize) {
        let p = &self.slots[idx];
        for pin in &p.pins {
            pin.store(NO_DOMAIN, Ordering::Release);
        }
        p.wildcard.store(0, Ordering::Release);
        if idx < MAX_THREADS - 1 {
            self.free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(idx);
        }
    }
}

/// Returns this thread's slot when the thread exits.
struct SlotLease(usize);

impl Drop for SlotLease {
    fn drop(&mut self) {
        Registry::get().release(self.0);
    }
}

thread_local! {
    /// Cached so the read path touches neither the `OnceLock` nor the slot
    /// `Vec`. Doing both on pin and unpin measured four times the cost of the
    /// pin itself.
    static MINE: Cell<Option<&'static Participant>> = const { Cell::new(None) };
    static LEASE: std::cell::RefCell<Option<SlotLease>> =
        const { std::cell::RefCell::new(None) };
}

/// This thread's participant slot.
#[inline]
pub(crate) fn participant() -> &'static Participant {
    if let Some(p) = MINE.with(|m| m.get()) {
        return p;
    }
    let registry = Registry::get();
    let idx = registry.acquire();
    let p = &registry.slots[idx];
    MINE.with(|m| m.set(Some(p)));
    // Installed separately so its `Drop` runs at thread exit. Failing during
    // TLS teardown leaks one slot rather than recycling it, which is why
    // `acquire` is bounded rather than fallible.
    let _ = LEASE.try_with(|l| *l.borrow_mut() = Some(SlotLease(idx)));
    p
}

/// Slots handed out so far. Diagnostic: past [`MAX_THREADS`] every further
/// thread shares the last slot, which is sound but contended, and shows up as
/// a sudden loss of flatness.
pub fn slots_in_use() -> usize {
    Registry::get().next.load(Ordering::Relaxed)
}

impl Registry {
    /// The slots a scan has to look at: every slot ever handed out.
    ///
    /// A slot is only ever handed out by `acquire`, which bumps `next`, and
    /// `release` returns the index to the free list without lowering `next`.
    /// So slots at or above this index have never been leased to a thread and
    /// their pins are still the `NO_DOMAIN` they were constructed with: a
    /// reader cannot appear in one without first taking a lease, which would
    /// have raised `next` before the pin was published.
    ///
    /// Reading it `Relaxed` is enough because it can only grow, and a scan
    /// that observes a stale smaller value is one that ran before the thread
    /// in question could pin. That thread's `pin` publishes with a `SeqCst`
    /// fence and `advance` reads with one, which is the ordering that makes a
    /// concurrent pin visible; the bound only decides how far to look, and a
    /// pin newer than the bound is newer than the fence too.
    pub(crate) fn active_slots(&self) -> &[Participant] {
        let leased = self.next.load(Ordering::Relaxed).min(MAX_THREADS);
        &self.slots[..leased]
    }
}
