//! Grace-period domains and their guards.

use crate::sync::GarbageMutex as Mutex;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use crate::registry::{
    DOMAIN_BITS, DOMAIN_MASK, NO_DOMAIN, PINS_PER_THREAD, Participant, Registry,
};

static NEXT_DOMAIN_ID: AtomicU64 = AtomicU64::new(1);
const MAX_EPOCH: u64 = u64::MAX >> DOMAIN_BITS;

const _: () = assert!(PINS_PER_THREAD <= u8::BITS as usize);

#[cold]
#[inline(never)]
fn pin_wildcard(participant: &Participant) -> usize {
    participant.wildcard.fetch_add(1, Ordering::Relaxed);
    usize::MAX
}

type Deferred = Box<dyn FnOnce() + Send + 'static>;

struct Retirement {
    sequence: u64,
    epoch: u64,
    run: Deferred,
}

struct Garbage {
    next_sequence: u64,
    entries: VecDeque<Retirement>,
}

/// If a callback unwinds, return the callbacks not yet invoked to their domain.
/// Keep original stamps: a concurrent scan must still respect its own cutoff.
struct ReclaimBatch<'a> {
    domain: &'a Domain,
    entries: VecDeque<Retirement>,
}

impl Drop for ReclaimBatch<'_> {
    fn drop(&mut self) {
        if !self.entries.is_empty() {
            crate::sync::garbage_lock(&self.domain.garbage)
                .entries
                .append(&mut self.entries);
        }
    }
}

/// One grace period.
///
/// Normal pins are domain-specific; wildcard overflow pins conservatively
/// delay all domains. The participant registry is process-wide.
pub struct Domain {
    id: u64,
    /// Advanced only by [`Domain::advance`], never on a read.
    epoch: AtomicU64,
    garbage: Mutex<Garbage>,
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Domain {
    /// Shallow: the garbage list holds closures, which do not print.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Domain")
            .field("id", &self.id)
            .field("epoch", &self.epoch.load(Ordering::Relaxed))
            .field("pending", &self.pending())
            .finish()
    }
}

impl Domain {
    /// A fresh domain with its own grace period.
    pub fn new() -> Self {
        Self {
            id: NEXT_DOMAIN_ID.fetch_add(1, Ordering::Relaxed),
            epoch: AtomicU64::new(1),
            garbage: Mutex::new(Garbage {
                next_sequence: 0,
                entries: VecDeque::new(),
            }),
        }
    }

    /// Pin the calling thread into this domain.
    ///
    /// Hold the returned guard across every load and dereference of a pointer
    /// this domain protects. Pin before loading a protected pointer; a pin
    /// cannot resurrect a pointer retired before the protected load.
    ///
    /// After registration, a normal pin uses a thread-local read, a relaxed
    /// epoch load, a store to its own padded slot, and a fence. Overflow pins
    /// instead increment a wildcard counter, shared on registry overflow.
    #[inline]
    pub fn pin(&self) -> Guard<'_> {
        // One thread-local lookup for the whole pin. The participant, the
        // shared-slot flag and the pin mask are fields of one `Local`.
        //
        // The order inside is exactly what it was when these were separate:
        // resolve the participant, then load the epoch, then claim an entry.
        let (p, entry) = crate::registry::with_local(|local| {
            let p = crate::registry::participant_in(local);
            let e = self.epoch.load(Ordering::Relaxed);
            let packed = (e << DOMAIN_BITS) | (self.id & DOMAIN_MASK);

            // Occupancy lives in a thread-local `Cell`, not in the slot, so the
            // fast path never loads the address it is about to store to.
            // Reading `pins[0]` first cost 2 ns: the fence cannot drain until
            // the store issues, and the store could not issue until that
            // same-address load resolved. `crossbeam` sidesteps it the same
            // way, by testing its `guard_count` rather than the epoch it is
            // about to write. A bit mask also makes a second-domain pin cheap:
            // WorkTable deliberately holds its page domain while looking
            // through an index domain.
            //
            // A zero mask means this thread holds no pin, so every entry is
            // free. A thread sharing the overflow slot must not touch `pins`:
            // those entries are not per-thread there, so another overflow
            // thread's guard drop would clear this pin and expose this reader
            // to reclamation. The wildcard is a count, so it composes across
            // however many threads share the slot, at the cost of stopping
            // reclamation entirely while any of them is pinned. Conservative,
            // and correct.
            let entry = if local.shared.get() {
                pin_wildcard(p)
            } else {
                let occupied = &local.pin_mask;
                let mask = occupied.get();
                if mask == 0 {
                    p.pins[0].store(packed, Ordering::Release);
                    occupied.set(1);
                    0
                } else {
                    let free = (!mask).trailing_zeros() as usize;
                    if free < PINS_PER_THREAD {
                        p.pins[free].store(packed, Ordering::Release);
                        occupied.set(mask | (1_u8 << free));
                        free
                    } else {
                        pin_wildcard(p)
                    }
                }
            };
            (p, entry)
        });

