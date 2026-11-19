//! The `IMFActivate` Frame Server creates first, which makes the source.

use std::ffi::c_void;
use std::sync::Mutex;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate_Impl, IMFAttributes, IMFAttributes_Impl, MF_ATTRIBUTE_TYPE,
    MF_ATTRIBUTES_MATCH_TYPE, MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES,
    MFT_TRANSFORM_CLSID_Attribute,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{BOOL, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result};

use crate::source::{Source, new_attributes};
use crate::{Live, guard, lock};

pub use object::Activator;
use object::Activator_Impl;

/// The activator on its own, since everything `#[implement]` generates lands beside it.
#[expect(
    unsafe_code,
    trivial_casts,
    clippy::ref_as_ptr,
    clippy::inline_always,
    reason = "the COM glue #[implement] generates casts vtables and implements unsafe methods"
)]
mod object {
    use std::sync::Mutex;

    use windows::Win32::Media::MediaFoundation::{IMFActivate, IMFAttributes};
    use windows_core::{ComObject, implement};

    use crate::Live;
    use crate::source::Source;

    /// Creates the media source on demand and carries the attributes Frame
    /// Server sets on it, which the source copies at creation.
    #[derive(Debug)]
    #[implement(IMFActivate)]
    pub struct Activator {
        pub(super) attributes: IMFAttributes,
        pub(super) source: Mutex<Option<ComObject<Source>>>,
        pub(super) _live: Live,
    }
}

impl Activator {
    /// Makes an activator with the attributes `VCamSample` sets.
    ///
    /// # Errors
    ///
    /// Fails when Media Foundation can't make or fill the attribute store.
    #[expect(unsafe_code, reason = "setting attributes is a COM call")]
    pub fn new() -> Result<Self> {
        let attributes = new_attributes()?;
        // SAFETY: the keys and values are statics.
        unsafe {
            attributes.SetUINT32(&MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, 1)?;
            attributes.SetGUID(&MFT_TRANSFORM_CLSID_Attribute, &shared::CLSID)?;
        }
        Ok(Self {
            attributes,
            source: Mutex::new(None),
            _live: Live::new(),
        })
    }
}

#[expect(unsafe_code, reason = "COM hands over raw out pointers")]
impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        guard(|| {
            if ppv.is_null() || riid.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: checked for null above, and COM says it points at writable storage.
            unsafe { *ppv = std::ptr::null_mut() };
            let mut cached = lock(&self.source);
            let source = match &*cached {
                Some(s) if !s.is_shut_down() => s.clone(),
                _ => {
                    let s = Source::new(&self.attributes)?;
                    *cached = Some(s.clone());
                    s
                }
            };
            drop(cached);
            // SAFETY: both pointers were checked for null and come from COM's caller.
            unsafe { source.as_interface::<IUnknown>().query(riid, ppv).ok() }
        })
    }

    // Microsoft's own virtual camera sample and VCamSample both return S_OK
    // here and leave shutting the source down to whoever holds it.
    fn ShutdownObject(&self) -> Result<()> {
        Ok(())
    }

    fn DetachObject(&self) -> Result<()> {
        guard(|| {
            lock(&self.source).take();
            Ok(())
        })
    }
}

/// Calls an `IMFAttributes` method on the inner store straight through its
/// vtable, for the methods whose safe wrappers reshape the arguments.
macro_rules! raw {
    ($store:expr, $method:ident $(, $arg:expr)*) => {
        // SAFETY: the vtable belongs to the live store, and the arguments are COM's, untouched.
        unsafe { (Interface::vtable(&$store).$method)(Interface::as_raw(&$store) $(, $arg)*).ok() }
    };
}

#[expect(
    unsafe_code,
    reason = "every method forwards raw COM arguments to the inner store"
)]
impl IMFAttributes_Impl for Activator_Impl {
    fn GetItem(&self, key: *const GUID, value: *mut PROPVARIANT) -> Result<()> {
        guard(|| raw!(self.attributes, GetItem, key, value))
    }

