//! What Setup asks of Windows: NVIDIA's Video Effects, the installer, and the programs that
//! can keep the camera from Viewfinder.

use std::fs::File;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Security::Cryptography::Core::{HashAlgorithmNames, HashAlgorithmProvider};
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::ProcessStatus::K32EnumProcesses;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, PostMessageW, SW_SHOWNORMAL, WM_CLOSE,
};
use windows_core::BOOL;
use windows_core::{HSTRING, PCWSTR, PWSTR};

use crate::worker::{lock, log};

/// NVIDIA's wheel on pypi.nvidia.com and the hash published for it. Only six of its files
/// are needed.
const WHEEL: &str = "nvidia_vfx-0.2.0.0-cp312-abi3-win_amd64.whl";
const URL: &str = "https://pypi.nvidia.com/nvidia-vfx/nvidia_vfx-0.2.0.0-cp312-abi3-win_amd64.whl";
const HASH: &str = "cebaa12411f7e22b39293205c5c4d07b8073d14ca1e0e7727ee4375207a36445";
pub(crate) const WHEEL_BYTES: u64 = 435_807_359;
const NEEDED: [&str; 6] = [
    "cudart64_12.dll",
    "NVCVImage.dll",
    "nvngxruntime.dll",
    "NVVideoEffects.dll",
    "nvVFXVideoSuperRes.dll",
    "nvngx_vsr.dll",
];
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Programs that open the camera and keep it, found by a piece of their file name with the
/// spaces taken out.
const HOLDERS: [(&str, &str); 2] = [
    ("eosutility", "EOS Utility"),
    ("eoswebcam", "EOS Webcam Utility"),
];

/// Where Setup puts NVIDIA's DLLs. They load into this app only, so they don't need Program
/// Files or an administrator.
fn effects_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Viewfinder").join("vfx"))
}

/// NVIDIA's DLLs from `VIEWFINDER_VFX`, `vfx` next to the exe where older installs put them,
/// or where Setup downloads them.
pub(crate) fn vfx_dir() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("vfx")));
    std::env::var_os("VIEWFINDER_VFX")
        .map(PathBuf::from)
        .into_iter()
        .chain(beside)
        .chain(effects_dir())
        .find(|dir| NEEDED.iter().all(|n| dir.join(n).is_file()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Download {
    Idle,
    /// Bytes of the wheel on disk so far.
    Fetching(u64),
    Unpacking,
    Done,
    Failed(String),
}

pub(crate) fn download(state: Arc<Mutex<Download>>, repaint: impl Fn() + Send + 'static) {
    *lock(&state) = Download::Fetching(0);
    std::thread::spawn(move || {
        crate::mta();
        let set = |d| {
            *lock(&state) = d;
            repaint();
        };
        let done = fetch(&|got| set(Download::Fetching(got))).and_then(|wheel| {
            set(Download::Unpacking);
            unpack(&wheel)
        });
        set(match done {
            Ok(()) => Download::Done,
            Err(e) => {
                log(&format!("NVIDIA Video Effects download failed: {e}"));
                Download::Failed(e)
            }
        });
    });
}

fn system(file: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map_or_else(|| r"C:\Windows".into(), PathBuf::from);
    root.join("System32").join(file)
}

/// curl.exe and tar.exe have come with Windows since 2018, so there's no HTTP or zip code here.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the download thread, which only watches curl"
)]
fn fetch(progress: &dyn Fn(u64)) -> Result<PathBuf, String> {
    let wheel = std::env::temp_dir().join(WHEEL);
    let size = || std::fs::metadata(&wheel).map_or(0, |m| m.len());
    if size() > WHEEL_BYTES {
        std::fs::remove_file(&wheel).ok();
    }
    if size() < WHEEL_BYTES {
        // Picks up where an earlier try stopped.
        let mut curl = Command::new(system("curl.exe"))
            .args(["--fail", "--location", "--silent", "--show-error"])
            .args(["--retry", "3", "--continue-at", "-", "--output"])
            .arg(&wheel)
            .arg(URL)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("Couldn't start curl.exe ({e})."))?;
        let _job = tie_to_app(&curl);
        let ended = loop {
            progress(size());
            if let Some(ended) = curl.try_wait().map_err(|e| e.to_string())? {
                break ended;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        if !ended.success() {
            let mut said = String::new();
            if let Some(mut err) = curl.stderr.take() {
                err.read_to_string(&mut said).ok();
            }
            return Err(format!(
                "The download stopped ({}). Check the internet connection and try again.",
                said.trim()
            ));
        }
    }
    if sha256(&wheel)? != HASH {
        std::fs::remove_file(&wheel).ok();
        return Err(
            "The download doesn't match the hash NVIDIA published, so it wasn't used. Try again."
                .into(),
        );
    }
    Ok(wheel)
}

/// Puts curl in a job that Windows closes along with this process, so quitting mid-download
/// can't leave it writing to the file the next try appends to.
#[expect(unsafe_code, reason = "job objects are Win32 calls")]
fn tie_to_app(child: &std::process::Child) -> Option<Job> {
    // SAFETY: an unnamed job with default security.
    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.ok()?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `limits` is a live local of the size given, and the child's handle is open while
    // `child` lives.
    let tied = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .and_then(|()| AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())))
    };
    let job = Job(job);
    tied.is_ok().then_some(job)
}

