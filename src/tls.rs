//! `thread_local!` for a build that does not link `std`.
//!
//! Compiled only when `std` is off. With `std` the crate keeps using
//! `thread_local!` exactly as before.
//!
//! # What this costs, measured rather than assumed
//!
//! An earlier version of this comment said the `no_std` pin path is slower than
//! the `std` one *by construction*. That was wrong twice over, and the
//! correction is worth more than the original claim.
//!
//! It is wrong on this platform. On Mach-O, `std`'s thread-local is itself a
//! call: both `#[thread_local]` and a `const`-initialised `thread_local!`
//! compile at `-O` to five instructions ending in `blr x8`, through a TLV
//! descriptor, before the variable is addressed. So both options here are a
//! call and the only difference is which function you land in. Fifty million
//! accesses each, three interleaved rounds on an M4 Max, with the same
//! `thread_local!` arm measured twice per round as the null:
//!
//! ```text
//!            thread_local!   pthread_getspecific   null (macro again)
//!   round 1       1.71 ns              1.41 ns             1.19 ns
//!   round 2       1.17 ns              1.40 ns             1.20 ns
//!   round 3       1.19 ns              1.36 ns             1.16 ns
//! ```
//!
//! The gap between the mechanisms is about 0.2 ns; the null moves by 0.5 on a
//! bad round. There is no penalty here to speak of.
//!
//! It is also wrong in principle, which matters more, because it would have
//! sent someone looking for a cleverer `Tls`. **The fast path is not gated on
//! `std`, it is gated on the feature being stable.** `std`'s own fast path *is*
//! `#[thread_local]`, in `sys/thread_local/native/`, and a `no_std` crate on
//! nightly can write that attribute and get identical codegen. On ELF, where
//! local-exec collapses to a register plus an offset, that is the whole win
//! with no `std` involved. What `std` genuinely owns is the destructor
//! plumbing: the weak `__cxa_thread_atexit_impl` lookup, Apple's `_tlv_atexit`,
//! and the Windows `.CRT$XLB` callback, and even those are reachable through
//! `libc`.
//!
//! # What actually costs, then
//!
//! The count, not the mechanism. Four thread-locals on the pin path is four
//! calls at roughly 1.4 ns whichever mechanism is chosen, and three of the four
//! values here have no destructor and could share one slot. Folding `MINE`,
//! `SHARED` and `PIN_MASK` into a single struct is worth about 2.8 ns per pin,
//! which is a larger number than anything in the table above. That is a change
//! to the hot path of a lock-free crate and wants its own measurement, so it is
//! not in the change that introduced this file.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

extern crate alloc;
use alloc::boxed::Box;

/// One value of type `T` per thread.
pub(crate) struct Tls<T> {
    /// The platform key, biased by one so that zero means "not yet created"
    /// and `new` stays a `const fn`. A `usize` holds either a `pthread_key_t`
    /// or a Windows FLS index.
    key: AtomicUsize,
    owns: PhantomData<fn() -> T>,
}

// SAFETY: the key is atomic, and every value behind it belongs to one thread.
unsafe impl<T> Sync for Tls<T> {}

#[cfg(unix)]
unsafe extern "C" fn drop_value<T>(value: *mut core::ffi::c_void) {
    if !value.is_null() {
        // SAFETY: the only pointer ever stored under this key came from
        // `Box::into_raw` in `with`, for this same `T`.
        drop(unsafe { Box::from_raw(value.cast::<T>()) });
    }
}

#[cfg(windows)]
unsafe extern "system" fn drop_value<T>(value: *const core::ffi::c_void) {
    if !value.is_null() {
        // SAFETY: as above. `FlsAlloc` types its callback argument as const;
        // the pointer under it is the one `Box::into_raw` handed over.
        drop(unsafe { Box::from_raw(value.cast::<T>().cast_mut()) });
    }
}

mod slot {
    use core::ffi::c_void;

    #[cfg(unix)]
    pub(super) fn create(drop: unsafe extern "C" fn(*mut c_void)) -> usize {
        let mut fresh: libc::pthread_key_t = 0;
        // SAFETY: `fresh` is a live local and `drop` is valid for the life of
        // the process.
        let rc = unsafe { libc::pthread_key_create(&raw mut fresh, Some(drop)) };
        assert_eq!(rc, 0, "pthread_key_create");
        fresh as usize
    }

