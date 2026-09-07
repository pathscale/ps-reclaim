//! Real FLS lifetime/ownership test; a platform cross-check alone cannot test it.
#![cfg(all(windows, not(feature = "std")))]

use core::ffi::c_void;
use ps_reclaim::Domain;
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::System::Threading::{
    ConvertFiberToThread, ConvertThreadToFiber, CreateFiber, DeleteFiber, SwitchToFiber,
};

struct State {
    parent: *mut c_void,
    domain: Domain,
    freed: Arc<AtomicBool>,
    stage: Cell<usize>,
}

unsafe extern "system" fn child(parameter: *mut c_void) {
    // SAFETY: the parent owns this boxed state until after DeleteFiber.
    let state = unsafe { &*parameter.cast::<State>() };
    let guard = state.domain.pin();
    let freed = Arc::clone(&state.freed);
    state.domain.retire(move || freed.store(true, Ordering::Release));
    state.stage.set(1);
    // SAFETY: parent names the suspended parent fiber on this same OS thread.
    unsafe { SwitchToFiber(state.parent) };
    drop(guard);
    state.stage.set(2);
    // A fiber entry must not return (that would terminate the OS thread).
    loop {
        unsafe { SwitchToFiber(state.parent) };
    }
}

struct Child(*mut c_void);
impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: parent only deletes its suspended child, never itself.
        unsafe { DeleteFiber(self.0) };
    }
}

struct Parent;
impl Drop for Parent {
    fn drop(&mut self) {
        // SAFETY: execution has switched back to the converted parent fiber.
        unsafe { ConvertFiberToThread() };
    }
}

#[test]
fn switching_fibers_does_not_share_or_erase_their_pins() {
    std::thread::spawn(|| {
        // SAFETY: fresh test thread has not been converted to a fiber before.
        let parent = unsafe { ConvertThreadToFiber(core::ptr::null()) };
        assert!(!parent.is_null());
        let _restore = Parent;
        for _ in 0..8 {
            let state = Box::new(State {
                parent,
                domain: Domain::new(),
                freed: Arc::new(AtomicBool::new(false)),
                stage: Cell::new(0),
            });
            // SAFETY: State stays at its boxed address through child deletion.
            let fiber = unsafe { CreateFiber(0, Some(child), (&*state as *const State).cast()) };
            assert!(!fiber.is_null());
            let fiber = Child(fiber);
            unsafe { SwitchToFiber(fiber.0) };
            assert_eq!(state.stage.get(), 1);
            // Parent and child are on one OS thread but need separate FLS
            // registrations. Parent unpin must not erase the suspended child.
            drop(state.domain.pin());
            state.domain.advance();
            assert!(!state.freed.load(Ordering::Acquire));
            unsafe { SwitchToFiber(fiber.0) };
            assert_eq!(state.stage.get(), 2);
            state.domain.advance();
            assert!(state.freed.load(Ordering::Acquire));
            drop(fiber); // runs child FLS destructors; next child may reuse slots
        }
    })
    .join()
    .unwrap();
}