        // Release publication also carries preceding accesses across an
        // unpin/repin if a scanner observes the repin instead of the idle store.
        // Do not rely on a relaxed repin extending a prior release sequence.
        // Publish the pin before any protected pointer is loaded. Paired with
        // the fence in `advance`; without both, a reclaimer can read this slot
        // as idle while this thread goes on to load a pointer it is freeing.
        fence(Ordering::SeqCst);

        Guard::new(p, entry)
    }

    /// Retire `f`, to run once every reader pinned now has unpinned.
    ///
    /// Does not pin. Call it while holding the guard under which the pointer
    /// was unlinked.
    ///
    /// Callback order is unspecified. Callbacks should not panic. If one
    /// unwinds from `advance`, it is not retried, but callbacks not yet invoked
    /// are returned to the queue. During domain destruction there is no queue
    /// to return to: an unwinding callback drops the remaining captures without
    /// invoking their callbacks. Use non-panicking cleanup for owned resources.
    pub fn retire<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let e = self.epoch.load(Ordering::Relaxed);
        let run: Deferred = Box::new(f);
        let mut garbage = crate::sync::garbage_lock(&self.garbage);
        let sequence = garbage.next_sequence;
        // Never wrap: an old scan must not mistake new garbage for its batch.
        garbage.next_sequence = sequence
            .checked_add(1)
            .expect("retirement sequence exhausted");
        garbage.entries.push_back(Retirement {
            sequence,
            epoch: e,
            run,
        });
    }

    /// Retirements not yet run.
    ///
    /// This queue can grow without bound unless reclamation is driven.
    pub fn pending(&self) -> usize {
        crate::sync::garbage_lock(&self.garbage).entries.len()
    }

    /// Remaining epoch increments before conservative saturation. A diagnostic
    /// snapshot, not a reservation: concurrent advancement consumes headroom.
    /// Plan an exclusive maintenance window before this approaches zero.
    pub fn epoch_headroom(&self) -> u64 {
        MAX_EPOCH - self.epoch.load(Ordering::Relaxed)
    }

    /// Renew epoch headroom during an exclusive maintenance window.
    ///
    /// The mutable borrow excludes concurrent pins, retirements and scans,
    /// just as domain destruction excludes live guards. Existing retirements
    /// become older than every new pin, without invoking callbacks or changing
    /// their publication sequences. This takes O(pending) metadata work.
    /// It does not erase any registration or recover a forgotten guard's slot.
    ///
    /// This is NOT transparent rollover: arrange exclusive ownership before
    /// saturation. For an Arc-owned domain, workers must relinquish their Arc
    /// references before Arc::get_mut can provide this maintenance access.
    ///
    /// ```compile_fail
    /// let mut domain = ps_reclaim::Domain::new();
    /// let guard = domain.pin();
    /// domain.renew_epoch();
    /// drop(guard);
    /// ```
    pub fn renew_epoch(&mut self) {
        for retirement in &mut crate::sync::garbage_get_mut(&mut self.garbage).entries {
            retirement.epoch = 0;
        }
        *self.epoch.get_mut() = 1;
    }

    /// Run eligible retirements from a pre-scan snapshot. Returns how many ran.
    /// A nonempty, non-wildcard-blocked scan attempts to advance the epoch.
    /// Concurrent advancement or epoch saturation may prevent it.
    ///
    /// Never waits for readers, which is the property that matters and is not
    /// the same as never blocking: this takes the domain's own garbage lock,
    /// twice, so it is not lock-free. What it does not do is wait for a
    /// quiescent state. While a reader pinned before a retirement is still
    /// pinned, that retirement is simply not run yet. A reader that started
    /// in a later epoch does not hold it up. Readers in the retirement's
    /// epoch conservatively delay it, even if they started after retirement.
    /// Epochs saturate rather than wrapping unsafely. At saturation new garbage
    /// requires a scan without matching pins. Monitor [`Self::epoch_headroom`]
    /// and arrange [`Self::renew_epoch`] with exclusive access before that point.
    pub fn advance(&self) -> usize {
        self.advance_up_to(usize::MAX)
    }

    /// Run at most `limit` eligible retirements with the same epoch progression
    /// as [`Self::advance`]. A zero limit does nothing. Returns how many ran.
    ///
    /// This has the same reader-safety and non-blocking grace-period semantics
    /// as [`Self::advance`], but bounds the number of callbacks invoked.
    /// Eligible retirements beyond `limit` stay queued for a later pass. This
    /// does not bound scan time, allocation time, lock wait, or callback time.
    pub fn advance_up_to(&self, limit: usize) -> usize {
        self.advance_with(limit, || ())
    }

    // The hook permits deterministic scheduling of the post-scan race in unit
    // tests. The public path supplies a zero-sized no-op, with no runtime flag.
    fn advance_with(&self, limit: usize, after_scan: impl FnOnce()) -> usize {
        if limit == 0 {
            return 0;
        }

        // Nothing retired means nothing to reclaim, and the whole body below
        // exists only to decide what is safe to reclaim. Checking first turns
        // an advance on an empty domain from a full registry sweep plus two
        // allocations into one uncontended lock and a length read.
        //
        // This matters because callers advance far more often than they
        // retire: WorkTable calls it on every mutation of a versioned page,
        // and the common case is that the previous call already drained.
        let cutoff = {
            let garbage = crate::sync::garbage_lock(&self.garbage);
            if garbage.entries.is_empty() {
                return 0;
            }
            garbage.next_sequence
        };

        // The mutex orders every retirement below cutoff before this fence.
        // Later retirements are excluded even if they have the same epoch.
        // Concurrent scans may remove entries, so a Vec length is not a safe
        // boundary; sequence numbers remain meaningful after such removals.

        // Paired with the fence in `pin`.
        fence(Ordering::SeqCst);

        let now = self.epoch.load(Ordering::Relaxed);
        // No reader pinned here means nothing retired so far can be reached,
        // so everything is safe. Without this an idle domain needs two
        // advances to free anything, because a retirement stamped with the
        // current epoch is not strictly less than it.
        let mut min_pinned = now + 1;
        // Only slots that have ever been leased. A 256-slot registry scanned
        // in full is 1024 `Acquire` loads across 256 cache lines (each
        // `Participant` is `repr(align(128))`), which is 32 KiB of traffic to
        // read what is, in a process with eight threads, eight live slots.
        for p in Registry::get().active_slots() {
            if p.wildcard.load(Ordering::Acquire) != 0 {
                // Someone is conservatively pinned everywhere.
                return 0;
            }
            for i in 0..PINS_PER_THREAD {
                let packed = p.pins[i].load(Ordering::Acquire);
                if packed != NO_DOMAIN && (packed & DOMAIN_MASK) == (self.id & DOMAIN_MASK) {
                    min_pinned = min_pinned.min(packed >> DOMAIN_BITS);
                }
            }
        }

        after_scan();
        let mut expired = ReclaimBatch {
            domain: self,
            entries: VecDeque::new(),
        };
        {
            let mut garbage = crate::sync::garbage_lock(&self.garbage);
            // Strictly less than: something retired in the same epoch a reader
            // pinned in may still be reachable by that reader.
            //
            // Pop/rotate, never compact the unchecked tail. Vec::extract_if
            // followed by take(k) moved N-k records on iterator destruction,
            // making a ready backlog cost quadratic work to drain in batches.
            // Inspect each initially queued entry at most once. Rotation also
            // permits out-of-order epochs and callbacks requeued after unwind.
            let queued = garbage.entries.len();
            for _ in 0..queued {
                if expired.entries.len() == limit {
                    break;
                }
                let retirement = garbage.entries.pop_front().expect("snapshot entry");
                if retirement.sequence < cutoff && retirement.epoch < min_pinned {
                    expired.entries.push_back(retirement);
                } else {
                    garbage.entries.push_back(retirement);
                }
            }
        }

        // Outside the lock: a retirement may retire more.
        let n = expired.entries.len();
        while let Some(retirement) = expired.entries.pop_front() {
            (retirement.run)();
        }
        // Only 40 epoch bits fit in a pin. Keep saturation conservative. A
        // reader can pause between sampling an epoch and publishing its pin;
        // modular decoding plus a scan-based age cap is NOT sufficient for
        // safe rollover. Renewal instead requires exclusive domain ownership.
        // CAS avoids repeatedly advancing on behalf of a stale concurrent scan.
        let _ = self.epoch.compare_exchange(
            now,
            now.saturating_add(1).min(MAX_EPOCH),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        n
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // Exclusive access, so no reader can be pinned here.
        let garbage = core::mem::take(&mut crate::sync::garbage_get_mut(&mut self.garbage).entries);
        for retirement in garbage {
            (retirement.run)();
        }
    }
}

