//! The same regression source is supplied for the pre-fix revision by
//! docs/repro/post-scan-race-before-fix.patch. No alternate reclaimer is copied
//! into this test: it pauses the actual implementation after its scan.

use super::Domain;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

struct Payload {
    value: u64,
    drops: Arc<AtomicUsize>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

#[test]
fn post_scan_retirement_must_not_destroy_a_pinned_object() {
    const TIMEOUT: Duration = Duration::from_secs(30);
    let domain = Domain::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let payload = Box::new(Payload {
        value: 0xfeed,
        drops: Arc::clone(&drops),
    });
    let root = AtomicPtr::new(Box::into_raw(payload));

    // Pre-register the reader so its idle slot is included in the scan. This
    // does not depend on racing registry growth or on the explicit handle API.
    drop(domain.pin());
    // Without seed garbage, advance would legitimately return before scanning.
    domain.retire(|| ());

    let (scanned_tx, scanned_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let (early_drops, pending_while_pinned, first_pass) = std::thread::scope(|scope| {
        let reclaimer_domain = &domain;
        let reclaimer = scope.spawn(move || {
            reclaimer_domain.advance_with(usize::MAX, || {
                // This hook is after ALL participant loads, but before the
                // garbage lock/extraction. No pins were live during that scan.
                scanned_tx.send(()).expect("reader abandoned the scan");
                resume_rx
                    .recv_timeout(TIMEOUT)
                    .expect("reader did not resume the reclaimer");
            })
        });
        scanned_rx
            .recv_timeout(TIMEOUT)
            .expect("reclaimer did not reach the post-scan pause");

        // Reader and unlinking writer share this thread, which is legal: the
        // reader's guard remains live across both the unlink and reclamation.
        let guard = domain.pin();
        let observed = root.load(Ordering::Acquire);
        assert!(!observed.is_null());
        // SAFETY: the published allocation has not been unlinked or retired. Never
        // dereference observed after resuming the potentially buggy reclaimer.
        assert_eq!(unsafe { (*observed).value }, 0xfeed);
        let removed = root.swap(core::ptr::null_mut(), Ordering::AcqRel);
        assert_eq!(removed, observed);
        // SAFETY: removed is exactly the Box::into_raw pointer. This thread is
        // the only unlinker, and this is its sole ownership reconstruction.
        let payload = unsafe { Box::from_raw(removed) };
        domain.retire(move || drop(payload));

        // No advance has completed yet: the new reader and new retirement are
        // still in epoch 1, but the paused scan computed min_pinned = 2.
        resume_tx
            .send(())
            .expect("reclaimer exited before extraction");
        let first_pass = reclaimer.join().expect("reclaimer panicked");
        let early_drops = drops.load(Ordering::Acquire);
        let pending_while_pinned = domain.pending();
        std::hint::black_box(&guard);
        // A broken implementation has freed observed already. Recording that
        // fact is enough; a deliberate use-after-free would make a bad test.
        drop(guard);
        (early_drops, pending_while_pinned, first_pass)
    });

    // Cleanup/progress check before reporting the safety failure. In the fixed
    // implementation this is the first pass allowed to destroy the payload.
    domain.advance();
    assert_eq!(
        drops.load(Ordering::Acquire),
        1,
        "payload must be destroyed exactly once"
    );
    assert_eq!(
        domain.pending(), 0,
        "quiescent cleanup must drain the queue"
    );
    assert_eq!(
        early_drops, 0,
        "retired object was destroyed while its reader guard was still live"
    );
    assert_eq!(
        pending_while_pinned, 1,
        "the reader's retirement must remain queued"
    );
    assert_eq!(first_pass, 1, "only the pre-scan seed may be reclaimed");
}
