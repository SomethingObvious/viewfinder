//! Webcams and capture cards through Media Foundation.

use std::ptr::null_mut;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Open_Device_Interface_KeyW, CR_SUCCESS, RegDisposition_OpenExisting,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample,
    IMFSourceReader, IMFSourceReaderCallback, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
    MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED, MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED, MF_LOW_LATENCY,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE, MF_MT_YUV_MATRIX,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ASYNC_CALLBACK,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED, MF_VERSION,
    MFCreateAttributes, MFCreateDeviceSource, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoFormat_MJPG,
    MFVideoFormat_NV12, MFVideoFormat_YUY2, MFVideoTransferMatrix_BT601,
    MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{HKEY, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW};
use windows_core::{GUID, HSTRING, Interface, PWSTR, w};

use crate::{Choice, Error, Format, Mta, Pixels, Result, Source};

/// Apps never get more than this, so a bigger mode only costs bandwidth.
const MAX_SIZE: (u32, u32) = (1920, 1080);
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// A webcam whose frames stop for this long has hung, and is opened again. Even 5 fps in the
/// dark comes well inside it.
const STALL: Duration = Duration::from_secs(5);
/// A capture card with no signal sends nothing at all, so a first frame gets longer.
const FIRST_FRAME: Duration = Duration::from_secs(30);
/// How long one `frame` call waits, so the camera thread still sees quit and switches.
const STEP: Duration = Duration::from_millis(250);

/// What the reader hands back for one `ReadSample`.
struct Arrival {
    status: windows_core::HRESULT,
    flags: u32,
    sample: Option<IMFSample>,
}

#[expect(
    unsafe_code,
    reason = "the sample interface pointer isn't Send on its own"
)]
#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "Media Foundation samples are free-threaded"
)]
// SAFETY: Media Foundation samples are free-threaded.
unsafe impl Send for Arrival {}

#[expect(
    unsafe_code,
    trivial_casts,
    clippy::ref_as_ptr,
    clippy::inline_always,
    reason = "the COM glue #[implement] generates casts vtables and implements unsafe methods"
)]
mod arrivals {
    use std::sync::mpsc::Sender;

    use windows::Win32::Media::MediaFoundation::{
        IMFMediaEvent, IMFSample, IMFSourceReaderCallback, IMFSourceReaderCallback_Impl,
    };
    use windows_core::{HRESULT, Ref, Result, implement};

    use super::Arrival;

    /// Passes each sample from the reader's thread to the camera thread.
    #[derive(Debug)]
    #[implement(IMFSourceReaderCallback)]
    pub(super) struct Arrivals(pub(super) Sender<Arrival>);

    impl IMFSourceReaderCallback_Impl for Arrivals_Impl {
        fn OnReadSample(
            &self,
            status: HRESULT,
            _stream: u32,
            flags: u32,
            _time: i64,
            sample: Ref<'_, IMFSample>,
        ) -> Result<()> {
            // Nobody listening means the webcam closed, which is fine.
            self.0
                .send(Arrival {
                    status,
                    flags,
                    sample: sample.cloned(),
                })
                .ok();
            Ok(())
        }

        fn OnFlush(&self, _stream: u32) -> Result<()> {
            Ok(())
        }

        fn OnEvent(&self, _stream: u32, _event: Ref<'_, IMFMediaEvent>) -> Result<()> {
            Ok(())
        }
    }
}

/// How a capture mode's frames are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Coding {
    /// Uncompressed 4:2:2.
    Yuy2,
    /// Motion JPEG, which most webcams need for their bigger sizes.
    Mjpg,
    /// Uncompressed 4:2:0.
    Nv12,
}

/// One native capture mode of a device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mode {
    /// How frames are coded.
    pub coding: Coding,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second.
    pub fps: f32,
}

/// The biggest mode that fits 1080p at 30 fps, then NV12 before MJPG before YUY2.
#[must_use]
pub fn best(modes: &[Mode]) -> Option<usize> {
    let key = |m: &Mode| {
        let area = i64::from(m.width) * i64::from(m.height);
        let fits = m.width <= MAX_SIZE.0 && m.height <= MAX_SIZE.1;
        // NTSC rates come in at 29.97.
        let smooth = m.fps >= 29.9;
        (
            smooth,
            fits,
            if fits { area } else { -area },
            m.coding,
            (m.fps * 100.0) as i64,
        )
    };
    (0..modes.len()).max_by_key(|&i| modes.get(i).map(key))
}

/// A video capture device Media Foundation lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The name Windows shows.
    pub name: String,
    /// The device interface path.
    pub link: String,
}