/// A guard is one word: the participant pointer with its pin entry packed
/// into the low bits.
///
/// `Participant` is `#[repr(align(128))]`, so the low seven bits of any
/// pointer to one are zero. An entry index needs three of them.
///
/// The size is not incidental. A guard's cost is not only what it takes to
/// create: a caller that returns one from a hot lookup pays for its size on
/// every call. WorkTable's `partition_ref` builds a `PartRef { guard, &T }`
/// and drops it per lookup, and at three words that measured 3.6 ns against
/// 3.2 ns for the single-word `crossbeam-epoch` guard it replaced, even
/// though this crate's *pin* is the faster of the two in isolation.
const ENTRY_MASK: usize = 0b111;

/// The wildcard tag. Entries are stored as `entry + 1`, leaving zero free to
/// mean "no entry", which is what a thread past [`PINS_PER_THREAD`] gets.
const WILDCARD_TAG: usize = 0;

// If the entry count ever outgrows the tag, the packing is silently wrong,
// so refuse to compile instead.
const _: () = assert!(PINS_PER_THREAD < ENTRY_MASK);
const _: () = assert!(align_of::<Participant>() > ENTRY_MASK);

/// Holds a thread's pin open. Dropping it releases the pin.
///
/// Not `Send`, and the marker below is what enforces it. A guard names the
/// slot of the thread that took it, so dropping one on another thread would
/// store `NO_DOMAIN` into the *original* thread's slot while that thread is
/// still reading, and would clear the wrong thread-local occupancy bit. Both
/// are use-after-free windows.
///
/// 0.1.0 documented this and did not enforce it: every field was `Send`, so
/// the auto trait applied and the sentence above was decoration. This test is
/// the enforcement.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<ps_reclaim::Guard<'static>>();
/// ```
#[must_use = "a dropped guard no longer protects reader accesses"]
pub struct Guard<'a> {
    /// The participant pointer, with this guard's pin entry in its low bits.
    ///
    /// A raw pointer rather than a `usize` for two reasons: it keeps the
    /// provenance (a `usize` round trip is rejected under Miri's strict
    /// provenance mode), and it makes the guard `!Send` and `!Sync` by
    /// construction rather than by a marker someone can delete.
    packed: *const Participant,
    /// Borrows the domain without storing it. Keeping a `&Domain` field
    /// would double the size for the sake of two convenience methods; a
    /// caller that wants to retire has the domain in hand already, because
    /// it needed one to pin.
    domain: PhantomData<&'a Domain>,
}