/// Closing it ends curl if it's still running.
struct Job(HANDLE);

impl Drop for Job {
    #[expect(unsafe_code, reason = "closing a handle is a Win32 call")]
    fn drop(&mut self) {
        // SAFETY: the job was made by `tie_to_app` and nothing else closes it.
        unsafe { CloseHandle(self.0) }.ok();
    }
}

/// Unpacks beside the real folder first, so a half-finished one is never picked up.
fn unpack(wheel: &Path) -> Result<(), String> {
    let dir = effects_dir().ok_or("LOCALAPPDATA isn't set, so there's nowhere to put them.")?;
    let part = dir.with_extension("part");
    std::fs::remove_dir_all(&part).ok();
    std::fs::create_dir_all(&part)
        .map_err(|e| format!("Couldn't make {} ({e}).", part.display()))?;
    let out = Command::new(system("tar.exe"))
        .arg("-xf")
        .arg(wheel)
        .arg("-C")
        .arg(&part)
        .args(["--strip-components", "2"])
        .args(NEEDED.map(|n| format!("nvvfx/libs/{n}")))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Couldn't start tar.exe ({e})."))?;
    if !out.status.success() {
        return Err(format!(
            "Couldn't unpack the download ({}).",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // Moved aside whole first. Deleting in place would take the files that aren't loaded and
    // leave a folder missing the ones that are.
    let old = dir.with_extension("old");
    std::fs::remove_dir_all(&old).ok();
    if dir.exists() {
        std::fs::rename(&dir, &old).map_err(|e| {
            // Sharing and lock violations are what an open file gives.
            if e.kind() == std::io::ErrorKind::PermissionDenied
                || matches!(e.raw_os_error(), Some(32 | 33))
            {
                format!(
                    "NVIDIA's effects are in use. Restart Viewfinder, then press Try Again. ({e})"
                )
            } else {
                format!("Couldn't move the old effects aside ({e}).")
            }
        })?;
    }
    if let Err(e) = std::fs::rename(&part, &dir) {
        std::fs::rename(&old, &dir).ok();
        return Err(format!("Couldn't move it into {} ({e}).", dir.display()));
    }
    std::fs::remove_dir_all(&old).ok();
    std::fs::remove_file(wheel).ok();
    Ok(())
}

fn sha256(path: &Path) -> Result<String, String> {
    let fail = |e: windows_core::Error| format!("Couldn't check the download ({e}).");
    let hash = HashAlgorithmNames::Sha256()
        .and_then(|name| HashAlgorithmProvider::OpenAlgorithm(&name))
        .and_then(|provider| provider.CreateHash())
        .map_err(fail)?;
    let mut file =
        File::open(path).map_err(|e| format!("Couldn't read {} ({e}).", path.display()))?;
    let mut chunk = vec![0; 1 << 20];
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|e| format!("Couldn't read {} ({e}).", path.display()))?;
        let Some(read) = chunk.get(..n).filter(|r| !r.is_empty()) else {
            break;
        };
        CryptographicBuffer::CreateFromByteArray(read)
            .and_then(|buffer| hash.Append(&buffer))
            .map_err(fail)?;
    }
    let hex = hash
        .GetValueAndReset()
        .and_then(|value| CryptographicBuffer::EncodeToHexString(&value))
        .map_err(fail)?;
    Ok(hex.to_string().to_lowercase())
}

/// A process id and the program's name.
pub(crate) type Holder = (u32, &'static str);

/// Running programs that hold the camera.
#[expect(unsafe_code, reason = "listing processes is a Win32 call")]
pub(crate) fn holders() -> Vec<Holder> {
    let mut pids = vec![0u32; 4096];
    let mut bytes = 0u32;
    // SAFETY: the size given is the buffer's own, and `bytes` is a live local.
    let listed = unsafe {
        K32EnumProcesses(
            pids.as_mut_ptr(),
            (pids.len() * size_of::<u32>()) as u32,
            &raw mut bytes,
        )
    };
    if !listed.as_bool() {
        return Vec::new();
    }
    pids.truncate((bytes as usize).div_euclid(size_of::<u32>()));
    pids.into_iter()
        .filter_map(|pid| Some((pid, holder(&file_name(pid)?)?)))
        .collect()
}

fn holder(file: &str) -> Option<&'static str> {
    let squashed = file.to_lowercase().replace(' ', "");
    HOLDERS
        .iter()
        .find(|(piece, _)| squashed.contains(piece))
        .map(|&(_, name)| name)
}

#[expect(unsafe_code, reason = "reading a process's file name is a Win32 call")]
fn file_name(pid: u32) -> Option<String> {
    // SAFETY: takes plain values, and a process that's gone just fails.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: `len` says how many characters `path` holds, and both outlive the call.
    let read = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &raw mut len,
        )
    };
    // SAFETY: the handle was opened above and isn't used again.
    unsafe { CloseHandle(process) }.ok();
    read.ok()?;
    let path = String::from_utf16_lossy(path.get(..len as usize)?);
    path.rsplit('\\').next().map(str::to_owned)
}

