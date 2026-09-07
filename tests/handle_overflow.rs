//! Isolated: shared overflow deliberately stops process-wide reclamation.
use ps_reclaim::{Domain, Handle, MAX_THREADS};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn independent_overflow_handles_share_a_count_not_a_pin() {
    let domain = Domain::new();
    let handles: Vec<_> = (0..MAX_THREADS + 2).map(|_| Handle::new()).collect();
    let first = domain.pin_with(&handles[MAX_THREADS]);
    let second = domain.pin_with(&handles[MAX_THREADS + 1]);
    let hits = Arc::new(AtomicUsize::new(0));
    let retired = Arc::clone(&hits);
    domain.retire(move || {
        retired.fetch_add(1, Ordering::Release);
    });
    drop(first);
    domain.advance();
    assert_eq!(hits.load(Ordering::Acquire), 0);
    drop(second);
    domain.advance();
    assert_eq!(hits.load(Ordering::Acquire), 1);
}
