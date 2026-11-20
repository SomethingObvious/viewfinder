//! Windows adds " (Windows Virtual Camera)" to a virtual camera's name and denies its app
//! `AddProperty`, but an administrator can set the interface's name. So install.ps1 registers a
//! task that runs `viewfinder.exe --name-camera` elevated.

use std::os::windows::process::CommandExt;
use std::time::Duration;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL,
    CM_Locate_DevNodeW, CM_Set_DevNode_PropertyW, CM_Set_Device_Interface_PropertyW,
    CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_FriendlyName, DEVPKEY_Device_InstanceId, DEVPKEY_DeviceInterface_FriendlyName,
    DEVPROP_TYPE_STRING, DEVPROPTYPE,
};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
    MFCreateVirtualCamera, MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_core::{GUID, HSTRING, PCWSTR, PWSTR};

const TASK: &str = "Viewfinder Camera Name";
/// Where the app keeps its camera's device path, so the elevated rename touches only that one.
const LINK: &str = "camera-link.txt";
/// Each class keeps its own name. Media Foundation apps read `KSCATEGORY_VIDEO_CAMERA`, and
/// browsers, Discord and OBS read `KSCATEGORY_CAPTURE` and `KSCATEGORY_VIDEO`.
const CAMERA_CLASSES: &[GUID] = &[
    GUID::from_u128(0xe5323777_f976_4f5b_9b55_b94699c46e44),
    GUID::from_u128(0x65e8773d_8f56_11d0_a3b9_00a0c9223196),
    GUID::from_u128(0x6994ad05_93ef_11d0_a3cc_00a0c9223196),
];

pub(crate) struct VirtualCamera(IMFVirtualCamera);

impl VirtualCamera {
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
        // Wrapped first, so a camera that fails to start is still shut down before release.
        let camera = Self(camera);
        // SAFETY: the camera was just created, and no callback is passed.
        unsafe { camera.0.Start(None) }.map_err(|e| {
            format!(
                "Viewfinder isn't installed, so call apps can't see it. Setup in General can install it. ({e})"
            )
        })?;
        remember_link(&camera.0);
        rename();
        Ok(camera)
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

pub(crate) fn remember_model(model: &str) {
    if crate::selection::save_model(model) {
        rename();
    }
}

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

#[expect(unsafe_code, reason = "setting a device property is a Win32 call")]
#[expect(
    clippy::disallowed_methods,
    reason = "runs in the elevated rename process, which has no window to keep responsive"
)]
pub(crate) fn name_interfaces() -> Result<usize, String> {
    let mut problem = String::new();
    for _ in 0..10 {
        let mut renamed = 0;
        // Both read each round, as the app writes them while its camera starts, and the task
        // ignores a second run while one is going.
        let name = name();
        let value: Vec<u8> = name
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        // With no saved path, which a copy from before it was saved leaves, every Viewfinder
        // camera counts. A saved one is waited for, as another user's Viewfinder camera
        // could otherwise take its name.
        let ours = match our_link() {
            Some(l) => {
                let device = interface_string(&wide(&l), &DEVPKEY_Device_InstanceId);
                if device.is_empty() {
                    "the camera's device never showed up".clone_into(&mut problem);
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                Some(device)
            }
            None => None,
        };
        // Right after the camera is made its interfaces are still arriving, so a list that
        // fails is tried again with the rest.
        let lists = match CAMERA_CLASSES
            .iter()
            .map(interfaces)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(lists) => lists,
            Err(e) => {
                problem = e;
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
        };
        for path in lists.concat() {
            let current = interface_string(&path, &DEVPKEY_DeviceInterface_FriendlyName);
            // The path comes from a file the user can write and this can run as SYSTEM, so it
            // only ever picks out a virtual camera that's Viewfinder's or not named yet, never
            // a real webcam.
            let viewfinder = current.starts_with("Viewfinder");
            let mine = ours.as_ref().map_or(viewfinder, |device| {
                device.starts_with(r"SWD\VCAMDEVAPI\")
                    && (viewfinder || current.is_empty())
                    && interface_string(&path, &DEVPKEY_Device_InstanceId) == *device
            });
            if !mine {
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
        "found no Viewfinder camera to rename".clone_into(&mut problem);
        std::thread::sleep(Duration::from_millis(300));
    }
    // Exits non-zero, so Task Scheduler shows why the name didn't change.
    Err(problem)
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
    let mut r = unsafe {
        CM_Get_Device_Interface_ListW(
            class,
            PCWSTR::null(),
            &mut list,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    // A camera that arrives between the two calls makes the list too small for its size.
    for _ in 0..4 {
        if r != CR_BUFFER_SMALL {
            break;
        }
        list.resize(list.len() * 2 + 256, 0);
        // SAFETY: as above, with `list` grown.
        r = unsafe {
            CM_Get_Device_Interface_ListW(
                class,
                PCWSTR::null(),
                &mut list,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
    }
    if r != CR_SUCCESS {
        return Err(format!("couldn't list cameras (CONFIGRET {})", r.0));
    }
    Ok(list
        .split(|&c| c == 0)
        .filter(|p| !p.is_empty())
        .map(|p| p.iter().copied().chain([0]).collect())
        .collect())
}

/// For Device Manager and Settings. Only the interface names reach apps, so a refusal is let go.
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

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// The camera's device path, which Windows hands out once it starts.
#[expect(unsafe_code, reason = "reading an attribute is a COM call")]
fn remember_link(camera: &IMFVirtualCamera) {
    let mut text = PWSTR::null();
    let mut len = 0;
    // SAFETY: both out slots are live locals, and the string is freed below.
    let read = unsafe {
        camera.GetAllocatedString(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            &raw mut text,
            &raw mut len,
        )
    };
    // SAFETY: on success Windows wrote a NUL-terminated string there.
    let link = read.ok().and_then(|()| unsafe { text.to_string() }.ok());
    // SAFETY: the string came from GetAllocatedString, and null is allowed.
    unsafe { CoTaskMemFree(Some(text.0.cast_const().cast())) };
    match link {
        // A stale path from an earlier session would point the rename at a camera that's gone.
        Some(link) if crate::store::write(LINK, &link) => {}
        _ => drop(crate::store::path(LINK).map(std::fs::remove_file)),
    }
}

/// From this user's settings, or as SYSTEM, from the settings of the user install.ps1
/// recorded. It's only ever compared with real device paths, so a file that isn't one does
/// nothing.
fn our_link() -> Option<String> {
    let text = crate::store::read(LINK).or_else(|| {
        let sid = registry_string(r"SOFTWARE\Viewfinder", "User")?;
        let profile = registry_string(
            &format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}"),
            "ProfileImagePath",
        )?;
        let path = std::path::Path::new(&profile)
            .join(r"AppData\Roaming\Viewfinder")
            .join(LINK);
        std::fs::read_to_string(path).ok()
    })?;
    let link = text.trim();
    (link.starts_with(r"\\?\") && link.len() < 512 && !link.chars().any(char::is_control))
        .then(|| link.to_owned())
}

#[expect(unsafe_code, reason = "reading the registry is a Win32 call")]
fn registry_string(key: &str, value: &str) -> Option<String> {
    let mut text = [0u16; 1024];
    let mut size = size_of_val(&text) as u32;
    // SAFETY: `size` says how many bytes `text` holds, and both outlive the call. A REG_EXPAND_SZ
    // comes back expanded.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    }
    .ok()
    .ok()?;
    let len = (size as usize).div_euclid(2).saturating_sub(1);
    Some(String::from_utf16_lossy(text.get(..len)?))
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
