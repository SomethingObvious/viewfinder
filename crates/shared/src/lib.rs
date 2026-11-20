//! The frame handoff between the app and its camera DLL. The DLL runs in the Frame Server
//! service (LOCAL SERVICE, session 0) and creates the mapping, since a normal user can't create
//! `Global\` objects.

use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering, fence};

use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows_core::{GUID, HSTRING};

/// CLSID of the media source the camera DLL exports.
pub const CLSID: GUID = GUID::from_u128(0xD6F48B83_FB1F_4421_BE38_B81B93E940ED);
/// [`CLSID`] in registry form.
pub const CLSID_STR: &str = "{D6F48B83-FB1F-4421-BE38-B81B93E940ED}";

/// `Local\` only works when the DLL is hosted in the user's own session, as in the tests.
/// `VIEWFINDER_SHARED` renames the mapping so a test doesn't meet the installed app's.
fn names() -> [String; 2] {
    let base = std::env::var("VIEWFINDER_SHARED").unwrap_or_else(|_| "Viewfinder-v1".into());
    [format!("Global\\{base}"), format!("Local\\{base}")]
}

const MAGIC: u32 = u32::from_le_bytes(*b"VFv1");
const SLOTS: usize = 3;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const SLOT_BYTES: usize = (MAX_WIDTH * MAX_HEIGHT * 2) as usize;
const HEADER_BYTES: usize = 4096;
const MAPPING_BYTES: usize = HEADER_BYTES + SLOTS * SLOT_BYTES;

/// The NV12 fourcc.
pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
/// The YUY2 fourcc.
pub const YUY2: u32 = u32::from_le_bytes(*b"YUY2");

/// LOCAL SERVICE, SYSTEM and admins get full access, and the user who installed Viewfinder can
/// read and write, so nobody else signed in can watch or replace the picture. Without a record
/// of who that is, any signed-in user can.
fn sddl() -> String {
    let user = owner().unwrap_or_else(|| "IU".into());
    format!("D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;{user})")
}

/// The SID install.ps1 records, kept only if it looks like one, since it goes into the SDDL.
#[expect(unsafe_code, reason = "reading the registry is a Win32 call")]
fn owner() -> Option<String> {
    let mut sid = [0u16; 192];
    let mut size = size_of_val(&sid) as u32;
    // SAFETY: `size` says how many bytes `sid` holds, and both outlive the call.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows_core::w!(r"SOFTWARE\Viewfinder"),
            windows_core::w!("User"),
            RRF_RT_REG_SZ,
            None,
            Some(sid.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    }
    .ok()
    .ok()?;
    let len = (size as usize).div_euclid(2).saturating_sub(1);
    let sid = String::from_utf16_lossy(sid.get(..len)?);
    (sid.starts_with("S-1-")
        && sid
            .chars()
            .all(|c| c == 'S' || c == '-' || c.is_ascii_digit()))
    .then_some(sid)
}

#[repr(C)]
struct Slot {
    /// Odd while the app is writing the slot.
    seq: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    fourcc: AtomicU32,
    frame: AtomicU64,
}

#[repr(C)]
struct Header {
    magic: AtomicU32,
    want_width: AtomicU32,
    want_height: AtomicU32,
    want_fourcc: AtomicU32,
    /// `GetTickCount64` of the last sample request, 0 once the stream stops.
    consumer_tick: AtomicU64,
    producer_tick: AtomicU64,
    /// Index of the newest finished slot, or `u32::MAX` before the first.
    latest: AtomicU32,
    _pad: u32,
    slots: [Slot; SLOTS],
    /// `MFGetSystemTime` of the DLL's next read. Last so the fields before it keep their offsets.
    next_read: AtomicI64,
    /// Odd while the DLL changes the wanted format, so the app never reads half of a change.
    /// After `next_read` for the same reason, and an older DLL leaves it at 0.
    want_seq: AtomicU32,
}

const _: () = assert!(size_of::<Header>() <= HEADER_BYTES);

/// Milliseconds since boot, the same clock in every session.
#[must_use]
#[expect(unsafe_code, reason = "GetTickCount64 is a Win32 call")]
pub fn now_ms() -> u64 {
    // SAFETY: GetTickCount64 takes nothing and can't fail.
    unsafe { GetTickCount64() }
}

