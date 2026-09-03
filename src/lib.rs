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
//! 2. **A reader that starts *after* a retirement does not delay it.** This is
//!    the one that separates schemes. Under continuous read traffic there may
//!    never be an instant with zero readers, and a scheme that waits for one
//!    never reclaims anything at all. Reference-counted schemes (`seize`, and
//!    a plain global reader counter) do not provide this; epoch schemes do.
//!
//! 3. **Reclamation is driven, not incidental.** [`Domain::advance`] is the
//!    only thing that runs retirements. Nothing happens on the read path, so
//!    a read never pays for someone else's garbage.
//!
//! # What a domain is
//!
//! One grace period. Readers of domain A never delay reclamation in domain B,
//! so a slow scan over one table cannot stall another. Domains are cheap: a
//! few words, because the participant registry is process-wide rather than
//! per-domain. That matters when there is a domain per table and a thousand
//! tables.
//!
//! # Cost
//!
//! The read path publishes into the calling thread's own padded slot and
//! fences. No shared cache line is written, so it does not degrade as readers
//! are added. `benches/pin.rs` measures it against `crossbeam-epoch` and
//! `seize`; run it before changing anything here.

#![deny(missing_docs)]

mod domain;
mod registry;

pub use domain::{Domain, Guard};
pub use registry::slots_handed_out;

/// Upper bound on threads holding pins at once.
///
/// Past it, threads share the last slot: correct, because a shared pin only
/// ever delays reclamation, never permits it early, but contended.
pub const MAX_THREADS: usize = registry::MAX_THREADS;

/// Domains one thread can be pinned in simultaneously.
///
/// Past it a thread publishes a wildcard and is treated as pinned everywhere
/// until it releases: conservative, never unsound.
pub const MAX_NESTED_PINS: usize = registry::PINS_PER_THREAD;
