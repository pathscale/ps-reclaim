//! Grace-period reclamation for lock-free readers.
//!
//! A reader loads a pointer out of a shared slot and dereferences it without
//! taking a lock. A writer removes that pointer and wants the memory back. The
//! question this crate answers is *when is that safe*, and the answer is: once
//! every reader that could have loaded the pointer has finished.
//!
//! # The contract
//!
//! Three guarantees, each of which has a test in `tests/contract.rs` that
//! names it. They are the reason this exists rather than a dependency.
//!
//! 1. **A retirement is not run while any reader that predates it is live.**
//!    The obvious one. Without it, use-after-free.
//!
//! 2. **Readers in later epochs do not delay older retirements.** Readers in
//!    the same epoch can delay them. Wildcard pins delay all reclamation, and
//!    at epoch saturation reclamation requires a quiescent scan.
//!
//! 3. **Reclamation is driven, not incidental.** [`Domain::advance`] is the
//!    explicit driver (along with `advance_up_to`); dropping a domain also
//!    runs its remaining retirements. Read-side unpin never runs callbacks.
//!
//! # What a domain is
//!
//! One grace period. Normal pins are domain-specific; overflow wildcard pins
//! delay every domain. Packed domain-ID collisions are conservative. Domains
//! are a few words, because the participant registry is process-wide rather than
//! per-domain. That matters when there is a domain per table and a thousand
//! tables.
//!
//! # Cost
//!
//! The read path publishes into the calling thread's own padded slot and
//! fences. Normal pins do not write another reader's cache line; overflow
//! pins share a counter. This is not a bounded-latency guarantee for writers:
//! retirement and advancement take locks and may allocate.

// `not(test)` so the harness keeps its own prelude while the library under test
// is the `no_std` one. `cargo check --no-default-features` is what proves the
// library does not link `std`, since a test binary cannot.
// Native no_std TLS is Unix-only: a platform-key lease closes its cache at
// teardown. Windows keeps cache and lease together in FLS (fiber lifetime).
#![cfg_attr(
    all(not(feature = "std"), feature = "nightly", unix),
    feature(thread_local)
)]
#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]
#![deny(missing_docs)]

extern crate alloc;

mod sync;
#[cfg(not(feature = "std"))]
mod tls;

mod domain;
mod registry;

pub use domain::{Domain, Guard, Handle, HandleGuard};
pub use registry::slots_handed_out;

/// Registry capacity, including one shared overflow slot.
///
/// Each live `Handle` and each implicit TLS registration uses a slot. After
/// 255 exclusive registrations, pins share the last slot and delay reclamation
/// in all domains. It is not a bound on the number of OS threads.
pub const MAX_THREADS: usize = registry::MAX_THREADS;

/// Normal simultaneous pins per registration (including repeated domains).
///
/// Past it a thread publishes a wildcard and is treated as pinned everywhere
/// until it releases: conservative, never unsound.
pub const MAX_NESTED_PINS: usize = registry::PINS_PER_THREAD;