/// Keeps Media Foundation started while it lives. The calls nest.
#[derive(Debug)]
struct Mf;

impl Mf {
    #[expect(unsafe_code, reason = "starting Media Foundation is a Win32 call")]
    fn start() -> Result<Self> {
        // SAFETY: takes constants only.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map(|()| Self)
            .map_err(|e| Error::Failed(format!("Media Foundation didn't start ({e})")))
    }
}

impl Drop for Mf {
    #[expect(unsafe_code, reason = "stopping Media Foundation is a Win32 call")]
    fn drop(&mut self) {
        // SAFETY: pairs with the MFStartup in `start`.
        unsafe { MFShutdown() }.ok();
    }
}

fn failed(what: &str) -> impl Fn(windows_core::Error) -> Error + '_ {
    move |e| {
        let gone = [
            MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
            MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED,
        ];
        if gone.contains(&e.code()) {
            Error::Disconnected
        } else {
            Error::Failed(format!("{what} ({e})"))
        }
    }
}

#[expect(unsafe_code, reason = "attribute stores are COM objects")]
fn attributes(n: u32) -> Result<IMFAttributes> {
    let mut a = None;
    // SAFETY: `a` is a live out slot.
    unsafe { MFCreateAttributes(&raw mut a, n) }.map_err(failed("no attribute store"))?;
    a.ok_or_else(|| Error::Failed("no attribute store".into()))
}

#[expect(unsafe_code, reason = "the string comes back in COM task memory")]
fn string(a: &IMFAttributes, key: &GUID) -> String {
    let mut p = PWSTR::null();
    let mut len = 0;
    // SAFETY: both out slots are live locals.
    if unsafe { a.GetAllocatedString(key, &raw mut p, &raw mut len) }.is_err() || p.is_null() {
        return String::new();
    }
    // SAFETY: not null, and Media Foundation hands back NUL-terminated strings.
    let s = unsafe { p.to_string() }.unwrap_or_default();
    // SAFETY: allocated with CoTaskMemAlloc and owned by this function.
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    s
}

/// The media source CLSID of a Windows 11 virtual camera.
#[expect(unsafe_code, reason = "the device interface key is read through Win32")]
fn virtual_source(link: &str) -> Option<String> {
    let mut key = HKEY::default();
    // SAFETY: the path is a live wide string and `key` a live out slot.
    let r = unsafe {
        CM_Open_Device_Interface_KeyW(
            &HSTRING::from(link),
            KEY_READ.0,
            RegDisposition_OpenExisting,
            &raw mut key,
            0,
        )
    };
    if r != CR_SUCCESS {
        return None;
    }
    let mut buf = [0u16; 64];
    let mut len = size_of_val(&buf) as u32;
    // SAFETY: the buffer and its length in bytes are live locals, and the key is open.
    let got = unsafe {
        RegGetValueW(
            key,
            None,
            w!("CustomCaptureSourceClsid"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut len),
        )
    };
    // SAFETY: opened above and not used after this.
    unsafe { RegCloseKey(key) }.ok().ok();
    got.ok().ok()?;
    let units = buf
        .iter()
        .take_while(|&&u| u != 0)
        .copied()
        .collect::<Vec<_>>();
    Some(String::from_utf16_lossy(&units))
}

/// Viewfinder's own virtual camera would feed back, and Canon's EOS Webcam Utility wants the
/// same camera.
fn excluded(d: &Device, source: Option<&str>) -> bool {
    let link = d.link.to_ascii_lowercase();
    d.name.starts_with("Viewfinder")
        || link.contains("root#eoswebcamsource")
        || source.is_some_and(|s| s.eq_ignore_ascii_case(shared::CLSID_STR))
}

#[expect(unsafe_code, reason = "device enumeration is a Media Foundation call")]
fn activates() -> Result<Vec<IMFActivate>> {
    let a = attributes(1)?;
    // SAFETY: both GUIDs are statics.
    unsafe {
        a.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(failed("can't ask for video devices"))?;
    let mut list = null_mut();
    let mut n = 0;
    // SAFETY: both out slots are live locals.
    unsafe { MFEnumDeviceSources(&a, &raw mut list, &raw mut n) }
        .map_err(failed("can't list video devices"))?;
    if list.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: Media Foundation returned `n` slots at `list`, each owning one reference.
    let slots = unsafe { std::slice::from_raw_parts_mut(list, n as usize) };
    let out = slots.iter_mut().filter_map(Option::take).collect();
    // SAFETY: the array came from CoTaskMemAlloc, and its references were all taken out above.
    unsafe { CoTaskMemFree(Some(list.cast())) };
    Ok(out)
}

/// Every video capture device Viewfinder may stream from.
/// # Errors
/// Fails when Media Foundation won't start or list devices.
pub fn choices() -> Result<Vec<Choice>> {
    let _mta = Mta::enter()?;
    let _mf = Mf::start()?;
    Ok(activates()?
        .iter()
        .map(|a| Device {
            name: string(a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME),
            link: string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK),
        })
        .filter(|d| !excluded(d, virtual_source(&d.link).as_deref()))
        .map(|d| Choice::Video {
            link: d.link,
            name: d.name,
        })
        .collect())
}

