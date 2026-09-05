//! The three guarantees from the crate docs, one test each.
//!
//! These define the contract. A reclamation scheme that fails any of them is
//! not a substitute for this crate, whatever else it offers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use ps_reclaim::Domain;

/// A thread whose pin can be opened and closed on command, so a test can
/// arrange overlaps that a single thread cannot express.
struct RemoteReader {
    cmd: Option<mpsc::Sender<bool>>,
    ack: mpsc::Receiver<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RemoteReader {
    fn spawn(domain: Arc<Domain>) -> Self {
        let (cmd, rx) = mpsc::channel::<bool>();
        let (tx, ack) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let mut held = None;
            while let Ok(pin) = rx.recv() {
                held = if pin { Some(domain.pin()) } else { None };
                if tx.send(()).is_err() {
                    break;
                }
            }
            drop(held);
        });
        Self {
            cmd: Some(cmd),
            ack,
            handle: Some(handle),
        }
    }

    fn pin(&self) {
        self.cmd.as_ref().unwrap().send(true).unwrap();
        self.ack.recv().unwrap();
    }

    fn unpin(&self) {
        self.cmd.as_ref().unwrap().send(false).unwrap();
        self.ack.recv().unwrap();
    }
}

impl Drop for RemoteReader {
    fn drop(&mut self) {
        self.cmd.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn drive(domain: &Domain, hits: &AtomicUsize, want: usize) {
    for _ in 0..64 {
        domain.advance();
        if hits.load(Ordering::SeqCst) >= want {
            return;
        }
    }
}

/// Guarantee 1: a retirement does not run while a reader older than it lives.
#[test]
fn a_reader_older_than_the_retirement_holds_it() {
    let domain = Arc::new(Domain::new());
    let hits = Arc::new(AtomicUsize::new(0));
    let reader = RemoteReader::spawn(domain.clone());

    reader.pin();
    let h = hits.clone();
    domain.retire(move || {
        h.fetch_add(1, Ordering::SeqCst);
    });

    drive(&domain, &hits, 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "ran a retirement while a reader that predates it was still pinned"
    );

    reader.unpin();
    drive(&domain, &hits, 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "never ran after the reader left"
    );
}

/// Guarantee 2, the one that separates schemes: reclamation progresses even
/// though there is never an instant with zero readers.
///
/// A reference-counted scheme waits for a quiescent instant and so reclaims
/// nothing at all under continuous read traffic. `seize` fails this; so does a
/// plain global reader counter.
#[test]
fn a_reader_newer_than_the_retirement_does_not_hold_it() {
    let domain = Arc::new(Domain::new());
    let hits = Arc::new(AtomicUsize::new(0));

    let first = RemoteReader::spawn(domain.clone());
    let second = RemoteReader::spawn(domain.clone());

    first.pin();
    let h = hits.clone();
    domain.retire(move || {
        h.fetch_add(1, Ordering::SeqCst);
    });

    // Hand over so that at every instant at least one reader is pinned.
    second.pin();
    first.unpin();
    domain.advance();
    first.pin();
    second.unpin();

    drive(&domain, &hits, 1);
    let during_overlap = hits.load(Ordering::SeqCst);
    first.unpin();

    assert_eq!(
        during_overlap, 1,
        "reclamation must progress although readers never stopped overlapping"
    );
}

/// Guarantee 3: only `advance` runs retirements, so a read never pays for
/// someone else's garbage.
#[test]
fn nothing_is_reclaimed_without_advance() {
    let domain = Domain::new();
    let hits = Arc::new(AtomicUsize::new(0));

    for _ in 0..1000 {
        let h = hits.clone();
        domain.retire(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
    }

    // Plenty of read traffic, no maintenance.
    for _ in 0..10_000 {
        let g = domain.pin();
        std::hint::black_box(&g);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "a read ran a retirement");
    assert_eq!(domain.pending(), 1000);

    domain.advance();
    assert_eq!(hits.load(Ordering::SeqCst), 1000, "advance must run them");
    assert_eq!(domain.pending(), 0);
}

#[test]
fn bounded_advance_never_runs_more_than_the_requested_limit() {
    let domain = Domain::new();
    let hits = Arc::new(AtomicUsize::new(0));

    for _ in 0..23 {
        let h = hits.clone();
        domain.retire(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
    }

    assert_eq!(domain.advance_up_to(7), 7);
    assert_eq!(hits.load(Ordering::SeqCst), 7);
    assert_eq!(domain.pending(), 16);

    assert_eq!(domain.advance_up_to(7), 7);
    assert_eq!(hits.load(Ordering::SeqCst), 14);
    assert_eq!(domain.pending(), 9);

    assert_eq!(domain.advance(), 9);
    assert_eq!(hits.load(Ordering::SeqCst), 23);
    assert_eq!(domain.pending(), 0);
}

/// Domains are independent: a reader of one does not delay the other.
#[test]
fn domains_do_not_delay_each_other() {
    let a = Arc::new(Domain::new());
    let b = Domain::new();
    let hits = Arc::new(AtomicUsize::new(0));

    let reader_of_a = RemoteReader::spawn(a.clone());
    reader_of_a.pin();

    let h = hits.clone();
    b.retire(move || {
        h.fetch_add(1, Ordering::SeqCst);
    });
    drive(&b, &hits, 1);

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "a reader pinned in another domain delayed this one"
    );
    reader_of_a.unpin();
}

/// Dropping a domain runs whatever is left: exclusive access proves no reader.
#[test]
fn dropping_the_domain_runs_the_remainder() {
    let hits = Arc::new(AtomicUsize::new(0));
    {
        let domain = Domain::new();
        for _ in 0..8 {
            let h = hits.clone();
            domain.retire(move || {
                h.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        8,
        "dropping leaked retirements"
    );
}

/// A guard is one word.
///
/// Not a style preference: `partition_ref` in WorkTable returns a
/// `PartRef { guard, &T }` and drops it on every lookup, so the guard's size
/// is paid per call. At three words that cost more than this crate's cheaper
/// pin saved, and the lookup measured slower than the `crossbeam-epoch` one
/// it replaced despite winning every isolated pin benchmark.
#[test]
fn a_guard_is_one_word() {
    assert_eq!(
        std::mem::size_of::<ps_reclaim::Guard<'_>>(),
        std::mem::size_of::<usize>(),
        "a guard grew; anything returning one from a hot path pays for it per call"
    );
}

/// A retirement made on one thread must be runnable from another.
///
/// Ported from WorkTable's `markers_flushed_on_another_thread_are_collectable_here`,
/// which was deleted along with the epoch module this crate replaces. The
/// property is not implementation detail: a scheme that parks retirements in
/// thread-local state and reclaims them only from the retiring thread will pass
/// every single-threaded test here and then leak in a server, where the thread
/// that retired a page is idle and some other thread runs maintenance.
///
/// The retiring thread deliberately stays *alive and unpinned* while the main
/// thread advances. Letting it exit would prove something weaker, because a
/// scheme could flush on thread teardown and still be wrong for a live pool.
#[test]
fn a_retirement_from_another_thread_runs_here() {
    let domain = Arc::new(Domain::new());
    let hits = Arc::new(AtomicUsize::new(0));

    let (retired_tx, retired) = mpsc::channel();
    let (release, wait_for_release) = mpsc::channel::<()>();
    let d = domain.clone();
    let h = hits.clone();
    let thread = std::thread::spawn(move || {
        d.retire(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        retired_tx.send(()).unwrap();
        let _ = wait_for_release.recv();
    });
    retired.recv().unwrap();

    drive(&domain, &hits, 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "a retirement made on another thread was not reachable from this one"
    );

    release.send(()).unwrap();
    thread.join().unwrap();
}

/// Using many domains must not disturb a guard held on one of them.
///
/// Ported from WorkTable's `cache_rotation_keeps_pinning_sound`. That test
/// registered more domains than a per-thread handle cache could hold and
/// checked that evicting the cached handle did not silently unpin the live
/// guard. This crate has no such cache, so the mechanism is gone - but the
/// property it protected is not implementation-specific: whatever per-thread
/// bookkeeping a scheme keeps, exercising unrelated domains must never make a
/// held guard stop counting.
///
/// It is worth keeping precisely because the failure is silent. Nothing
/// crashes; a reader simply stops holding its retirement back, and the memory
/// is freed underneath it.
#[test]
fn many_domains_do_not_disturb_a_live_guard() {
    let domains: Vec<Domain> = (0..64).map(|_| Domain::new()).collect();
    let hits = Arc::new(AtomicUsize::new(0));

    // A live guard on the first domain, held across everything below.
    let held = domains[0].pin();
    let h = hits.clone();
    domains[0].retire(move || {
        h.fetch_add(1, Ordering::SeqCst);
    });

    // Churn every other domain: pin, advance, release. Each acquires and frees
    // whatever per-thread state the scheme uses.
    for d in &domains[1..] {
        let g = d.pin();
        std::hint::black_box(&g);
        drop(g);
        d.advance();
    }

    drive(&domains[0], &hits, 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "churning unrelated domains released a guard that was still held"
    );

    drop(held);
    drive(&domains[0], &hits, 1);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the retirement never ran once the guard was dropped"
    );
}
