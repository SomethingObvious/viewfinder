//! The frame handoff between the Viewfinder app and its camera DLL.
//!
//! The DLL runs inside the Frame Server service (LOCAL SERVICE, session 0)
//! and creates the mapping, since a normal user can't create `Global\`
//! objects. The app opens it, reads what size and format the DLL wants,
//! and writes frames into three slots guarded by a sequence counter.

use std::ptr::NonNull;
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
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows_core::{GUID, HSTRING};

/// CLSID of the media source the camera DLL exports.
pub const CLSID: GUID = GUID::from_u128(0xD6F48B83_FB1F_4421_BE38_B81B93E940ED);
/// [`CLSID`] in registry form.
pub const CLSID_STR: &str = "{D6F48B83-FB1F-4421-BE38-B81B93E940ED}";

/// `Local\` only works when the DLL is hosted in the user's own session,
/// which is what the in-process tests do.
const NAMES: [&str; 2] = ["Global\\Viewfinder-v1", "Local\\Viewfinder-v1"];

const MAGIC: u32 = u32::from_le_bytes(*b"VFv1");
/// How many frames the mapping holds, so the app can write one while the DLL reads another.
pub const SLOTS: usize = 3;
/// The widest frame either side can ask for.
pub const MAX_WIDTH: u32 = 1920;
/// The tallest frame either side can ask for.
pub const MAX_HEIGHT: u32 = 1080;
/// YUY2 at the largest size is the biggest frame either side can ask for.
pub const SLOT_BYTES: usize = (MAX_WIDTH * MAX_HEIGHT * 2) as usize;
const HEADER_BYTES: usize = 4096;
const MAPPING_BYTES: usize = HEADER_BYTES + SLOTS * SLOT_BYTES;

/// The NV12 fourcc, a Y plane then interleaved UV at half height.
pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
/// The YUY2 fourcc, Y0 U Y1 V for each pair of pixels.
pub const YUY2: u32 = u32::from_le_bytes(*b"YUY2");

/// LOCAL SERVICE, SYSTEM and admins get full access, and the interactive
/// user can read and write. Nobody else can plant frames.
const SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)";

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
    // Written by the DLL.
    want_width: AtomicU32,
    want_height: AtomicU32,
    want_fourcc: AtomicU32,
    /// `GetTickCount64` of the last sample request, 0 once the stream stops.
    consumer_tick: AtomicU64,
    // Written by the app.
    producer_tick: AtomicU64,
    /// Index of the newest finished slot, or `u32::MAX` before the first.
    latest: AtomicU32,
    _pad: u32,
    slots: [Slot; SLOTS],
    /// `MFGetSystemTime` of the DLL's next read, written by the DLL. Last so the
    /// fields before it keep their offsets.
    next_read: AtomicI64,
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
    /// How many bytes one tightly packed frame takes.
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
}

// SAFETY: the mapping is only touched through atomics and seqlocked slot copies.
#[expect(unsafe_code, reason = "the raw view pointer isn't Send on its own")]
unsafe impl Send for Shared {}
// SAFETY: as for Send.
#[expect(unsafe_code, reason = "the raw view pointer isn't Sync on its own")]
unsafe impl Sync for Shared {}

