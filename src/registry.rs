//! Process-wide participant registry.
//!
//! Threads register once and publish *which domains* they are pinned in. The
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

/// Empty pin entry. Published epochs start at 1 and never wrap, so a packed
/// pin is nonzero even if its truncated domain ID is zero.
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
        // Keep the diagnostic counter monotonic even on long-lived 32-bit
        // targets. Wrapping could hand out an exclusive slot still in use.
        let idx = self.next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        }).unwrap_or(usize::MAX);
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
        // A guard may outlive the TLS lease in another TLS destructor, or be
        // forgotten through safe code. Never erase such a pin to recycle its
        // slot. Leaking the registration in this exceptional case is safer
        // than permitting a new owner to overwrite a still-live reader.
        if p.wildcard.load(Ordering::Acquire) != 0
            || p.pins.iter().any(|pin| pin.load(Ordering::Acquire) != NO_DOMAIN)
        {
            return;
        }
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
/// The cache has no destructor. Its owner closes it before releasing the slot;
/// one lookup in `pin` and one in `drop` access the related fields together.
pub(crate) struct Local {
    /// Cached so the read path touches neither the `OnceLock` nor the slot
    /// `Vec` after registration.
    pub(crate) mine: Cell<Option<&'static Participant>>,
    /// Whether this thread shares the overflow slot, and so must pin through
    /// the wildcard rather than through `pins`.
    pub(crate) shared: Cell<bool>,
    /// Occupied entries in this thread's participant slot. Guards are `!Send`,
    /// so this is exact rather than a hint and handles out-of-order drops.
    pub(crate) pin_mask: Cell<u8>,
    closed: Cell<bool>,
}

impl Local {
    const fn new() -> Self {
        Self {
            mine: Cell::new(None),
            shared: Cell::new(false),
            pin_mask: Cell::new(0),
            closed: Cell::new(false),
        }
    }

    fn close(&self) {
        self.closed.set(true);
        self.mine.set(None);
    }
}

// Native Local has no destructor. Close its cache before returning the lease.
// The hot cache remains available during later TLS destructors, but attempting
// to register through it again is rejected on the cold path.
#[cfg(any(feature = "std", all(feature = "nightly", unix)))]
struct ThreadLease {
    _lease: SlotLease,
}

#[cfg(any(feature = "std", all(feature = "nightly", unix)))]
impl Drop for ThreadLease {
    fn drop(&mut self) {
        let _ = try_with_local(Local::close);
    }
}

#[cfg(feature = "std")]
thread_local! {
    static LOCAL: Local = const { Local::new() };
    static LEASE: RefCell<Option<ThreadLease>> = const { RefCell::new(None) };
}

// Without `std`, `LEASE` is a platform key because it has a destructor that
// has to run at thread exit. That is the one thing `#[thread_local]` cannot do
// and the one thing a platform key is genuinely needed for here.
#[cfg(all(not(feature = "std"), feature = "nightly", unix))]
static LEASE: crate::tls::Tls<RefCell<Option<ThreadLease>>> = crate::tls::Tls::new();

// `Local` is the hot one and it has no destructor, so on nightly it can be the
// native TLS cache. Generated access costs depend on the target and linkage;
// the platform-key lease is only accessed on registration and teardown.
#[cfg(all(not(feature = "std"), feature = "nightly", unix))]
#[thread_local]
static LOCAL: Local = Local::new();

// On Windows, FLS lifetime is a fiber's lifetime, not a thread's. Keep both
// cache and owner in the same FLS object, including when nightly is enabled.
#[cfg(all(not(feature = "std"), not(all(feature = "nightly", unix))))]
struct OwnedLocal {
    local: Local,
    lease: RefCell<Option<SlotLease>>,
}

#[cfg(all(not(feature = "std"), not(all(feature = "nightly", unix))))]
impl OwnedLocal {
    fn new() -> Self {
        Self { local: Local::new(), lease: RefCell::new(None) }
    }
}

#[cfg(all(not(feature = "std"), not(all(feature = "nightly", unix))))]
impl Drop for OwnedLocal {
    fn drop(&mut self) {
        self.local.close();
        // `lease` is dropped after the cache is closed.
    }
}

