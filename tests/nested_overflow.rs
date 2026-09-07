//! Isolated process: a wildcard intentionally blocks every domain and must not
//! race tests asserting immediate progress in another domain.
use ps_reclaim::{Domain, MAX_NESTED_PINS};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn nested_overflow_alone_protects_its_domain_and_releases() {
    let ordinary = Domain::new();
    let overflow_domain = Domain::new();
    let normal: Vec<_> = (0..MAX_NESTED_PINS).map(|_| ordinary.pin()).collect();
    let overflow = overflow_domain.pin();
    let ran = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ran);
    overflow_domain.retire(move || {
        counter.fetch_add(1, Ordering::Release);
    });
    // Release every ordinary entry. The wildcard is now the ONLY protection
    // of overflow_domain; same-domain normal pins cannot mask a broken fallback.
    drop(normal);
    for _ in 0..4 {
        overflow_domain.advance();
    }
    assert_eq!(ran.load(Ordering::Acquire), 0);
    drop(overflow);
    assert_eq!(overflow_domain.advance(), 1);
    assert_eq!(ran.load(Ordering::Acquire), 1);
}