/// Asks the program's windows to close first, so it can hand the camera back cleanly, and
/// only ends it if it's still there after a few seconds. Checks the name again first, in case
/// the id has gone to another program since the scan.
#[expect(unsafe_code, reason = "closing and ending a process are Win32 calls")]
pub(crate) fn end(pid: u32, name: &str) -> Result<(), String> {
    if file_name(pid).as_deref().and_then(holder).is_none() {
        return Ok(());
    }
    // SAFETY: the callback only reads the pid passed through `LPARAM`.
    unsafe { EnumWindows(Some(close_window), LPARAM(pid.try_into().unwrap_or(-1))) }.ok();
    if gone_within(pid, 3000) {
        return Ok(());
    }
    let refused = || {
        format!(
            "Windows won't let Viewfinder close {name}. Close it from its own window or the tray."
        )
    };
    // SAFETY: takes plain values, and a process that's gone just fails.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }.map_err(|_| refused())?;
    // SAFETY: the handle was just opened with PROCESS_TERMINATE.
    let ended = unsafe { TerminateProcess(process, 0) };
    // SAFETY: the handle was opened above and isn't used again.
    unsafe { CloseHandle(process) }.ok();
    ended.map_err(|_| refused())?;
    if gone_within(pid, 2000) {
        Ok(())
    } else {
        Err(refused())
    }
}

#[expect(unsafe_code, reason = "a window enumeration callback")]
unsafe extern "system" fn close_window(window: HWND, pid: LPARAM) -> BOOL {
    let mut owner = 0u32;
    // SAFETY: `owner` is a live local.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut owner)) };
    if isize::try_from(owner).ok() == Some(pid.0) {
        // SAFETY: posting a message to a window that may close meanwhile just fails.
        unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) }.ok();
    }
    BOOL::from(true)
}

#[expect(unsafe_code, reason = "waiting on a process is a Win32 call")]
fn gone_within(pid: u32, ms: u32) -> bool {
    // SAFETY: takes plain values, and a process that's gone just fails.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return file_name(pid).is_none();
    };
    // SAFETY: the handle was just opened with PROCESS_SYNCHRONIZE.
    let gone = unsafe { WaitForSingleObject(process, ms) } == WAIT_OBJECT_0;
    // SAFETY: the handle was opened above and isn't used again.
    unsafe { CloseHandle(process) }.ok();
    gone
}