impl Guard<'_> {
    #[inline]
    fn new(participant: &'static Participant, entry: usize) -> Self {
        let tag = if entry == usize::MAX {
            WILDCARD_TAG
        } else {
            entry + 1
        };
        Self {
            packed: (participant as *const Participant).map_addr(|addr| addr | tag),
            domain: PhantomData,
        }
    }

    #[inline]
    fn participant(&self) -> &'static Participant {
        // Safety: `packed` was built from a `&'static Participant` whose
        // alignment guarantees the masked bits were zero, so this restores
        // exactly the pointer that went in.
        unsafe { &*self.packed.map_addr(|addr| addr & !ENTRY_MASK) }
    }

    /// This guard's pin entry, or `None` for the wildcard.
    #[inline]
    fn entry(&self) -> Option<usize> {
        match self.packed.addr() & ENTRY_MASK {
            WILDCARD_TAG => None,
            tag => Some(tag - 1),
        }
    }
}

impl Drop for Guard<'_> {
    #[inline]
    fn drop(&mut self) {
        let p = self.participant();
        match self.entry() {
            None => {
                p.wildcard.fetch_sub(1, Ordering::Release);
            }
            Some(entry) => {
                // Release, so a reclaimer that sees the slot free also sees every
                // access this reader made while pinned.
                p.pins[entry].store(NO_DOMAIN, Ordering::Release);
                // The second and last thread-local lookup of a pin/unpin cycle.
                let _ = crate::registry::try_with_local(|local| {
                    // A platform TLS destructor may have destroyed the old
                    // Local and a later destructor may have registered afresh.
                    // Never clear that new registration's occupancy bits.
                    if local.mine.get().is_some_and(|mine| core::ptr::eq(mine, p)) {
                        let occupied = &local.pin_mask;
                        occupied.set(occupied.get() & !(1_u8 << entry));
                    }
                });
            }
        }
    }
}

