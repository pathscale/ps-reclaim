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
