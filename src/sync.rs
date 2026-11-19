//! The two synchronisation primitives this crate uses, over `parking_lot_lite_hack`
//! (`Mutex`, kernel-blocking across Unix, `target_os = "none"`, and Windows) and `ekostd` (`OnceLock`).

pub(crate) use eko::thread::OnceLock;
pub(crate) use parking_lot::Mutex;
pub(crate) use parking_lot::Mutex as GarbageMutex;

#[inline]
pub(crate) fn garbage_lock<T>(mutex: &GarbageMutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

#[inline]
pub(crate) fn garbage_get_mut<T>(mutex: &mut GarbageMutex<T>) -> &mut T {
    mutex.get_mut()
}

#[inline]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

#[inline]
pub(crate) fn get_or_init<T>(once: &OnceLock<T>, init: impl FnOnce() -> T) -> &T {
    once.get_or_init(init)
}