/// One caller-owned registration, reusable across every domain on a thread.
///
/// Construct once outside the hot path and use [`Domain::pin_with`] for reads.
/// Each live handle consumes one registry slot, regardless of domain count.
/// Multiple handles and the implicit TLS registration consume separate slots;
/// after 255 exclusive registrations, pins use the shared wildcard slot and
/// delay reclamation in every domain. Prefer one handle per worker.
///
/// Both this registration and its guards are `!Send` and `!Sync`.
///
/// ```
/// use ps_reclaim::{Domain, Handle};
/// let registration = Handle::new();
/// let a = Domain::new();
/// let b = Domain::new();
/// let first = a.pin_with(&registration);
/// let second = b.pin_with(&registration);
/// drop(first); // Out-of-order guard drops are supported.
/// drop(second);
/// ```
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<ps_reclaim::Handle>();
/// ```
/// ```compile_fail
/// fn sync<T: Sync>() {}
/// sync::<ps_reclaim::Handle>();
/// ```
pub struct Handle {
    participant: &'static Participant,
    shared: bool,
    /// The same mask `pin` keeps in thread-local storage, kept here instead.
    pin_mask: core::cell::Cell<u8>,
    lease: Option<crate::registry::SlotLease>,
    _not_send: PhantomData<*const ()>,
}

impl Default for Handle {
    fn default() -> Self {
        Self::new()
    }
}

impl Handle {
    /// Acquire one registry slot. This may lock and initialize the registry;
    /// keep it outside latency-sensitive read loops.
    pub fn new() -> Self {
        let registry = Registry::get();
        let (idx, shared) = registry.acquire();
        Handle {
            participant: &registry.slots[idx],
            shared,
            pin_mask: core::cell::Cell::new(0),
            lease: Some(crate::registry::SlotLease(idx, shared)),
            _not_send: PhantomData,
        }
    }
}

