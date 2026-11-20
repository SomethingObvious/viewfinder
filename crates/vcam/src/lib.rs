//! The media source Frame Server loads for the virtual camera, serving the newest frame in
//! shared memory. A crash here takes every camera down for every app, so each COM method and
//! export catches panics.

macro_rules! forward_events {
    ($impl:ty) => {
        #[expect(
            unsafe_code,
            reason = "every method forwards to the event queue's COM methods"
        )]
        impl IMFMediaEventGenerator_Impl for $impl {
            fn GetEvent(
                &self,
                flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
            ) -> Result<IMFMediaEvent> {
                // SAFETY: forwards Frame Server's arguments untouched.
                guard(|| unsafe { self.queue.GetEvent(flags.0) })
            }

            fn BeginGetEvent(
                &self,
                callback: Ref<'_, IMFAsyncCallback>,
                state: Ref<'_, IUnknown>,
            ) -> Result<()> {
                // SAFETY: forwards Frame Server's arguments untouched.
                guard(|| unsafe { self.queue.BeginGetEvent(callback.as_ref(), state.as_ref()) })
            }

            fn EndGetEvent(&self, result: Ref<'_, IMFAsyncResult>) -> Result<IMFMediaEvent> {
                // SAFETY: forwards Frame Server's arguments untouched.
                guard(|| unsafe { self.queue.EndGetEvent(result.as_ref()) })
            }

            fn QueueEvent(
                &self,
                met: u32,
                extended: *const GUID,
                status: HRESULT,
                value: *const PROPVARIANT,
            ) -> Result<()> {
                // SAFETY: forwards Frame Server's arguments untouched.
                guard(|| unsafe { self.queue.QueueEventParamVar(met, extended, status, value) })
            }
        }
    };
}

mod activate;
mod source;
mod stream;

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, Ref, Result};

pub use activate::Activator;
use factory::{Factory, Factory_Impl};

/// Live COM objects plus server locks, for `DllCanUnloadNow`.
static LIVE: AtomicIsize = AtomicIsize::new(0);

#[derive(Debug)]
struct Live;

impl Live {
    fn new() -> Self {
        LIVE.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_UNEXPECTED.into()))
}

/// Ignores poisoning from a caught panic, since the service has to keep running.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Its own module, since everything `#[implement]` generates lands beside it.
#[expect(
    unsafe_code,
    trivial_casts,
    clippy::ref_as_ptr,
    clippy::inline_always,
    reason = "the COM glue #[implement] generates casts vtables and implements unsafe methods"
)]
mod factory {
    use windows::Win32::System::Com::IClassFactory;
    use windows_core::implement;

    use crate::Live;

    #[implement(IClassFactory)]
    pub(super) struct Factory {
        pub(super) _live: Live,
    }
}

impl IClassFactory_Impl for Factory_Impl {
    #[expect(unsafe_code, reason = "COM hands over raw out pointers")]
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        guard(|| {
            if ppv.is_null() || riid.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: checked for null above, and COM says it points at writable storage.
            unsafe { *ppv = std::ptr::null_mut() };
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let activator: IUnknown = Activator::new()?.into();
            // SAFETY: both pointers were checked for null and come from COM's caller.
            unsafe { activator.query(riid, ppv).ok() }
        })
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        let delta = if lock.as_bool() { 1 } else { -1 };
        LIVE.fetch_add(delta, Ordering::AcqRel);
        Ok(())
    }
}

/// Hands out the class factory for [`shared::CLSID`].
/// # Safety
/// The pointers must be valid as COM requires.
#[expect(
    unsafe_code,
    reason = "COM finds this export by name and hands over raw pointers"
)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let result = guard(|| {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked for null above, and the caller promises COM's contract.
        unsafe { *ppv = std::ptr::null_mut() };
        // SAFETY: as above.
        if unsafe { *rclsid } != shared::CLSID {
            return Err(CLASS_E_CLASSNOTAVAILABLE.into());
        }
        let factory: IClassFactory = Factory { _live: Live::new() }.into();
        // SAFETY: as above.
        unsafe { factory.query(riid, ppv).ok() }
    });
    result.map_or_else(|e| e.code(), |()| S_OK)
}

/// `S_OK` once every object this DLL handed out is gone.
#[expect(unsafe_code, reason = "COM finds this export by name")]
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}
