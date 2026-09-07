//! Registration ownership and reclamation through the explicit API.
//!
//! Kept in a separate test binary so intentional wildcard pins do not interfere
//! with the immediate-progress assertions in other suites.

use ps_reclaim::{Domain, Handle, MAX_NESTED_PINS};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

static SERIAL: Mutex<()> = Mutex::new(());

fn retire_counter(domain: &Domain, count: &Arc<AtomicUsize>) {
    let count = Arc::clone(count);
    domain.retire(move || { count.fetch_add(1, Ordering::Release); });
}

#[test]
fn one_registration_covers_many_domains_and_out_of_order_guards() {
    let _serial = SERIAL.lock().unwrap();
    let registration = Handle::new();
    let domains: Vec<_> = (0..512).map(|_| Domain::new()).collect();
    let hits = Arc::new(AtomicUsize::new(0));
    let first = domains[0].pin_with(&registration);
    let second = domains[1].pin_with(&registration);
    drop(first);
    let replacement = domains[2].pin_with(&registration);
    retire_counter(&domains[1], &hits);
    domains[1].advance();
    assert_eq!(hits.load(Ordering::Acquire), 0);
    // Visiting many domains must not allocate more registrations or erase the
    // held pin. Test the protection, rather than a process-global slot count.
    for domain in &domains[3..] {
        drop(domain.pin_with(&registration));
    }
    domains[1].advance();
    assert_eq!(hits.load(Ordering::Acquire), 0);
    drop(second);
    drop(replacement);
    for _ in 0..64 { domains[1].advance(); }
    assert_eq!(hits.load(Ordering::Acquire), 1);
}

#[test]
fn tls_and_explicit_pins_protect_independently() {
    let _serial = SERIAL.lock().unwrap();
    let domain = Domain::new();
    let registration = Handle::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let explicit = domain.pin_with(&registration);
    let implicit = domain.pin();
    retire_counter(&domain, &hits);
    drop(explicit);
    domain.advance();
    assert_eq!(hits.load(Ordering::Acquire), 0);
    drop(implicit);
    for _ in 0..64 { domain.advance(); }
    assert_eq!(hits.load(Ordering::Acquire), 1);
}

#[test]
fn forgotten_guard_does_not_allow_registration_reuse() {
    let _serial = SERIAL.lock().unwrap();
    let domain = Domain::new();
    let registration = Handle::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let guard = domain.pin_with(&registration);
    retire_counter(&domain, &hits);
    core::mem::forget(guard);
    drop(registration);
    // A mistaken release/clear here would expose the forgotten reader.
    for _ in 0..32 {
        let other = Handle::new();
        drop(domain.pin_with(&other));
        domain.advance();
    }
    assert_eq!(hits.load(Ordering::Acquire), 0);
}

#[test]
fn nested_wildcard_and_unwind_release_protection() {
    let _serial = SERIAL.lock().unwrap();
    let domain = Domain::new();
    let registration = Handle::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guards: Vec<_> = (0..=MAX_NESTED_PINS)
            .map(|_| domain.pin_with(&registration)).collect();
        retire_counter(&domain, &hits);
        domain.advance();
        assert_eq!(hits.load(Ordering::Acquire), 0);
        std::hint::black_box(&guards);
        panic!("exercise guard unwinding");
    }));
    assert!(result.is_err());
    for _ in 0..64 { domain.advance(); }
    assert_eq!(hits.load(Ordering::Acquire), 1);
}
