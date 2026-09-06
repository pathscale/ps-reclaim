//! Process-wide participant registry.
//!
//! Threads register once and pubish *which domains* they are pinned in. The
//! registry is shared across every domain rather than owned by each, because
//! there is a domain per table and a per-domain array would cost megabytes at
//! a thousand tables.

use crate::sync::Mutex;
use alloc::vec::Vec;
use core::cell::Cell;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
        static REGISTRY: crate::sync::OnceLock<Registry> = crate::sync::OnceLock::new();
        crate::sync::get_or_init(&REGISTRY, || Registry {
            slots: (0..MAX_THREADS).map(|_| Participant::new()).collect(),
            free: Mutex::new(Vec::new()),
            next: AtomicUsize::new(0),
        })
    }

    /// A slot, and whether this thread had to share the overflow one.
    ///
    /// Sharing is not "sound but contended", which is what this used to claim.
    /// A slot's pins are single values, not a counted aggregate: two threads on
    /// one slot both write `pins[0]`, and the first to drop its guard stores
    /// `NO_DOMAIN` over a pin the other still holds. The reclaimer then reads
    /// that slot as idle and is free to reclaim something the second thread is
    /// still reading. That is a use-after-free, not a delay.
    ///
    /// So the overflow slot is reported, and callers put those threads on the
    /// wildcard instead, which *is* a counter and therefore composes.
    pub(crate) fn acquire(&self) -> (usize, bool) {
        if let Some(idx) = crate::sync::lock(&self.free).pop() {
            return (idx, false);
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed);
        if idx < MAX_THREADS - 1 {
            (idx, false)
        } else {
            (MAX_THREADS - 1, true)
        }
    }

    fn release(&self, idx: usize, shared: bool) {
        // A shared slot belongs to every overflow thread at once. Clearing its
        // pins or resetting its wildcard here would erase state those other
        // threads still depend on, which is the same defect as sharing the
        // pins in the first place. An overflow thread never writes `pins`, and
        // its wildcard is decremented by its own guard, so leaving is nothing.
        if shared {
            return;
        }
        let p = &self.slots[idx];
        for pin in &p.pins {
            pin.store(NO_DOMAIN, Ordering::Release);
        }
        p.wildcard.store(0, Ordering::Release);
        crate::sync::lock(&self.free).push(idx);
    }
}

/// Returns this thread's slot when the thread exits.
pub(crate) struct SlotLease(pub(crate) usize, pub(crate) bool);

impl Drop for SlotLease {
    fn drop(&mut self) {
        Registry::get().release(self.0, self.1);
    }
}

/// This thread's per-thread state, in one place.
///
/// It was three separate thread-locals, and `pin` touched all three while
/// `Guard::drop` touched a fourth. That is four lookups per pin/unpin cycle,
/// which is free where a thread-local is a register offset and is not free
/// where it is a `pthread_getspecific` call. Measured on the burst benchmark,
/// the `no_std` build lost 22 to 40% of Arctic's throughput to exactly this.
///
/// None of the three has a destructor, and they are nine bytes between them.
/// One lookup in `pin` and one in `drop` costs half of what four did, and the
/// grouping is honest: these three are read together, always.
pub(crate) struct Local {
    /// Cached so the read path touches neither the `OnceLock` nor the slot
    /// `Vec`. Doing both on pin and unpin measured four times the cost of the
    /// pin itself.
    pub(crate) mine: Cell<Option<&'static Participant>>,
    /// Whether this thread shares the overflow slot, and so must pin through
    /// the wildcard rather than through `pins`.
    pub(crate) shared: Cell<bool>,
    /// Occupied entries in this thread's participant slot. Guards are `!Send`,
    /// so this is exact rather than a hint and handles out-of-order drops.
    pub(crate) pin_mask: Cell<u8>,
}

impl Local {
    const fn new() -> Self {
        Self {
            mine: Cell::new(None),
            shared: Cell::new(false),
            pin_mask: Cell::new(0),
        }
    }
}
#[cfg(feature = "std")]
thread_local! {
    static LOCAL: Local = const { Local::new() };
    static LEASE: RefCell<Option<SlotLease>> = const { RefCell::new(None) };
}