impl Shared {
    /// Creates the mapping, or opens it if it already exists. Called by the DLL.
    ///
    /// # Errors
    ///
    /// Fails when Windows won't create or open the mapping under either name, or won't map it.
    #[expect(
        unsafe_code,
        reason = "security descriptors and file mappings are Win32 calls"
    )]
    pub fn create() -> windows_core::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL string is valid and `sd` is a live out pointer, freed with LocalFree below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(SDDL),
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
        for name in NAMES {
            // SAFETY: `sa` outlives the call, and INVALID_HANDLE_VALUE asks for a pagefile mapping.
            result = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    Some(&raw const sa),
                    PAGE_READWRITE,
                    0,
                    MAPPING_BYTES as u32,
                    &HSTRING::from(name),
                )
            };
            // Opening an existing mapping asks for full access, which only
            // LOCAL SERVICE has, so a second source in a user process opens it.
            if result.as_ref().is_err_and(|e| e.code() == E_ACCESSDENIED) {
                // SAFETY: the name is a live HSTRING.
                result = unsafe {
                    OpenFileMappingW(
                        (FILE_MAP_READ | FILE_MAP_WRITE).0,
                        false,
                        &HSTRING::from(name),
                    )
                };
            }
            if result.is_ok() {
                break;
            }
        }
        // SAFETY: `sd` came from ConvertStringSecurityDescriptorToSecurityDescriptorW, which
        // says to free it with LocalFree, and nothing uses it after this.
        unsafe { LocalFree(Some(HLOCAL(sd.0))) };
        let shared = Self::map(result?)?;
        let header = shared.header();
        if header.magic.load(Ordering::Acquire) != MAGIC {
            header.latest.store(u32::MAX, Ordering::Relaxed);
            header.magic.store(MAGIC, Ordering::Release);
        }
        Ok(shared)
    }

    /// Opens a mapping the DLL made.
    ///
    /// # Errors
    ///
    /// Fails while no app is using the camera, since the mapping doesn't exist yet.
    #[expect(unsafe_code, reason = "OpenFileMappingW is a Win32 call")]
    pub fn open() -> windows_core::Result<Self> {
        let mut result = Err(windows_core::Error::empty());
        for name in NAMES {
            // SAFETY: the name is a live HSTRING.
            result = unsafe {
                OpenFileMappingW(
                    (FILE_MAP_READ | FILE_MAP_WRITE).0,
                    false,
                    &HSTRING::from(name),
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
            |base| Ok(Self { handle, base }),
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

    // DLL side.

    /// Says which size and format the DLL wants frames in.
    pub fn set_wanted(&self, f: Format) {
        let h = self.header();
        h.want_width.store(f.width, Ordering::Relaxed);
        h.want_height.store(f.height, Ordering::Relaxed);
        h.want_fourcc.store(f.fourcc, Ordering::Relaxed);
    }

    /// Marks the stream as running (`true`) or stopped.
    pub fn touch_consumer(&self, streaming: bool) {
        let tick = if streaming { now_ms() } else { 0 };
        self.header().consumer_tick.store(tick, Ordering::Release);
    }

    /// Copies the newest frame into `dst` if it matches `want` and was
    /// written in the last `max_age_ms`. Returns the frame number.
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

    /// Records when the next sample goes out, in `MFGetSystemTime` units.
    pub fn set_next_read(&self, at: i64) {
        self.header().next_read.store(at, Ordering::Release);
    }

    // App side.

    /// When the DLL reads next, in `MFGetSystemTime` units, or 0 if it never said.
    #[must_use]
    pub fn next_read(&self) -> i64 {
        self.header().next_read.load(Ordering::Acquire)
    }

    /// What the DLL wants right now, or `None` when no app is streaming.
    /// A stream counts as stopped once its last request is `idle_ms` old.
    #[must_use]
    pub fn wanted(&self, idle_ms: u64) -> Option<Format> {
        let h = self.header();
        let tick = h.consumer_tick.load(Ordering::Acquire);
        if tick == 0 || now_ms().saturating_sub(tick) > idle_ms {
            return None;
        }
        let f = Format {
            width: h.want_width.load(Ordering::Relaxed),
            height: h.want_height.load(Ordering::Relaxed),
            fourcc: h.want_fourcc.load(Ordering::Relaxed),
        };
        f.valid().then_some(f)
    }

    /// Hands `fill` the next free slot to write a frame of format `f` into,
    /// then publishes it unless `fill` returned false. Only one thread may write.
    ///
    /// # Panics
    ///
    /// Panics if `f` isn't a format [`Shared::wanted`] could return.
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
            // Never published, since `latest` still points at the previous slot.
            slot.seq.fetch_add(1, Ordering::Release);
            return;
        }
        slot.width.store(f.width, Ordering::Relaxed);
        slot.height.store(f.height, Ordering::Relaxed);
        slot.fourcc.store(f.fourcc, Ordering::Relaxed);
        slot.frame.store(frame, Ordering::Relaxed);
        slot.seq.fetch_add(1, Ordering::Release);
        h.latest.store(i as u32, Ordering::Release);
        self.heartbeat();
    }

    /// Tells the DLL the app is alive even when no frame is new.
    pub fn heartbeat(&self) {
        self.header()
            .producer_tick
            .store(now_ms(), Ordering::Release);
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
