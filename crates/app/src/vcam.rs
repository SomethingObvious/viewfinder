//! The Windows virtual camera and its name.
//!
//! Windows adds " (Windows Virtual Camera)" to every virtual camera's name and denies the app
//! that made it `AddProperty`. An administrator can still set the interface's name, so
//! install.ps1 registers a task that runs `viewfinder.exe --name-camera` elevated.

use std::os::windows::process::CommandExt;
use std::time::Duration;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL,
    CM_Locate_DevNodeW, CM_Set_DevNode_PropertyW, CM_Set_Device_Interface_PropertyW, CR_SUCCESS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_FriendlyName, DEVPKEY_Device_InstanceId, DEVPKEY_DeviceInterface_FriendlyName,
    DEVPROP_TYPE_STRING, DEVPROPTYPE,
};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFCreateVirtualCamera, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime_Session, MFVirtualCameraType_SoftwareCameraSource,
};
use windows_core::{GUID, HSTRING, PCWSTR};

pub(crate) const TASK: &str = "Viewfinder Camera Name";
/// Each class keeps its own name for the camera. Media Foundation apps read
/// `KSCATEGORY_VIDEO_CAMERA`, and browsers, Discord and OBS read `KSCATEGORY_CAPTURE` and
/// `KSCATEGORY_VIDEO`.
const CAMERA_CLASSES: &[GUID] = &[
    GUID::from_u128(0xe5323777_f976_4f5b_9b55_b94699c46e44),
    GUID::from_u128(0x65e8773d_8f56_11d0_a3b9_00a0c9223196),
    GUID::from_u128(0x6994ad05_93ef_11d0_a3cc_00a0c9223196),
];

pub(crate) struct VirtualCamera(IMFVirtualCamera);

impl VirtualCamera {
    /// Registers the camera for this user until this is dropped or the process exits.
    #[expect(
        unsafe_code,
        reason = "making the virtual camera is a Media Foundation call"
    )]
    pub(crate) fn start() -> Result<Self, String> {
        // SAFETY: the strings are live HSTRINGs for the length of the call.
        let camera = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                &HSTRING::from(name()),
                &HSTRING::from(shared::CLSID_STR),
                None,
            )
        }
        .map_err(|e| format!("Windows refused to create the camera ({e})"))?;
        // SAFETY: the camera was just created, and no callback is passed.
        unsafe { camera.Start(None) }.map_err(|e| {
            format!(
                "The camera DLL isn't installed. Run scripts\\install.ps1 as administrator. ({e})"
            )
        })?;
        rename();
        Ok(Self(camera))
    }
}

impl Drop for VirtualCamera {
    #[expect(
        unsafe_code,
        reason = "removing the virtual camera is a Media Foundation call"
    )]
    fn drop(&mut self) {
        // SAFETY: the camera is live, and nothing uses it after drop.
        unsafe { self.0.Shutdown().ok() };
    }
}

/// "Viewfinder (Canon EOS REBEL T3)" once a camera has been seen, plain "Viewfinder" before.
fn name() -> String {
    let model: String = crate::selection::model()
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    if model.is_empty() {
        "Viewfinder".into()
    } else {
        format!("Viewfinder ({model})")
    }
}

/// Saves the camera's model for the name, renaming the virtual camera if it changed.
pub(crate) fn remember_model(model: &str) {
    if crate::selection::save_model(model) {
        rename();
    }
}

/// Starts the elevated rename task.
fn rename() {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("schtasks")
        .args(["/run", "/tn", TASK])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok();
}