    #[cfg(unix)]
    pub(super) fn destroy(key: usize) {
        // SAFETY: `key` came from `create` and no thread has stored under it.
        unsafe { libc::pthread_key_delete(key as libc::pthread_key_t) };
    }

    #[cfg(unix)]
    pub(super) fn get(key: usize) -> *mut c_void {
        // SAFETY: `key` came from `create` and is valid process-wide.
        unsafe { libc::pthread_getspecific(key as libc::pthread_key_t) }
    }

    #[cfg(unix)]
    pub(super) fn set(key: usize, value: *mut c_void) -> bool {
        // SAFETY: as above; `value` is live and uniquely owned.
        unsafe { libc::pthread_setspecific(key as libc::pthread_key_t, value) == 0 }
    }

    #[cfg(windows)]
    pub(super) fn create(drop: unsafe extern "system" fn(*const c_void)) -> usize {
        // SAFETY: `drop` is valid for the life of the process.
        let fresh = unsafe { windows_sys::Win32::System::Threading::FlsAlloc(Some(drop)) };
        assert_ne!(fresh, u32::MAX, "FlsAlloc");
        fresh as usize
    }

    #[cfg(windows)]
    pub(super) fn destroy(key: usize) {
        // SAFETY: as above.
        unsafe { windows_sys::Win32::System::Threading::FlsFree(key as u32) };
    }

    #[cfg(windows)]
    pub(super) fn get(key: usize) -> *mut c_void {
        // SAFETY: as above.
        unsafe { windows_sys::Win32::System::Threading::FlsGetValue(key as u32) }
    }

    #[cfg(windows)]
    pub(super) fn set(key: usize, value: *mut c_void) -> bool {
        // SAFETY: as above.
        unsafe { windows_sys::Win32::System::Threading::FlsSetValue(key as u32, value) != 0 }
    }
}

impl<T> Tls<T> {
    pub(crate) const fn new() -> Self {
        Self {
            key: AtomicUsize::new(0),
            owns: PhantomData,
        }
    }

    /// Racing threads each create a key and one wins the swap; the losers hand
    /// theirs straight back.
    ///
    /// A lock would create it once instead, and is not worth reaching for: the
    /// window is one process-lifetime initialisation, and a lock here would be
    /// a second thing to get right on a path that has to be correct before it
    /// is fast. `std` does the same thing for the same reason, in
    /// `sys/thread_local/key/racy.rs`.
    fn key(&self) -> usize {
        match self.key.load(Ordering::Acquire) {
            0 => {}
            biased => return biased - 1,
        }
        let fresh = slot::create(drop_value::<T>);
        match self
            .key
            .compare_exchange(0, fresh + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => fresh,
            Err(biased) => {
                // Another thread won. This key holds no value on any thread,
                // since nothing has been handed it yet.
                slot::destroy(fresh);
                biased - 1
            }
        }
    }

    /// Run `body` against this thread's value, creating it on first touch.
    ///
    /// `None` means the slot could not be established, which is what
    /// `thread_local!`'s `try_with` reports during thread teardown. Callers
    /// have to tolerate it, and in this crate they do: a failure here loses a
    /// cached lookup or leaks one slot, and never loses correctness.
    pub(crate) fn with<R>(
        &self,
        init: impl FnOnce() -> T,
        body: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        let key = self.key();
        let existing = slot::get(key);
        if !existing.is_null() {
            // SAFETY: stored below for this same `T`, and owned by this thread
            // until its destructor runs.
            return Some(body(unsafe { &*existing.cast::<T>() }));
        }
        let fresh = Box::into_raw(Box::new(init()));
        if !slot::set(key, fresh.cast()) {
            // SAFETY: nothing took ownership, so this reclaims it.
            drop(unsafe { Box::from_raw(fresh) });
            return None;
        }
        // SAFETY: just stored, and unreachable from any other thread.
        Some(body(unsafe { &*fresh }))
    }
}