/// Whether the virtual camera Windows has registered is the DLL beside this exe, so an old
/// install somewhere else still counts as not installed.
#[expect(unsafe_code, reason = "reading the registry is a Win32 call")]
pub(crate) fn registered_here() -> bool {
    let Some(dll) = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("viewfinder_vcam.dll")))
    else {
        return false;
    };
    let key = HSTRING::from(format!(
        r"SOFTWARE\Classes\CLSID\{}\InprocServer32",
        shared::CLSID_STR
    ));
    let mut path = [0u16; 1024];
    let mut size = size_of_val(&path) as u32;
    // SAFETY: `size` says how many bytes `path` holds, and both outlive the call.
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &key,
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(path.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if read.is_err() {
        return false;
    }
    let len = (size as usize).div_euclid(2).saturating_sub(1);
    path.get(..len)
        .map(String::from_utf16_lossy)
        .is_some_and(|p| p.eq_ignore_ascii_case(&dll.to_string_lossy()))
}

/// What `ShellExecute` returned, where 32 and below are errors.
#[expect(
    unsafe_code,
    reason = "starting a program through the shell is a Win32 call"
)]
fn shell(verb: &str, file: &Path, args: &str) -> isize {
    // SAFETY: the strings are live HSTRINGs for the length of the call.
    let started = unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from(verb),
            &HSTRING::from(file),
            &HSTRING::from(args),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    started.0 as isize
}

pub(crate) fn open(url: &str) {
    shell("open", Path::new(url), "");
}

/// Next to the exe in a release zip and an install, or in scripts in a checkout. The checkout
/// is only looked for beside a release build, since from Program Files it would be
/// C:\scripts, which any user can make, and the script runs as administrator.
pub(crate) fn install_script() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let checkout = dir
        .ends_with(r"target\release")
        .then(|| dir.join(r"..\..\scripts\install.ps1"));
    [Some(dir.join("install.ps1")), checkout]
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
}

/// Runs install.ps1 as administrator. It waits while Windows asks, so it can't be on the UI
/// thread.
/// The script is told who asked, since an administrator's password at the prompt runs it as
/// that administrator, and the shortcuts and login start belong to the person who asked.
#[expect(unsafe_code, reason = "ShellExecute wants COM on its thread")]
pub(crate) fn install(script: &Path) -> Result<(), String> {
    // SAFETY: this thread belongs to the installer and does nothing else with COM.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
        .ok()
        .ok();
    let var = |name| std::env::var(name).unwrap_or_default();
    let args = format!(
        "-NoProfile -ExecutionPolicy Bypass -File \"{}\" -User \"{}\\{}\"",
        script.display(),
        var("USERDOMAIN"),
        var("USERNAME")
    );
    match shell(
        "runas",
        &system(r"WindowsPowerShell\v1.0\powershell.exe"),
        &args,
    ) {
        code if code > 32 => Ok(()),
        // SE_ERR_ACCESSDENIED, which is also what saying no at the prompt gives.
        5 => Err("Windows didn't get permission, so nothing changed.".into()),
        code => Err(format!(
            "Windows couldn't start the installer (error {code}), so nothing changed."
        )),
    }
}

/// Starts a new copy that waits for this one to quit, since only one copy runs at a time.
pub(crate) fn restart() -> bool {
    std::env::current_exe().is_ok_and(|exe| {
        Command::new(exe)
            .args(["--after", &std::process::id().to_string()])
            .spawn()
            .is_ok()
    })
}

/// The copy `restart` starts waits here until the old one has quit.
#[expect(unsafe_code, reason = "waiting on a process is a Win32 call")]
pub(crate) fn wait_after_restart(args: &[String]) {
    let Some(pid) = args
        .iter()
        .position(|a| a == "--after")
        .and_then(|i| args.get(i + 1)?.parse::<u32>().ok())
    else {
        return;
    };
    // SAFETY: takes plain values, and a process that's gone just fails.
    if let Ok(process) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        // SAFETY: the handle was just opened with PROCESS_SYNCHRONIZE.
        unsafe { WaitForSingleObject(process, 30_000) };
        // SAFETY: the handle was opened above and isn't used again.
        unsafe { CloseHandle(process) }.ok();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn canon_programs_are_known_by_file_name() {
        assert_eq!(super::holder("EOS Utility 3.exe"), Some("EOS Utility"));
        assert_eq!(super::holder("EOSUtility.exe"), Some("EOS Utility"));
        assert_eq!(
            super::holder("EOS Webcam Utility Pro.exe"),
            Some("EOS Webcam Utility")
        );
        assert_eq!(super::holder("viewfinder.exe"), None);
    }
}
