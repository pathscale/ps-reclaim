//! The grace-period domain and its guard.

use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering, fence};

use crate::registry::{
    DOMAIN_BITS, DOMAIN_MASK, NO_DOMAIN, PINS_PER_THREAD, Participant, Registry, participant,
};

static NEXT_DOMAIN_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// Occupied entries in this thread's participant slot. Guards are `!Send`,
    /// so this is exact rather than a hint and handles out-of-order drops.
    static PIN_MASK: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}
const _: () = assert!(PINS_PER_THREAD <= u8::BITS as usize);

#[cold]
#[inline(never)]
fn pin_wildcard(participant: &Participant) -> usize {
    participant.wildcard.fetch_add(1, Ordering::Relaxed);
    usize::MAX
}

type Deferred = Box<dyn FnOnce() + Send + 'static>;

/// One grace period.
///
/// Readers of this domain never delay reclamation in another, so a slow scan
/// over one structure cannot stall an unrelated one. A domain is a few words:
/// the participant registry behind it is process-wide.
pub struct Domain {
    id: u64,
    /// Advanced only by [`Domain::advance`], never on a read.
    epoch: AtomicU64,
    garbage: Mutex<Vec<(u64, Deferred)>>,
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Domain {
    /// Shallow: the garbage list holds closures, which do not print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
            garbage: Mutex::new(Vec::new()),
        }
    }

    /// Pin the calling thread into this domain.
    ///
    /// Hold the returned guard across every load and dereference of a pointer
    /// this domain protects. Nothing retired before the pin is taken can be
    /// reclaimed while it is held.
    ///
    /// The whole hot path: a thread-local read, a relaxed load of this
    /// domain's epoch, one store into this thread's own cache line, and one
    /// fence. No shared line is written.
    #[inline]
    pub fn pin(&self) -> Guard<'_> {
        let p = participant();
        let e = self.epoch.load(Ordering::Relaxed);
        let packed = (e << DOMAIN_BITS) | (self.id & DOMAIN_MASK);

        // Occupancy lives in a thread-local `Cell`, not in the slot, so the fast
        // path never loads the address it is about to store to. Reading
        // `pins[0]` first cost 2 ns: the fence cannot drain until the store
        // issues, and the store could not issue until that same-address load
        // resolved. `crossbeam` sidesteps it the same way, by testing its
        // `guard_count` rather than the epoch it is about to write. A bit mask
        // also makes a second-domain pin cheap: WorkTable deliberately holds
        // its page domain while looking through an index domain.
        //
        // A zero mask means this thread holds no pin, so every entry is free.
        // A thread sharing the overflow slot must not touch `pins`: those
        // entries are not per-thread there, so another overflow thread's guard
        // drop would clear this pin and expose this reader to reclamation. The
        // wildcard is a count, so it composes across however many threads
        // share the slot, at the cost of stopping reclamation entirely while
        // any of them is pinned. Conservative, and correct.
        let entry = if crate::registry::is_shared_slot() {
            pin_wildcard(p)
        } else {
            PIN_MASK.with(|occupied| {
                let mask = occupied.get();
                if mask == 0 {
                    p.pins[0].store(packed, Ordering::Relaxed);
                    occupied.set(1);
                    return 0;
                }
                let free = (!mask).trailing_zeros() as usize;
                if free < PINS_PER_THREAD {
                    p.pins[free].store(packed, Ordering::Relaxed);
                    occupied.set(mask | (1_u8 << free));
                    free
                } else {
                    pin_wildcard(p)
                }
            })
        };

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
    pub fn retire<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let e = self.epoch.load(Ordering::Relaxed);
        self.garbage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((e, Box::new(f)));
    }

    /// Retirements not yet run.
    ///
    /// The only unbounded thing here: nothing drains without [`Self::advance`].
    pub fn pending(&self) -> usize {
        self.garbage.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Run every retirement whose grace period has expired, then advance the
    /// epoch one step. Returns how many ran.
    ///
    /// Never waits for readers, which is the property that matters and is not
    /// the same as never blocking: this takes the domain's own garbage lock,
    /// twice, so it is not lock-free. What it does not do is wait for a
    /// quiescent state. While a reader pinned before a retirement is still
    /// pinned, that retirement is simply not run yet. A reader that started
    /// *after* it does not hold it up, which is what lets reclamation progress
    /// under continuous read traffic.
    pub fn advance(&self) -> usize {
        self.advance_up_to(usize::MAX)
    }

    /// Run at most `limit` retirements whose grace period has expired, then
    /// advance the epoch one step. Returns how many ran.
    ///
    /// This has the same reader-safety and non-blocking grace-period semantics
    /// as [`Self::advance`], but bounds destructor work charged to the caller.
    /// Eligible retirements beyond `limit` stay queued for a later pass.
    pub fn advance_up_to(&self, limit: usize) -> usize {
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
        if self
            .garbage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return 0;
        }

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

        let expired: Vec<Deferred> = {
            let mut garbage = self.garbage.lock().unwrap_or_else(|e| e.into_inner());
            // Strictly less than: something retired in the same epoch a reader
            // pinned in may still be reachable by that reader.
            //
            // `extract_if` drains in place. The previous `partition` moved the
            // whole queue into two fresh `Vec`s and wrote one back on every
            // call, so a domain holding a long backlog paid for the backlog on
            // each advance even when nothing had expired. Retirements are
            // pushed in epoch order, so this preserves order in both halves.
            garbage
                .extract_if(.., |(e, _)| *e < min_pinned)
                .take(limit)
                .map(|(_, f)| f)
                .collect()
        };

        // Outside the lock: a retirement may retire more.
        let n = expired.len();
        for f in expired {
            f();
        }
        self.epoch.fetch_add(1, Ordering::Relaxed);
        n
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // Exclusive access, so no reader can be pinned here.
        let garbage =
            std::mem::take(&mut *self.garbage.get_mut().unwrap_or_else(|e| e.into_inner()));
        for (_, f) in garbage {
            f();
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
                PIN_MASK.with(|occupied| {
                    occupied.set(occupied.get() & !(1_u8 << entry));
                });
            }
        }
    }
}
