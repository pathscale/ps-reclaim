//! The two synchronisation primitives this crate uses, and what they are
//! without `std`.
//!
//! With `std` these are `std::sync::Mutex` and `std::sync::OnceLock`
//! unchanged, so the default build is what it was and blocks in the kernel
//! under contention exactly as before. Without `std` they come from `spin`.
//!
//! A spin mutex burns CPU while waiting, including when its holder is
//! descheduled. Neither mutex is on the steady-state read path, but garbage
//! extraction scans a potentially unbounded backlog and may allocate under
//! the lock. No bounded writer-latency guarantee follows from short pins.
//! `spin-garbage` changes only the garbage mutex in a std build, permitting an
//! isolated experiment without simultaneously changing TLS or registration.
//!
//! `spin` rather than something written here, because a spin lock and a
//! once-cell are exactly the kind of small unsafe primitive that looks obvious
//! and is not, and this crate exists to make memory reclamation safe. With
//! `default-features = false` it brings in nothing of its own.

#[cfg(feature = "std")]
pub(crate) use std::sync::{Mutex, OnceLock};

#[cfg(not(feature = "std"))]
pub(crate) use spin::{Mutex, Once as OnceLock};

#[cfg(all(feature = "std", not(feature = "spin-garbage")))]
pub(crate) use std::sync::Mutex as GarbageMutex;
#[cfg(any(not(feature = "std"), feature = "spin-garbage"))]
pub(crate) use spin::Mutex as GarbageMutex;

#[cfg(all(feature = "std", not(feature = "spin-garbage")))]
#[inline]
pub(crate) fn garbage_lock<T>(mutex: &GarbageMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(any(not(feature = "std"), feature = "spin-garbage"))]
#[inline]
pub(crate) fn garbage_lock<T>(mutex: &GarbageMutex<T>) -> spin::MutexGuard<'_, T> {
    mutex.lock()
}

#[cfg(all(feature = "std", not(feature = "spin-garbage")))]
#[inline]
pub(crate) fn garbage_get_mut<T>(mutex: &mut GarbageMutex<T>) -> &mut T {
    mutex.get_mut().unwrap_or_else(|e| e.into_inner())
}

#[cfg(any(not(feature = "std"), feature = "spin-garbage"))]
#[inline]
pub(crate) fn garbage_get_mut<T>(mutex: &mut GarbageMutex<T>) -> &mut T {
    mutex.get_mut()
}

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
