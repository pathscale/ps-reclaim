//! Platform-owned TLS for builds without `std`.
//!
//! Unix values have pthread-key lifetime; Windows values have FLS (fiber)
//! lifetime. The stable path stores the registration cache and lease together.
//! Native Unix TLS uses a separate platform-key lease and explicitly closes
//! the native cache before returning the registration. `get` never creates a
//! value, so guard teardown cannot initialize a replacement cache.
//!
//! Access cost depends on target, linkage, optimizer and initialization path.
//! Historical timings are not a performance guarantee; see docs/tls-cost.md.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

extern crate alloc;
use alloc::boxed::Box;

/// One value of type `T` per thread on Unix, per fiber on Windows.
pub(crate) struct Tls<T> {
    /// The platform key, biased by one so that zero means "not yet created"
    /// and `new` stays a `const fn`. A `usize` holds either a `pthread_key_t`
    /// or a Windows FLS index.
    key: AtomicUsize,
    owns: PhantomData<fn() -> T>,
}

// SAFETY: the key is atomic; values are only accessed on the owning execution
// context (thread on Unix, fiber on Windows).
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
    /// Read an existing value without creating a key or invoking an initializer.
    #[cfg(not(all(feature = "nightly", unix)))]
    #[inline]
    pub(crate) fn get<R>(&self, body: impl FnOnce(&T) -> R) -> Option<R> {
        let biased = self.key.load(Ordering::Acquire);
        if biased == 0 {
            return None;
        }
        let existing = slot::get(biased - 1);
        if existing.is_null() {
            return None;
        }
        // SAFETY: the slot is thread/fiber-local, holds this T, and no user
        // code runs between retrieving it and lending it to the closure.
        Some(body(unsafe { &*existing.cast::<T>() }))
    }

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
    /// `None` means storing a newly initialized value failed. Unlike std's
    /// `try_with`, this is not a teardown detector: a later destructor may
    /// initialize a new platform-key value. Registration ownership must handle
    /// that case without recycling a slot that still has live pins.
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