impl Domain {
    /// Pin through a reusable registration without looking up TLS.
    ///
    /// The guard borrows both the domain and registration. It must cover every
    /// protected pointer load and dereference, just as with [`Self::pin`].
    ///
    /// ```compile_fail
    /// use ps_reclaim::{Domain, Handle};
    /// let domain = Domain::new();
    /// let registration = Handle::new();
    /// let guard = domain.pin_with(&registration);
    /// drop(registration);
    /// drop(guard);
    /// ```
    /// ```compile_fail
    /// use ps_reclaim::{Domain, Handle};
    /// let domain = Domain::new();
    /// let registration = Handle::new();
    /// let guard = domain.pin_with(&registration);
    /// drop(domain);
    /// drop(guard);
    /// ```
    #[inline]
    pub fn pin_with<'h, 'd>(&'d self, handle: &'h Handle) -> HandleGuard<'h, 'd> {
        let p = handle.participant;
        let e = self.epoch.load(Ordering::Relaxed);
        let packed = (e << DOMAIN_BITS) | (self.id & DOMAIN_MASK);
        let entry = if handle.shared {
            pin_wildcard(p)
        } else {
            let mask = handle.pin_mask.get();
            if mask == 0 {
                p.pins[0].store(packed, Ordering::Release);
                handle.pin_mask.set(1);
                0
            } else {
                let free = (!mask).trailing_zeros() as usize;
                if free < PINS_PER_THREAD {
                    p.pins[free].store(packed, Ordering::Release);
                    handle.pin_mask.set(mask | (1_u8 << free));
                    free
                } else {
                    pin_wildcard(p)
                }
            }
        };
        fence(Ordering::SeqCst);
        HandleGuard {
            handle,
            entry,
            domain: PhantomData,
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        drop(self.lease.take());
    }
}

/// A pin taken through a [`Handle`].
///
/// This is two words (registration reference and entry), unlike the packed
/// one-word [`Guard`]. Benchmark returned read views as well as pin/drop loops
/// before assuming their calling-convention costs are interchangeable.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<ps_reclaim::HandleGuard<'static, 'static>>();
/// ```
/// ```compile_fail
/// fn sync<T: Sync>() {}
/// sync::<ps_reclaim::HandleGuard<'static, 'static>>();
/// ```
#[must_use = "a dropped guard no longer protects reader accesses"]
pub struct HandleGuard<'h, 'd> {
    handle: &'h Handle,
    entry: usize,
    domain: PhantomData<&'d Domain>,
}

impl Drop for HandleGuard<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        let p = self.handle.participant;
        if self.entry == usize::MAX {
            p.wildcard.fetch_sub(1, Ordering::Release);
        } else {
            p.pins[self.entry].store(NO_DOMAIN, Ordering::Release);
            let m = &self.handle.pin_mask;
            m.set(m.get() & !(1_u8 << self.entry));
        }
    }
}

