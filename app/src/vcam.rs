//! Owns the Windows virtual camera for as long as the app runs, and its name.
//!
//! Windows adds " (Windows Virtual Camera)" to every virtual camera's name and refuses to let
//! the app that made it change that (`AddProperty` gets access denied). An administrator can
//! still set the interface's name, so install.ps1 registers a task that runs
//! `viewfinder.exe --name-camera` elevated, and the app starts it each time it makes the camera.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_PropertyW,
    CM_Set_Device_Interface_PropertyW, CR_SUCCESS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_DeviceInterface_FriendlyName, DEVPROP_TYPE_STRING, DEVPROPTYPE,
};
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFCreateVirtualCamera, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime_Session, MFVirtualCameraType_SoftwareCameraSource,
};
use windows_core::{GUID, HSTRING, PCWSTR};

/// The task install.ps1 registers.
pub(crate) const TASK: &str = "Viewfinder Camera Name";
/// `KSCATEGORY_VIDEO_CAMERA`, the interface class apps list cameras from.
const CAMERA_CLASS: GUID = GUID::from_u128(0xe5323777_f976_4f5b_9b55_b94699c46e44);

pub(crate) struct VirtualCamera(IMFVirtualCamera);

impl VirtualCamera {
    /// Registers the camera for this user. It goes away when this is dropped
    /// or the process exits, so no app is left holding a dead camera.
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
            format!("The camera DLL isn't installed. Run install.ps1 as administrator. ({e})")
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

fn model_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Viewfinder").join("camera.txt"))
}

/// "Viewfinder (Canon EOS REBEL T3)" once a camera has been seen, plain "Viewfinder" before.
fn name() -> String {
    let model = model_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let model: String = model
        .lines()
        .next()
        .unwrap_or("")
        .trim()
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
    let Some(path) = model_path() else { return };
    if std::fs::read_to_string(&path).is_ok_and(|m| m.trim() == model) {
        return;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if std::fs::write(&path, model).is_ok() {
        rename();
    }
}

/// Starts the elevated rename task. Without it, or before install.ps1 made it, the camera
/// keeps the suffix Windows gave it.
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

/// The elevated half. Renames every camera interface whose name starts with "Viewfinder",
/// waiting up to three seconds for Windows to finish making it.
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
            let current = interface_name(&path);
            if !current.starts_with("Viewfinder") {
                continue;
            }
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

/// Every present camera interface path, NUL-terminated.
#[expect(unsafe_code, reason = "listing device interfaces is a Win32 call")]
fn camera_interfaces() -> Result<Vec<Vec<u16>>, String> {
    let mut len = 0;
    // SAFETY: `len` is a live out slot and the class GUID is a static.
    let r = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut len,
            &CAMERA_CLASS,
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
            &CAMERA_CLASS,
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

#[expect(unsafe_code, reason = "reading a device property is a Win32 call")]
fn interface_name(path: &[u16]) -> String {
    let mut ty = DEVPROPTYPE::default();
    let mut bytes = vec![0u8; 512];
    let mut size = bytes.len() as u32;
    // SAFETY: `path` is NUL-terminated, and `size` tells Windows how big `bytes` is.
    let r = unsafe {
        CM_Get_Device_Interface_PropertyW(
            PCWSTR(path.as_ptr()),
            &DEVPKEY_DeviceInterface_FriendlyName,
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
