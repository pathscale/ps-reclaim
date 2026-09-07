//! A guard in an older TLS destructor can outlive the cached slot's lease.
use ps_reclaim::{Domain, Guard, Handle, MAX_THREADS, registry_stats, slots_handed_out};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct LateReader {
    domain: &'static Domain,
    _guard: Guard<'static>,
    freed: Arc<AtomicBool>,
    early_free: Arc<AtomicBool>,
}

impl Drop for LateReader {
    fn drop(&mut self) {
        // Reusing and clearing the dying TLS registration here used to erase
        // this reader's still-live pin. Avoid panicking in a TLS destructor.
        let other = Handle::new();
        drop(self.domain.pin_with(&other));
        self.domain.advance();
        self.early_free
            .store(self.freed.load(Ordering::Acquire), Ordering::Release);
    }
}

std::thread_local! {
    static LATE: RefCell<Option<LateReader>> = const { RefCell::new(None) };
}

#[test]
fn a_guard_outliving_its_tls_lease_remains_visible() {
    // A static owner outlives every joined thread without an unreachable leaked
    // Box. Miri can keep allocation-leak checking enabled for this fixture too.
    static DOMAIN: std::sync::OnceLock<Domain> = std::sync::OnceLock::new();
    let domain = DOMAIN.get_or_init(Domain::new);
    let rounds = if cfg!(miri) { 8 } else { MAX_THREADS * 2 };
    for _ in 0..rounds {
        let freed = Arc::new(AtomicBool::new(false));
        let early_free = Arc::new(AtomicBool::new(false));
        let reader_freed = Arc::clone(&freed);
        let reader_early = Arc::clone(&early_free);
        std::thread::spawn(move || {
            // Register LATE's destructor before the registry lease destructor.
            LATE.with(|late| {
                let guard = domain.pin();
                let retired = Arc::clone(&reader_freed);
                domain.retire(move || {
                    retired.store(true, Ordering::Release);
                });
                *late.borrow_mut() = Some(LateReader {
                    domain,
                    _guard: guard,
                    freed: reader_freed,
                    early_free: reader_early,
                });
            });
        })
        .join()
        .unwrap();
        assert!(!early_free.load(Ordering::Acquire));
        domain.advance();
        assert!(freed.load(Ordering::Acquire));
        let stats = registry_stats();
        assert_eq!(stats.pinned_quarantined, 0);
        assert_eq!(stats.available_exclusive, MAX_THREADS - 1);
    }
    // The destructor's temporary explicit handle needs a second slot, but
    // repeated lease-before-guard teardown must not consume new ones forever.
    assert!(slots_handed_out() <= 2);
}