/// Opens the webcam `choice` names.
/// # Errors
/// Fails when it isn't plugged in, or as [`Webcam::open`] does.
pub fn open_choice(choice: &Choice) -> Result<Webcam> {
    let mut found = choices()?;
    match choice.find(&found).map(|i| found.swap_remove(i)) {
        Some(Choice::Video { link, name }) => Webcam::open(&Device { name, link }),
        _ => Err(Error::NotFound),
    }
}

#[expect(unsafe_code, reason = "reading a media type is a COM call")]
fn mode(t: &IMFMediaType) -> Option<Mode> {
    // SAFETY: the keys are statics.
    let (subtype, size, rate) = unsafe {
        (
            t.GetGUID(&MF_MT_SUBTYPE).ok()?,
            t.GetUINT64(&MF_MT_FRAME_SIZE).ok()?,
            t.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0),
        )
    };
    let coding = match subtype {
        s if s == MFVideoFormat_NV12 => Coding::Nv12,
        s if s == MFVideoFormat_MJPG => Coding::Mjpg,
        s if s == MFVideoFormat_YUY2 => Coding::Yuy2,
        _ => return None,
    };
    let (num, den) = ((rate >> 32) as u32, rate as u32);
    Some(Mode {
        coding,
        width: (size >> 32) as u32,
        height: size as u32,
        fps: if den == 0 {
            0.0
        } else {
            num as f32 / den as f32
        },
    })
}

/// A Media Foundation device streaming its best mode.
#[derive(Debug)]
pub struct Webcam {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    name: String,
    format: Format,
    arrivals: Receiver<Arrival>,
    /// A `ReadSample` is out and its answer hasn't come yet.
    asked: bool,
    /// A frame has come since it opened.
    flowing: bool,
    /// When it opened or last sent something.
    heard: Instant,
    _mf: Mf,
    _mta: Mta,
}

#[expect(
    unsafe_code,
    reason = "the Media Foundation interface pointers aren't Send on their own"
)]
#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the media source and source reader are free-threaded"
)]
// SAFETY: the device source and source reader are free-threaded, and `Mta` keeps the MTA up.
unsafe impl Send for Webcam {}

impl Webcam {
    /// Opens a device in its [`best`] mode.
    /// # Errors
    /// Fails when the device is gone, busy in another app, or has no mode Viewfinder can read.
    #[expect(unsafe_code, reason = "Media Foundation is COM")]
    pub fn open(device: &Device) -> Result<Self> {
        let mta = Mta::enter()?;
        let mf = Mf::start()?;
        let a = attributes(2)?;
        // SAFETY: the keys are statics and the link outlives the call.
        unsafe {
            a.SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .and_then(|()| {
                a.SetString(
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                    &HSTRING::from(&device.link),
                )
            })
        }
        .map_err(failed("can't name the device"))?;
        // SAFETY: the attributes are live.
        let source =
            unsafe { MFCreateDeviceSource(&a) }.map_err(failed("can't open the camera"))?;
        let (sender, arrivals): (Sender<Arrival>, _) = channel();
        let callback: IMFSourceReaderCallback = arrivals::Arrivals(sender).into();
        let opened = (|| {
            let ra = attributes(2)?;
            // SAFETY: the keys are statics. Low latency stops the reader queueing frames.
            unsafe {
                ra.SetUINT32(&MF_LOW_LATENCY, 1)
                    .and_then(|()| ra.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback))
            }
            .map_err(failed("can't set the reader up"))?;
            // SAFETY: both are live.
            let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &ra) }
                .map_err(failed("can't read the camera"))?;
            let usable: Vec<(IMFMediaType, Mode)> = (0..)
                // SAFETY: a type past the last one is an error, which ends the list.
                .map_while(|i| unsafe { reader.GetNativeMediaType(STREAM, i) }.ok())
                .filter_map(|t| mode(&t).map(|m| (t, m)))
                .collect();
            let modes: Vec<Mode> = usable.iter().map(|&(_, m)| m).collect();
            let (chosen, picked) = best(&modes).and_then(|i| usable.get(i)).ok_or_else(|| {
                Error::Unsupported(format!("{} has no NV12, YUY2 or MJPG mode", device.name))
            })?;
            // SAFETY: the type came from this reader, and the stream constants are Windows'.
            unsafe {
                reader
                    .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                    .and_then(|()| reader.SetStreamSelection(STREAM, true))
                    .and_then(|()| reader.SetCurrentMediaType(STREAM, None, chosen))
            }
            .map_err(failed("the camera refused its own mode"))?;
            // SAFETY: the key is a static.
            let matrix = unsafe { chosen.GetUINT32(&MF_MT_YUV_MATRIX) }.ok();
            Ok((reader, format_of(*picked, matrix)))
        })();
        let (reader, format) = match opened {
            Ok(o) => o,
            Err(e) => {
                // SAFETY: the source is live and not used after this.
                unsafe { source.Shutdown() }.ok();
                return Err(e);
            }
        };
        Ok(Self {
            reader,
            source,
            name: device.name.clone(),
            format,
            arrivals,
            asked: false,
            flowing: false,
            heard: Instant::now(),
            _mf: mf,
            _mta: mta,
        })
    }
}

