//! Tells the capture thread when a Canon appears on WPD, so it needn't poll.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_EVENT_DATA,
    CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CR_SUCCESS,
    HCMNOTIFICATION,
};
use windows::Win32::Devices::PortableDevices::GUID_DEVINTERFACE_WPD;

use crate::CANON_VID;

static ARRIVED: AtomicBool = AtomicBool::new(false);
static ARRIVALS: AtomicU64 = AtomicU64::new(0);
/// The registration lives as long as the process, so only whether it worked is kept.
static WATCHING: OnceLock<bool> = OnceLock::new();

#[expect(unsafe_code, reason = "Windows calls this back with raw event data")]
unsafe extern "system" fn on_change(
    _: HCMNOTIFICATION,
    _: *const core::ffi::c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    size: u32,
) -> u32 {
    if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL && !data.is_null() {
        // The symbolic link is a null-terminated string that runs past the struct, to `size`.
        // SAFETY: not null, and Windows passes an interface arrival record here.
        let link =
            unsafe { std::ptr::addr_of!((*data).u.DeviceInterface.SymbolicLink) }.cast::<u16>();
        let start = link as usize - data as usize;
        let units = (size as usize).saturating_sub(start).div_euclid(2);
        // SAFETY: the record is `size` bytes long, and `units` stops at its end.
        let link = unsafe { std::slice::from_raw_parts(link, units) };
        let link = String::from_utf16_lossy(link.split(|&u| u == 0).next().unwrap_or_default());
        if link.to_ascii_lowercase().contains(CANON_VID) {
            ARRIVED.store(true, Ordering::Relaxed);
            ARRIVALS.fetch_add(1, Ordering::Relaxed);
        }
    }
    0
}

/// Starts listening for Canons arriving on WPD, false if Windows wouldn't register.
#[expect(
    unsafe_code,
    reason = "registering for device notifications is a Win32 call"
)]
pub fn watch_arrivals() -> bool {
    *WATCHING.get_or_init(|| {
        let mut filter = CM_NOTIFY_FILTER {
            cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            ..Default::default()
        };
        filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_WPD;
        let mut handle = HCMNOTIFICATION(std::ptr::null_mut());
        // SAFETY: the filter and handle are live locals, and the callback lives for the process.
        let r = unsafe {
            CM_Register_Notification(&raw const filter, None, Some(on_change), &raw mut handle)
        };
        r == CR_SUCCESS
    })
}

/// Whether a Canon has arrived since the last call.
pub fn arrived() -> bool {
    ARRIVED.swap(false, Ordering::Relaxed)
}

/// How many times a Canon has arrived. A body that came back may have moved its lens.
pub fn arrivals() -> u64 {
    ARRIVALS.load(Ordering::Relaxed)
}
