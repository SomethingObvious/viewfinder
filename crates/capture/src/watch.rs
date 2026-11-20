//! Counts cameras coming and going, so the list of them is only made again when it could
//! have changed.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
    CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CR_SUCCESS, HCMNOTIFICATION,
};
use windows_core::GUID;

/// WPD for PTP bodies, then `KSCATEGORY_VIDEO_CAMERA` and `KSCATEGORY_VIDEO`, which between
/// them hold every device Media Foundation lists as a video source.
const CLASSES: [GUID; 3] = [
    GUID::from_u128(0x6ac2_7878_a6fa_4155_ba85_f98f_491d_4f33),
    GUID::from_u128(0xe532_3777_f976_4f5b_9b55_b946_99c4_6e44),
    GUID::from_u128(0x6994_ad05_93ef_11d0_a3cc_00a0_c922_3196),
];

static CHANGES: AtomicU64 = AtomicU64::new(0);
/// The registrations live as long as the process, so only whether they worked is kept.
static WATCHING: OnceLock<bool> = OnceLock::new();

#[expect(unsafe_code, reason = "Windows calls this back with raw event data")]
unsafe extern "system" fn on_change(
    _: HCMNOTIFICATION,
    _: *const core::ffi::c_void,
    action: CM_NOTIFY_ACTION,
    _: *const CM_NOTIFY_EVENT_DATA,
    _: u32,
) -> u32 {
    if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
        || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
    {
        CHANGES.fetch_add(1, Ordering::Relaxed);
    }
    0
}

#[expect(
    unsafe_code,
    reason = "registering for device notifications is a Win32 call"
)]
fn watch() -> bool {
    *WATCHING.get_or_init(|| {
        CLASSES.iter().all(|class| {
            let mut filter = CM_NOTIFY_FILTER {
                cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
                FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
                ..Default::default()
            };
            filter.u.DeviceInterface.ClassGuid = *class;
            let mut handle = HCMNOTIFICATION(std::ptr::null_mut());
            // SAFETY: the filter and handle are live locals, and the callback lives for the
            // process.
            let r = unsafe {
                CM_Register_Notification(&raw const filter, None, Some(on_change), &raw mut handle)
            };
            r == CR_SUCCESS
        })
    })
}

/// How many times a camera, webcam or capture card has arrived or gone. The first call starts
/// listening, and if Windows won't register, the count never moves.
pub fn changes() -> u64 {
    watch();
    CHANGES.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    #[test]
    fn registers_for_every_class() {
        assert!(super::watch());
    }
}