const fn format_of(mode: Mode, matrix: Option<u32>) -> Format {
    let bt709 = match matrix {
        Some(m) if m == MFVideoTransferMatrix_BT709.0 as u32 => true,
        Some(m) if m == MFVideoTransferMatrix_BT601.0 as u32 => false,
        // Unlabelled HD is BT.709 and SD is BT.601.
        _ => mode.height >= 720,
    };
    let pixels = match mode.coding {
        Coding::Mjpg => return Format::Jpeg,
        Coding::Nv12 => Pixels::Nv12,
        Coding::Yuy2 => Pixels::Yuy2,
    };
    Format::Raw {
        pixels,
        width: mode.width,
        height: mode.height,
        bt709,
    }
}

#[expect(unsafe_code, reason = "a sample's memory is read through COM")]
fn copy_out(
    buffer: &windows::Win32::Media::MediaFoundation::IMFMediaBuffer,
    out: &mut Vec<u8>,
) -> windows_core::Result<()> {
    // A 2D buffer can pad its rows, and the contiguous copy drops the padding.
    if let Ok(two) = buffer.cast::<IMF2DBuffer>() {
        // SAFETY: the buffer is live.
        let len = unsafe { two.GetContiguousLength()? } as usize;
        // The copy fills every byte, so a frame the same size as the last skips the zeroing.
        out.resize(len, 0);
        // SAFETY: `out` holds exactly the contiguous length.
        return unsafe { two.ContiguousCopyTo(out) };
    }
    out.clear();
    let mut p = null_mut();
    let mut len = 0;
    // SAFETY: both out slots are live locals, and Unlock pairs with this.
    unsafe { buffer.Lock(&raw mut p, None, Some(&raw mut len))? };
    if !p.is_null() {
        // SAFETY: locked, and the buffer holds `len` valid bytes.
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(p, len as usize) });
    }
    // SAFETY: locked above.
    unsafe { buffer.Unlock() }
}

