//! The two synchronisation primitives this crate uses, and what they are
//! without `std`.
//!
//! With `std` these are `std::sync::Mutex` and `std::sync::OnceLock`
//! unchanged, so the default build is what it was and blocks in the kernel
//! under contention exactly as before. Without `std` they come from `spin`.
//!
//! The difference is not cosmetic: a `std` mutex sleeps a waiter, and a spin
//! mutex burns a core until the holder releases. That is acceptable here only
//! because of what these guard. The free list is a `Vec<usize>` touched once
//! per thread lifetime, and the garbage list is touched on retire. **Neither is
//! on the read path**, which is the whole point of this crate. A spin around a
//! critical section that short and that rare is fine; a spin on the pin path
//! would not be, and there is none.
//!
//! `spin` rather than something written here, because a spin lock and a
//! once-cell are exactly the kind of small unsafe primitive that looks obvious
//! and is not, and this crate exists to make memory reclamation safe. With
//! `default-features = false` it brings in nothing of its own.

#[cfg(feature = "std")]
pub(crate) use std::sync::{Mutex, OnceLock};

#[cfg(not(feature = "std"))]
pub(crate) use spin::{Mutex, Once as OnceLock};

/// Lock, recovering from poisoning where the concept exists.
///
/// `std`'s `Mutex` reports a poisoned lock when a holder panicked, and every
/// call site here wants the value anyway: what is behind these locks is a `Vec`
/// of indices and a `Vec` of closures, and a panic mid-push leaves either
/// consistent. `spin` has no poisoning to report, so this is where the two
/// shapes meet.
#[cfg(feature = "std")]
#[inline]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(not(feature = "std"))]
#[inline]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> spin::MutexGuard<'_, T> {
    mutex.lock()
}

/// Unique access to the contents, for a caller that already has `&mut` and
/// should not pay for a lock at all.
#[cfg(feature = "std")]
#[inline]
pub(crate) fn get_mut<T>(mutex: &mut Mutex<T>) -> &mut T {
    mutex.get_mut().unwrap_or_else(|e| e.into_inner())
}

#[cfg(not(feature = "std"))]
#[inline]
pub(crate) fn get_mut<T>(mutex: &mut Mutex<T>) -> &mut T {
    mutex.get_mut()
}

/// Initialise once, then read forever.
///
/// `std` spells this `get_or_init` and `spin` spells it `call_once`; they mean
/// the same thing and return the same reference.
#[cfg(feature = "std")]
#[inline]
pub(crate) fn get_or_init<T>(once: &OnceLock<T>, init: impl FnOnce() -> T) -> &T {
    once.get_or_init(init)
}

#[cfg(not(feature = "std"))]
#[inline]
pub(crate) fn get_or_init<T>(once: &OnceLock<T>, init: impl FnOnce() -> T) -> &T {
    once.call_once(init)
}