#[cfg(test)]
#[path = "domain/post_scan_race.rs"]
mod post_scan_race;

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn saturated_epoch_still_protects_readers() {
        let domain = Domain::new();
        domain.epoch.store(MAX_EPOCH, Ordering::Relaxed);
        let registration = Handle::new();
        let guard = domain.pin_with(&registration);
        let freed = Arc::new(AtomicBool::new(false));
        let retired = Arc::clone(&freed);
        domain.retire(move || retired.store(true, Ordering::Release));
        for _ in 0..4 {
            domain.advance();
        }
        assert_eq!(domain.epoch.load(Ordering::Relaxed), MAX_EPOCH);
        assert!(!freed.load(Ordering::Acquire));
        drop(guard);
        domain.advance();
        assert!(freed.load(Ordering::Acquire));
    }

    #[test]
    fn exclusive_renewal_restores_headroom_and_preserves_pending_work() {
        let mut domain = Domain::new();
        domain.epoch.store(MAX_EPOCH, Ordering::Relaxed);
        assert_eq!(domain.epoch_headroom(), 0);
        let freed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&freed);
        domain.retire(move || flag.store(true, Ordering::Release));
        domain.renew_epoch();
        assert_eq!(domain.epoch_headroom(), MAX_EPOCH - 1);
        assert_eq!(domain.pending(), 1);
        assert!(
            !freed.load(Ordering::Acquire),
            "renewal must not invoke callbacks"
        );
        {
            let garbage = crate::sync::garbage_lock(&domain.garbage);
            assert_eq!(garbage.next_sequence, 1);
            assert_eq!(garbage.entries[0].sequence, 0);
            assert_eq!(garbage.entries[0].epoch, 0);
        }
        let fresh = domain.pin();
        assert_eq!(domain.advance(), 1, "new reader delayed pre-renewal work");
        assert!(freed.load(Ordering::Acquire));
        drop(fresh);
    }

    #[test]
    fn stale_scan_does_not_advance_a_newer_epoch() {
        let domain = Domain::new();
        domain.retire(|| ());
        domain.advance_with(1, || {
            assert_eq!(domain.advance(), 1);
            assert_eq!(domain.epoch.load(Ordering::Relaxed), 2);
        });
        assert_eq!(domain.epoch.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn bounded_ready_batch_does_not_move_the_unchecked_tail() {
        let domain = Domain::new();
        for _ in 0..4096 {
            domain.retire(|| ());
        }
        let tail = {
            let garbage = crate::sync::garbage_lock(&domain.garbage);
            &garbage.entries[8] as *const Retirement
        };
        assert_eq!(domain.advance_up_to(8), 8);
        let garbage = crate::sync::garbage_lock(&domain.garbage);
        assert_eq!(garbage.entries.len(), 4088);
        assert!(
            core::ptr::eq(&garbage.entries[0], tail),
            "unchecked records moved"
        );
    }

    #[test]
    fn blocked_prefix_does_not_hide_an_older_eligible_retirement() {
        let domain = Domain::new();
        domain.epoch.store(2, Ordering::Relaxed);
        let held = domain.pin();
        domain.retire(|| ()); // blocked epoch-2 prefix
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        domain.retire(move || flag.store(true, Ordering::Release));
        // Model a writer that sampled epoch 1, paused, and enqueued after the
        // epoch-2 retirement. Publication sequence and epoch need not agree.
        crate::sync::garbage_lock(&domain.garbage)
            .entries
            .back_mut()
            .unwrap()
            .epoch = 1;
        assert_eq!(domain.advance_up_to(1), 1);
        assert!(ran.load(Ordering::Acquire));
        assert_eq!(domain.pending(), 1);
        drop(held);
        assert_eq!(domain.advance(), 1);
    }

    #[test]
    fn callback_unwind_requeues_uninvoked_work_with_original_sequence() {
        let domain = Domain::new();
        let ran = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        domain.retire(|| panic!("expected callback panic"));
        for _ in 0..3 {
            let ran = Arc::clone(&ran);
            domain.retire(move || {
                ran.fetch_add(1, Ordering::Relaxed);
            });
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| domain.advance()));
        assert!(result.is_err());
        assert_eq!(domain.pending(), 3);
        assert_eq!(
            crate::sync::garbage_lock(&domain.garbage).entries[0].sequence,
            1
        );
        assert_eq!(domain.advance(), 3);
        assert_eq!(ran.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_post_scan_retirement_waits_for_its_reader() {
        let domain = Domain::new();
        let registration = Handle::new();
        // Ensure the initial garbage check does not skip this scan.
        domain.retire(|| ());
        let freed = Arc::new(AtomicBool::new(false));
        let held = RefCell::new(None);
        domain.advance_with(usize::MAX, || {
            // Deterministically enter after every participant was scanned.
            let guard = domain.pin_with(&registration);
            let freed = Arc::clone(&freed);
            domain.retire(move || freed.store(true, Ordering::Release));
            *held.borrow_mut() = Some(guard);
        });
        assert!(!freed.load(Ordering::Acquire));
        domain.advance();
        assert!(!freed.load(Ordering::Acquire));
        drop(held.borrow_mut().take());
        domain.advance();
        assert!(freed.load(Ordering::Acquire));
    }
}