impl Source for Webcam {
    fn model(&self) -> &str {
        &self.name
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    #[expect(unsafe_code, reason = "reading a sample is a Media Foundation call")]
    fn frame(&mut self, out: &mut Vec<u8>) -> Result<Option<Format>> {
        if !self.asked {
            // SAFETY: the reader is async, so every out slot stays empty and the answer comes
            // through the callback.
            unsafe { self.reader.ReadSample(STREAM, 0, None, None, None, None) }
                .map_err(failed("lost the camera's stream"))?;
            self.asked = true;
        }
        let Arrival {
            status,
            flags,
            sample,
        } = match self.arrivals.recv_timeout(STEP) {
            Ok(arrival) => arrival,
            Err(RecvTimeoutError::Timeout) => {
                let limit = if self.flowing { STALL } else { FIRST_FRAME };
                if self.heard.elapsed() < limit {
                    return Ok(None);
                }
                let what = if self.flowing {
                    "stopped sending frames"
                } else {
                    "hasn't sent a frame"
                };
                return Err(Error::Failed(format!("{} {what}", self.name)));
            }
            Err(RecvTimeoutError::Disconnected) => return Err(Error::Disconnected),
        };
        self.asked = false;
        self.heard = Instant::now();
        status.ok().map_err(failed("lost the camera's stream"))?;
        self.flowing |= sample.is_some();
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Err(Error::Disconnected);
        }
        // A webcam can change size mid-stream, after its own settings panel or a USB squeeze,
        // and every later frame is the new size.
        let changed = (MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0
            | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0) as u32;
        if flags & changed != 0 {
            // SAFETY: the stream index is Windows' constant for the first video stream.
            let current = unsafe { self.reader.GetCurrentMediaType(STREAM) }
                .map_err(failed("can't read the camera's new mode"))?;
            let picked = mode(&current).ok_or_else(|| {
                Error::Unsupported(format!(
                    "{} switched to a mode Viewfinder can't read",
                    self.name
                ))
            })?;
            // SAFETY: the key is a static.
            let matrix = unsafe { current.GetUINT32(&MF_MT_YUV_MATRIX) }.ok();
            self.format = format_of(picked, matrix);
        }
        // A gap in the stream comes back with no sample.
        let Some(sample) = sample else {
            return Ok(None);
        };
        // SAFETY: the sample is live.
        let buffer =
            unsafe { sample.ConvertToContiguousBuffer() }.map_err(failed("empty frame"))?;
        copy_out(&buffer, out).map_err(failed("can't read the frame"))?;
        Ok((!out.is_empty()).then_some(self.format))
    }

    fn close(self: Box<Self>) -> Result<()> {
        drop(self);
        Ok(())
    }
}

impl Drop for Webcam {
    #[expect(unsafe_code, reason = "shutting the source down is a COM call")]
    fn drop(&mut self) {
        // SAFETY: the source is live and unused after this. Shutdown turns the light off.
        unsafe { self.source.Shutdown() }.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(coding: Coding, width: u32, height: u32, fps: f32) -> Mode {
        Mode {
            coding,
            width,
            height,
            fps,
        }
    }

    #[test]
    fn best_takes_the_biggest_smooth_mode_that_fits() {
        let modes = [
            m(Coding::Yuy2, 640, 480, 30.0),
            m(Coding::Yuy2, 1920, 1080, 5.0),
            m(Coding::Mjpg, 1280, 720, 30.0),
            m(Coding::Mjpg, 1920, 1080, 30.0),
            m(Coding::Mjpg, 3840, 2160, 30.0),
            m(Coding::Nv12, 1280, 720, 30.0),
        ];
        assert_eq!(best(&modes), Some(3));
        assert_eq!(best(modes.get(..3).unwrap()), Some(2));
        assert_eq!(
            best(&[modes[2], modes[5]]),
            Some(1),
            "NV12 over MJPG at one size"
        );
        assert_eq!(
            best(&[modes[4], modes[0]]),
            Some(1),
            "4K is more than apps get"
        );
        let ntsc = [
            m(Coding::Yuy2, 1920, 1080, 29.97),
            m(Coding::Yuy2, 1920, 1080, 59.94),
        ];
        assert_eq!(best(&ntsc), Some(1));
        let slow = [
            m(Coding::Yuy2, 640, 480, 15.0),
            m(Coding::Yuy2, 1280, 720, 10.0),
        ];
        assert_eq!(best(&slow), Some(1));
        assert_eq!(best(&[]), None);
    }

    #[test]
    fn excludes_its_own_virtual_camera_and_canons_old_utility() {
        let d = |name: &str, link: &str| Device {
            name: name.into(),
            link: link.into(),
        };
        let vcam = r"\\?\swd#vcamdevapi#ad06#{e5323777-f976-4f5b-9b55-b94699c46e44}\{fcebba03-9d13-4c13-9940-cc84fcd132d1}";
        assert!(excluded(&d("Viewfinder (Canon EOS REBEL T3)", vcam), None));
        assert!(excluded(
            &d("Renamed", vcam),
            Some(&shared::CLSID_STR.to_lowercase())
        ));
        assert!(!excluded(
            &d("Pixel 11 Pro (Windows Virtual Camera)", vcam),
            Some("{E9F83CF2-E0C0-4CA7-AF01-E90C70BEF496}")
        ));
        let eos = r"\\?\ROOT#EOSWebcamSource#0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\EOSWebcamCameraSource";
        assert!(excluded(&d("EOS Webcam Utility", eos), None));
        assert!(!excluded(
            &d(
                "Integrated Camera",
                r"\\?\usb#vid_30c9&pid_00ac&mi_00#8&198797cf&1&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global"
            ),
            None
        ));
    }
}