/// Size and pixel format of one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// [`NV12`] or [`YUY2`].
    pub fourcc: u32,
}

impl Format {
    /// Bytes in one tightly packed frame.
    #[must_use]
    pub const fn bytes(self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.fourcc == YUY2 {
            w * h * 2
        } else {
            (w * h * 3).div_euclid(2)
        }
    }

    const fn valid(self) -> bool {
        (self.fourcc == NV12 || self.fourcc == YUY2)
            && self.width >= 2
            && self.height >= 2
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
            && self.width <= MAX_WIDTH
            && self.height <= MAX_HEIGHT
    }
}

/// The mapped frame handoff, as either side sees it.
#[derive(Debug)]
pub struct Shared {
    handle: HANDLE,
    base: NonNull<u8>,
    /// The last format [`Shared::wanted`] read steadily.
    last_wanted: Mutex<Option<Format>>,
}

// SAFETY: the mapping is only touched through atomics and seqlocked slot copies.
#[expect(unsafe_code, reason = "the raw view pointer isn't Send on its own")]
unsafe impl Send for Shared {}
// SAFETY: as for Send.
#[expect(unsafe_code, reason = "the raw view pointer isn't Sync on its own")]
unsafe impl Sync for Shared {}

impl Shared {
    /// Creates the mapping, or opens it if it already exists. Called by the DLL.
    /// # Errors
    /// When Windows won't create, open or map it under either name.
    #[expect(
        unsafe_code,
        reason = "security descriptors and file mappings are Win32 calls"
    )]
    pub fn create() -> windows_core::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL string is valid and `sd` is a live out pointer, freed below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl()),
                SDDL_REVISION_1,
                &raw mut sd,
                None,
            )?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let mut result = Err(windows_core::Error::empty());
        for name in names() {
            // SAFETY: `sa` outlives the call, and INVALID_HANDLE_VALUE asks for a pagefile mapping.
            result = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    Some(&raw const sa),
                    PAGE_READWRITE,
                    0,
                    MAPPING_BYTES as u32,
                    &HSTRING::from(&name),
                )
            };
            // Creating over an existing mapping asks for full access, which only LOCAL SERVICE
            // has, so a second source in a user process opens it.
            if result.as_ref().is_err_and(|e| e.code() == E_ACCESSDENIED) {
                // SAFETY: the name is a live HSTRING.
                result = unsafe {
                    OpenFileMappingW(
                        (FILE_MAP_READ | FILE_MAP_WRITE).0,
                        false,
                        &HSTRING::from(&name),
                    )
                };
            }
            if result.is_ok() {
                break;
            }
        }
        // SAFETY: the SDDL call says to free `sd` with LocalFree, and nothing uses it after this.
        unsafe { LocalFree(Some(HLOCAL(sd.0))) };
        let shared = Self::map(result?)?;
        let header = shared.header();
        if header.magic.load(Ordering::Acquire) != MAGIC {
            header.latest.store(u32::MAX, Ordering::Relaxed);
            header.magic.store(MAGIC, Ordering::Release);
        }
        Ok(shared)
    }

    /// Opens the mapping the DLL made.
    /// # Errors
    /// While no app is using the camera, since the mapping doesn't exist yet.
    #[expect(unsafe_code, reason = "OpenFileMappingW is a Win32 call")]
    pub fn open() -> windows_core::Result<Self> {
        let mut result = Err(windows_core::Error::empty());
        for name in names() {
            // SAFETY: the name is a live HSTRING.
            result = unsafe {
                OpenFileMappingW(
                    (FILE_MAP_READ | FILE_MAP_WRITE).0,
                    false,
                    &HSTRING::from(&name),
                )
            };
            if result.is_ok() {
                break;
            }
        }
        let shared = Self::map(result?)?;
        if shared.header().magic.load(Ordering::Acquire) != MAGIC {
            return Err(windows_core::Error::empty());
        }
        Ok(shared)
    }

    #[expect(
        unsafe_code,
        reason = "mapping a view and closing a handle are Win32 calls"
    )]
    fn map(handle: HANDLE) -> windows_core::Result<Self> {
        // SAFETY: `handle` is a live mapping at least MAPPING_BYTES long.
        let view =
            unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, MAPPING_BYTES) };
        NonNull::new(view.Value.cast::<u8>()).map_or_else(
            || {
                let err = windows_core::Error::from_thread();
                // SAFETY: the handle is ours and nothing else holds it.
                unsafe { CloseHandle(handle).ok() };
                Err(err)
            },
            |base| {
                Ok(Self {
                    handle,
                    base,
                    last_wanted: Mutex::new(None),
                })
            },
        )
    }

    #[expect(unsafe_code, reason = "the header lives in shared memory")]
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "MapViewOfFile returns a page-aligned view"
    )]
    const fn header(&self) -> &Header {
        // SAFETY: the view is at least HEADER_BYTES long and page-aligned, and every Header field
        // is an atomic or padding, so shared references from both processes are sound.
        unsafe { &*(self.base.as_ptr() as *const Header) }
    }

    #[expect(unsafe_code, reason = "slots live in shared memory")]
    const fn slot_ptr(&self, i: usize) -> *mut u8 {
        // SAFETY: callers pass i < SLOTS, which keeps the offset inside the MAPPING_BYTES view.
        unsafe { self.base.as_ptr().add(HEADER_BYTES + i * SLOT_BYTES) }
    }

    /// DLL side. The size and format it wants.
    pub fn set_wanted(&self, f: Format) {
        let h = self.header();
        // Set rather than added to, so a count left odd by a writer that died mid change
        // comes right on the next one.
        let odd = h.want_seq.load(Ordering::Relaxed) | 1;
        h.want_seq.store(odd, Ordering::Relaxed);
        fence(Ordering::Release);
        h.want_width.store(f.width, Ordering::Relaxed);
        h.want_height.store(f.height, Ordering::Relaxed);
        h.want_fourcc.store(f.fourcc, Ordering::Relaxed);
        h.want_seq.store(odd.wrapping_add(1), Ordering::Release);
    }

    /// DLL side. Marks the stream running or stopped.
    pub fn touch_consumer(&self, streaming: bool) {
        let tick = if streaming { now_ms() } else { 0 };
        self.header().consumer_tick.store(tick, Ordering::Release);
    }

    /// DLL side. Copies the newest frame if it matches `want` and is under `max_age_ms` old.
    #[expect(unsafe_code, reason = "the frame is copied out of shared memory")]
    pub fn read_latest(&self, want: Format, max_age_ms: u64, dst: &mut [u8]) -> Option<u64> {
        let h = self.header();
        if now_ms().saturating_sub(h.producer_tick.load(Ordering::Acquire)) > max_age_ms {
            return None;
        }
        let i = h.latest.load(Ordering::Acquire) as usize;
        let slot = h.slots.get(i)?;
        let len = want.bytes();
        if dst.len() < len || !want.valid() {
            return None;
        }
        let before = slot.seq.load(Ordering::Acquire);
        if before & 1 == 1 {
            return None;
        }
        let got = Format {
            width: slot.width.load(Ordering::Relaxed),
            height: slot.height.load(Ordering::Relaxed),
            fourcc: slot.fourcc.load(Ordering::Relaxed),
        };
        if got != want {
            return None;
        }
        let frame = slot.frame.load(Ordering::Relaxed);
        // SAFETY: `i` indexes a real slot, `len` fits both the slot (want is valid) and `dst`,
        // and the sequence check below throws away a copy the app wrote over meanwhile.
        unsafe { std::ptr::copy_nonoverlapping(self.slot_ptr(i), dst.as_mut_ptr(), len) };
        fence(Ordering::Acquire);
        (slot.seq.load(Ordering::Relaxed) == before).then_some(frame)
    }

    /// DLL side. When the next sample goes out, in `MFGetSystemTime` units.
    pub fn set_next_read(&self, at: i64) {
        self.header().next_read.store(at, Ordering::Release);
    }

    /// App side. When the DLL reads next, or 0 if it never said.
    #[must_use]
    pub fn next_read(&self) -> i64 {
        self.header().next_read.load(Ordering::Acquire)
    }

    /// App side. `None` when no app is streaming, or the last request is `idle_ms` old.
    #[must_use]
    pub fn wanted(&self, idle_ms: u64) -> Option<Format> {
        let h = self.header();
        let tick = h.consumer_tick.load(Ordering::Acquire);
        if tick == 0 || now_ms().saturating_sub(tick) > idle_ms {
            return None;
        }
        // A change takes nanoseconds, but a writer can be preempted in the middle of one, so a
        // miss gives its thread the core before trying again. A steady read is final.
        for try_no in 0..8 {
            if try_no > 0 {
                std::thread::yield_now();
            }
            let before = h.want_seq.load(Ordering::Acquire);
            let f = Format {
                width: h.want_width.load(Ordering::Relaxed),
                height: h.want_height.load(Ordering::Relaxed),
                fourcc: h.want_fourcc.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if before & 1 == 0 && h.want_seq.load(Ordering::Relaxed) == before {
                let f = f.valid().then_some(f);
                if let Ok(mut last) = self.last_wanted.lock() {
                    *last = f;
                }
                return f;
            }
        }
        self.last_wanted.lock().ok().and_then(|last| *last)
    }

    /// App side. Publishes the slot `fill` wrote unless it returned false. One writer only.
    /// # Panics
    /// If `f` isn't a format [`Shared::wanted`] could return.
    #[expect(unsafe_code, reason = "the frame is written into shared memory")]
    pub fn write(&self, f: Format, frame: u64, fill: impl FnOnce(&mut [u8]) -> bool) {
        assert!(f.valid(), "can't write a {f:?} frame");
        let h = self.header();
        let latest = h.latest.load(Ordering::Relaxed);
        let i = if latest as usize >= SLOTS {
            0
        } else {
            (latest as usize + 1) % SLOTS
        };
        let Some(slot) = h.slots.get(i) else {
            return;
        };
        slot.seq.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: `i` is a real slot and a valid format fits SLOT_BYTES. The odd sequence number
        // keeps the DLL from trusting what it reads until this write is published.
        if !fill(unsafe { std::slice::from_raw_parts_mut(self.slot_ptr(i), f.bytes()) }) {
            slot.seq.fetch_add(1, Ordering::Release);
            return;
        }
        slot.width.store(f.width, Ordering::Relaxed);
        slot.height.store(f.height, Ordering::Relaxed);
        slot.fourcc.store(f.fourcc, Ordering::Relaxed);
        slot.frame.store(frame, Ordering::Relaxed);
        slot.seq.fetch_add(1, Ordering::Release);
        h.latest.store(i as u32, Ordering::Release);
        h.producer_tick.store(now_ms(), Ordering::Release);
    }
}