// Without `std`, `LEASE` is a platform key because it has a destructor that
// has to run at thread exit. That is the one thing `#[thread_local]` cannot do
// and the one thing a platform key is genuinely needed for here.
#[cfg(not(feature = "std"))]
static LEASE: crate::tls::Tls<RefCell<Option<SlotLease>>> = crate::tls::Tls::new();

// `Local` is the hot one and it has no destructor, so on nightly it can be the
// same thing `thread_local!` lowers to: an address off the thread pointer, no
// call, no key. Measured identical to `std` and roughly half the cost of
// `pthread_getspecific`.
#[cfg(all(not(feature = "std"), feature = "nightly"))]
#[thread_local]
static LOCAL: Local = Local::new();

// Stable `no_std` has no way to say that, so it pays for a key.
#[cfg(all(not(feature = "std"), not(feature = "nightly")))]
static LOCAL: crate::tls::Tls<Local> = crate::tls::Tls::new();

/// Run `f` against this thread's `Local`, which is one lookup.
///
/// Callers on the hot path take this **once** and read every field they need
/// inside the closure. Calling it three times to read three fields puts back
/// exactly the cost this exists to remove.
#[cfg(feature = "std")]
#[inline]
pub(crate) fn with_local<R>(f: impl FnOnce(&Local) -> R) -> R {
    LOCAL.with(f)
}

/// No lookup at all: the address is an offset from the thread pointer, and
/// there is no lazy-init flag because `Local::new` is a `const` initialiser.
#[cfg(all(not(feature = "std"), feature = "nightly"))]
#[inline]
pub(crate) fn with_local<R>(f: impl FnOnce(&Local) -> R) -> R {
    f(&LOCAL)
}

#[cfg(all(not(feature = "std"), not(feature = "nightly")))]
#[inline]
pub(crate) fn with_local<R>(f: impl FnOnce(&Local) -> R) -> R {
    LOCAL
        .with(Local::new, f)
        .expect("thread-local storage is gone during teardown")
}

/// Install this thread's lease, whose `Drop` returns the slot at thread exit.
///
/// Failure is tolerated on purpose, which is why this returns nothing: losing
/// the lease during teardown leaks one slot rather than recycling it, and
/// `acquire` is bounded rather than fallible for exactly that reason.
#[cfg(feature = "std")]
#[inline]
fn install_lease(lease: SlotLease) {
    let _ = LEASE.try_with(|l| *l.borrow_mut() = Some(lease));
}

#[cfg(not(feature = "std"))]
#[inline]
fn install_lease(lease: SlotLease) {
    let _ = LEASE.with(|| RefCell::new(None), |l| *l.borrow_mut() = Some(lease));
}

/// This thread's participant slot, given a `Local` the caller already holds.
///
/// The hot path calls this rather than `participant`, so registration and the
/// pin that follows it share one thread-local lookup.
#[inline]
pub(crate) fn participant_in(local: &Local) -> &'static Participant {
    if let Some(p) = local.mine.get() {
        return p;
    }
    register(local)
}

/// First touch on this thread: take a slot and remember it.
///
/// Split out and marked cold so the branch above stays a load and a null test.
/// It reaches the registry mutex, which is the one place this crate takes a
/// lock, and it happens once per thread.
#[cold]
#[inline(never)]
fn register(local: &Local) -> &'static Participant {
    let registry = Registry::get();
    let (idx, shared) = registry.acquire();
    let p = &registry.slots[idx];
    local.mine.set(Some(p));
    local.shared.set(shared);
    // Installed separately so its `Drop` runs at thread exit. Failing during
    // TLS teardown leaks one slot rather than recycling it, which is why
    // `acquire` is bounded rather than fallible.
    install_lease(SlotLease(idx, shared));
    p
}

/// Slots handed out so far, which is a high-water mark and not a live count.
///
/// `release` returns an index to the free list without lowering this, so a
/// process that started and joined many threads reads high while few are live.
/// That is deliberate and it is what the diagnostic wants: past
/// [`MAX_THREADS`] every further thread shares the last slot, which is sound
/// but contended, and only the high-water mark shows that happened at all. A
/// live count would fall back to a flat number afterwards and hide it.
///
/// It was called `slots_in_use`, which said the opposite of what it returns.
pub fn slots_handed_out() -> usize {
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