/// The elevated half. Renames every camera interface named "Viewfinder", waiting up to three
/// seconds for Windows to finish making it.
#[expect(unsafe_code, reason = "setting a device property is a Win32 call")]
#[expect(
    clippy::disallowed_methods,
    reason = "runs in the elevated rename process, which has no window to keep responsive"
)]
pub(crate) fn name_interfaces() -> Result<usize, String> {
    let name = name();
    let value: Vec<u8> = name
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    for _ in 0..10 {
        let mut renamed = 0;
        for path in camera_interfaces()? {
            let current = interface_string(&path, &DEVPKEY_DeviceInterface_FriendlyName);
            if !current.starts_with("Viewfinder") {
                continue;
            }
            name_device(&path, &value);
            if current != name {
                // SAFETY: `path` is NUL-terminated and `value` outlives the call.
                let r = unsafe {
                    CM_Set_Device_Interface_PropertyW(
                        PCWSTR(path.as_ptr()),
                        &DEVPKEY_DeviceInterface_FriendlyName,
                        DEVPROP_TYPE_STRING,
                        Some(&value),
                        0,
                    )
                };
                if r != CR_SUCCESS {
                    return Err(format!(
                        "Windows refused to rename {current} (CONFIGRET {})",
                        r.0
                    ));
                }
            }
            renamed += 1;
        }
        if renamed > 0 {
            return Ok(renamed);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(0)
}

/// Every present camera interface path in every class, NUL-terminated.
fn camera_interfaces() -> Result<Vec<Vec<u16>>, String> {
    let mut all = Vec::new();
    for class in CAMERA_CLASSES {
        all.extend(interfaces(class)?);
    }
    Ok(all)
}

#[expect(unsafe_code, reason = "listing device interfaces is a Win32 call")]
fn interfaces(class: &GUID) -> Result<Vec<Vec<u16>>, String> {
    let mut len = 0;
    // SAFETY: `len` is a live out slot and the class GUID is a static.
    let r = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut len,
            class,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if r != CR_SUCCESS {
        return Err(format!("couldn't list cameras (CONFIGRET {})", r.0));
    }
    let mut list = vec![0u16; len as usize];
    // SAFETY: `list` holds the `len` units Windows asked for.
    let r = unsafe {
        CM_Get_Device_Interface_ListW(
            class,
            PCWSTR::null(),
            &mut list,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if r != CR_SUCCESS {
        return Err(format!("couldn't list cameras (CONFIGRET {})", r.0));
    }
    Ok(list
        .split(|&c| c == 0)
        .filter(|p| !p.is_empty())
        .map(|p| p.iter().copied().chain([0]).collect())
        .collect())
}

/// Names the device behind the interface too, for Device Manager and Settings. Only the
/// interfaces' names reach apps, so a refusal here is let go.
#[expect(unsafe_code, reason = "setting a device property is a Win32 call")]
fn name_device(path: &[u16], value: &[u8]) {
    let id: Vec<u16> = interface_string(path, &DEVPKEY_Device_InstanceId)
        .encode_utf16()
        .chain([0])
        .collect();
    let mut node = 0;
    // SAFETY: `id` is NUL-terminated and `node` is a live out slot.
    let found =
        unsafe { CM_Locate_DevNodeW(&raw mut node, PCWSTR(id.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) };
    if found == CR_SUCCESS {
        // SAFETY: the key is a static and `value` outlives the call.
        unsafe {
            CM_Set_DevNode_PropertyW(
                node,
                &DEVPKEY_Device_FriendlyName,
                DEVPROP_TYPE_STRING,
                Some(value),
                0,
            )
        };
    }
}

#[expect(unsafe_code, reason = "reading a device property is a Win32 call")]
fn interface_string(path: &[u16], key: &DEVPROPKEY) -> String {
    let mut ty = DEVPROPTYPE::default();
    let mut bytes = vec![0u8; 512];
    let mut size = bytes.len() as u32;
    // SAFETY: `path` is NUL-terminated, and `size` tells Windows how big `bytes` is.
    let r = unsafe {
        CM_Get_Device_Interface_PropertyW(
            PCWSTR(path.as_ptr()),
            key,
            &raw mut ty,
            Some(bytes.as_mut_ptr()),
            &raw mut size,
            0,
        )
    };
    if r != CR_SUCCESS || ty != DEVPROP_TYPE_STRING {
        return String::new();
    }
    let wide: Vec<u16> = bytes
        .get(..size as usize)
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&wide)
}