impl Drop for Shared {
    #[expect(unsafe_code, reason = "unmapping and closing are Win32 calls")]
    fn drop(&mut self) {
        // SAFETY: the view and handle are ours, and nothing can use them after drop.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base.as_ptr().cast(),
            })
            .ok();
            CloseHandle(self.handle).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_through_the_mapping() {
        let dll = Shared::create().unwrap();
        let app = Shared::open().unwrap();
        let f = Format {
            width: 64,
            height: 32,
            fourcc: NV12,
        };

        assert_eq!(app.wanted(1000), None);
        dll.set_wanted(f);
        dll.touch_consumer(true);
        assert_eq!(app.wanted(1000), Some(f));

        let mut out = vec![0u8; f.bytes()];
        assert_eq!(dll.read_latest(f, 1000, &mut out), None);

        for n in 1..=5u64 {
            app.write(f, n, |buf| {
                buf.fill(n as u8);
                true
            });
            assert_eq!(dll.read_latest(f, 1000, &mut out), Some(n));
            assert!(out.iter().all(|&b| b == n as u8));
        }

        let other = Format {
            width: 32,
            height: 32,
            fourcc: NV12,
        };
        assert_eq!(dll.read_latest(other, 1000, &mut out), None);

        dll.touch_consumer(false);
        assert_eq!(app.wanted(1000), None);
    }
}