#[cfg(all(not(feature = "std"), not(all(feature = "nightly", unix))))]
static LOCAL: crate::tls::Tls<OwnedLocal> = crate::tls::Tls::new();

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

/// Native TLS cache access; the generated addressing sequence is target-specific.
#[cfg(all(not(feature = "std"), feature = "nightly", unix))]
#[inline]
pub(crate) fn with_local<R>(f: impl FnOnce(&Local) -> R) -> R {
    f(&LOCAL)
}

#[cfg(all(not(feature = "std"), not(all(feature = "nightly", unix))))]
#[inline]
pub(crate) fn with_local<R>(f: impl FnOnce(&Local) -> R) -> R {
    LOCAL
        .with(OwnedLocal::new, |owned| f(&owned.local))
        .expect("cannot initialize platform-local registration storage")
}

/// Install this thread's lease, whose `Drop` returns the slot at thread exit.
///
/// Publish the cached participant only if installation succeeds. A rejected
/// installation drops the lease and returns the unused slot, not a stale cache.
#[cfg(feature = "std")]
#[inline]
fn install_lease(lease: SlotLease) -> bool {
    LEASE.try_with(|l| *l.borrow_mut() = Some(ThreadLease { _lease: lease })).is_ok()
}

#[cfg(all(not(feature = "std"), feature = "nightly", unix))]
#[inline]
fn install_lease(lease: SlotLease) -> bool {
    LEASE.with(|| RefCell::new(None), |l| *l.borrow_mut() = Some(ThreadLease { _lease: lease })).is_some()
}

#[cfg(all(not(feature = "std"), not(all(feature = "nightly", unix))))]
fn install_lease(lease: SlotLease) -> bool {
    LOCAL.get(|owned| *owned.lease.borrow_mut() = Some(lease)).is_some()
}

/// Access only an existing Local. Guard destruction must not initialize a new
/// registration during TLS teardown.
#[cfg(feature = "std")]
#[inline]
pub(crate) fn try_with_local<R>(f: impl FnOnce(&Local) -> R) -> Option<R> {
    LOCAL.try_with(f).ok()
}

#[cfg(all(not(feature = "std"), feature = "nightly", unix))]
#[inline]
pub(crate) fn try_with_local<R>(f: impl FnOnce(&Local) -> R) -> Option<R> {
    Some(f(&LOCAL))
}

#[cfg(all(not(feature = "std"), not(all(feature = "nightly", unix))))]
#[inline]
pub(crate) fn try_with_local<R>(f: impl FnOnce(&Local) -> R) -> Option<R> {
    LOCAL.get(|owned| f(&owned.local))
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
/// It reaches the registry mutex, outside the steady-state read path.
#[cold]
#[inline(never)]
fn register(local: &Local) -> &'static Participant {
    assert!(!local.closed.get(), "cannot pin after registration teardown");
    let registry = Registry::get();
    let (idx, shared) = registry.acquire();
    let p = &registry.slots[idx];
    // Install ownership before publishing a cache that readers can use.
    assert!(install_lease(SlotLease(idx, shared)), "cannot install registration during teardown");
    local.mine.set(Some(p));
    local.shared.set(shared);
    p
}

/// Slots handed out so far, which is a high-water mark and not a live count.
///
/// `release` returns an index to the free list without lowering this, so a
/// process that started and joined many threads reads high while few are live.
/// Once exclusive capacity is reached, acquisition uses returned free slots
/// first, otherwise the shared wildcard slot. This counter includes overflow
/// acquisition attempts, but not free-list reuse, and saturates at usize::MAX.
/// It is neither a live-registration count nor a literal count of unique slots.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturated_registration_counter_never_reissues_an_exclusive_slot() {
        let registry = Registry {
            slots: (0..MAX_THREADS).map(|_| Participant::new()).collect(),
            free: Mutex::new(Vec::new()),
            next: AtomicUsize::new(usize::MAX),
        };
        for _ in 0..3 {
            assert_eq!(registry.acquire(), (MAX_THREADS - 1, true));
        }
        assert_eq!(registry.next.load(Ordering::Relaxed), usize::MAX);
    }
}