    fn GetItemType(&self, key: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetItemType(key) })
    }

    fn CompareItem(&self, key: *const GUID, value: *const PROPVARIANT) -> Result<BOOL> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.CompareItem(key, value) })
    }

    fn Compare(
        &self,
        theirs: Ref<'_, IMFAttributes>,
        kind: MF_ATTRIBUTES_MATCH_TYPE,
    ) -> Result<BOOL> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.Compare(theirs.as_ref(), kind) })
    }

    fn GetUINT32(&self, key: *const GUID) -> Result<u32> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetUINT32(key) })
    }

    fn GetUINT64(&self, key: *const GUID) -> Result<u64> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetUINT64(key) })
    }

    fn GetDouble(&self, key: *const GUID) -> Result<f64> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetDouble(key) })
    }

    fn GetGUID(&self, key: *const GUID) -> Result<GUID> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetGUID(key) })
    }

    fn GetStringLength(&self, key: *const GUID) -> Result<u32> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetStringLength(key) })
    }

    fn GetString(&self, key: *const GUID, buf: PWSTR, cch: u32, len: *mut u32) -> Result<()> {
        guard(|| raw!(self.attributes, GetString, key, buf, cch, len))
    }

    fn GetAllocatedString(&self, key: *const GUID, value: *mut PWSTR, len: *mut u32) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetAllocatedString(key, value, len) })
    }

    fn GetBlobSize(&self, key: *const GUID) -> Result<u32> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetBlobSize(key) })
    }

    fn GetBlob(&self, key: *const GUID, buf: *mut u8, size: u32, len: *mut u32) -> Result<()> {
        guard(|| raw!(self.attributes, GetBlob, key, buf, size, len))
    }

    fn GetAllocatedBlob(&self, key: *const GUID, buf: *mut *mut u8, size: *mut u32) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetAllocatedBlob(key, buf, size) })
    }

    fn GetUnknown(&self, key: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        guard(|| raw!(self.attributes, GetUnknown, key, riid, ppv))
    }

    fn SetItem(&self, key: *const GUID, value: *const PROPVARIANT) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.SetItem(key, value) })
    }

    fn DeleteItem(&self, key: *const GUID) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.DeleteItem(key) })
    }

    fn DeleteAllItems(&self) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.DeleteAllItems() })
    }

    fn SetUINT32(&self, key: *const GUID, value: u32) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.SetUINT32(key, value) })
    }

    fn SetUINT64(&self, key: *const GUID, value: u64) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.SetUINT64(key, value) })
    }

    fn SetDouble(&self, key: *const GUID, value: f64) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.SetDouble(key, value) })
    }

    fn SetGUID(&self, key: *const GUID, value: *const GUID) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.SetGUID(key, value) })
    }

    fn SetString(&self, key: *const GUID, value: &PCWSTR) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.SetString(key, *value) })
    }

    fn SetBlob(&self, key: *const GUID, buf: *const u8, size: u32) -> Result<()> {
        guard(|| raw!(self.attributes, SetBlob, key, buf, size))
    }

    fn SetUnknown(&self, key: *const GUID, value: Ref<'_, IUnknown>) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.SetUnknown(key, value.as_ref()) })
    }

    fn LockStore(&self) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.LockStore() })
    }

    fn UnlockStore(&self) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.UnlockStore() })
    }

    fn GetCount(&self) -> Result<u32> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.GetCount() })
    }

    fn GetItemByIndex(&self, index: u32, key: *mut GUID, value: *mut PROPVARIANT) -> Result<()> {
        guard(|| raw!(self.attributes, GetItemByIndex, index, key, value))
    }

    fn CopyAllItems(&self, dest: Ref<'_, IMFAttributes>) -> Result<()> {
        // SAFETY: forwards COM's arguments to the inner store untouched.
        guard(|| unsafe { self.attributes.CopyAllItems(dest.as_ref()) })
    }
}
