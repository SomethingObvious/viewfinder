//! Any video capture device Windows knows, through Media Foundation: UVC webcams, HDMI and USB
//! capture cards, and cameras with their own webcam drivers.

use std::ptr::null_mut;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Open_Device_Interface_KeyW, CR_SUCCESS, RegDisposition_OpenExisting,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSourceReader,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
    MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED, MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED, MF_LOW_LATENCY,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE, MF_MT_YUV_MATRIX,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFCreateAttributes, MFCreateDeviceSource,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFSTARTUP_FULL, MFShutdown,
    MFStartup, MFVideoFormat_MJPG, MFVideoFormat_NV12, MFVideoFormat_YUY2,
    MFVideoTransferMatrix_BT601, MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{HKEY, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW};
use windows_core::{GUID, HSTRING, Interface, PWSTR, w};

use crate::{Choice, Error, Format, Mta, Pixels, Result, Source};

/// The pipeline never sends apps more than this, so a bigger mode only costs bandwidth.
const MAX_SIZE: (u32, u32) = (1920, 1080);
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// How a capture mode's frames are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Coding {
    /// Uncompressed 4:2:2, the most bandwidth for the picture.
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

/// The index of the mode to stream: the biggest that fits 1080p at 30 fps or more, then
/// NV12 before MJPG before YUY2. With nothing that fast, the fastest of the biggest.
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
    /// The device interface path, which opens it.
    pub link: String,
}

/// Starts Media Foundation for as long as it lives. The calls nest.
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

/// The media source CLSID a Windows 11 virtual camera was made with, from its interface key.
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

/// Whether a device is Viewfinder's own virtual camera, which would feed back, or Canon's EOS
/// Webcam Utility, which wants the same camera.
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

/// Every video capture device Viewfinder may stream from, in the order Windows lists them.
///
/// # Errors
///
/// Fails when Media Foundation won't start or list devices.
pub fn devices() -> Result<Vec<Device>> {
    let _mta = Mta::enter()?;
    let _mf = Mf::start()?;
    Ok(activates()?
        .iter()
        .map(|a| Device {
            name: string(a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME),
            link: string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK),
        })
        .filter(|d| !excluded(d, virtual_source(&d.link).as_deref()))
        .collect())
}

/// Every video device in [`devices`] as a [`Choice`].
///
/// # Errors
///
/// Fails when Media Foundation won't start or list devices.
pub fn choices() -> Result<Vec<Choice>> {
    Ok(devices()?
        .into_iter()
        .map(|d| Choice::Video {
            link: d.link,
            name: d.name,
        })
        .collect())
}

/// Opens the webcam `choice` names, see [`Choice::find`].
///
/// # Errors
///
/// Fails with [`Error::NotFound`] when it isn't plugged in, or as [`Webcam::open`] does.
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
    mode: Mode,
    format: Format,
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
// SAFETY: Media Foundation's device source and source reader are free-threaded objects, and
// every call comes from an MTA thread, which `Mta` keeps alive.
unsafe impl Send for Webcam {}

impl Webcam {
    /// Opens a device from [`devices`] and sets it to its [`best`] mode.
    ///
    /// # Errors
    ///
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
        let opened = (|| {
            let ra = attributes(1)?;
            // SAFETY: the key is a static. Low latency stops the reader queueing frames.
            unsafe { ra.SetUINT32(&MF_LOW_LATENCY, 1) }.map_err(failed("can't set low latency"))?;
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
            Ok((reader, format_of(*picked, matrix), *picked))
        })();
        let (reader, format, mode) = match opened {
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
            mode,
            format,
            _mf: mf,
            _mta: mta,
        })
    }

    /// The mode the device is streaming.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }
}

/// What a mode's frames come out as, given the colour matrix its media type names.
const fn format_of(mode: Mode, matrix: Option<u32>) -> Format {
    let bt709 = match matrix {
        Some(m) if m == MFVideoTransferMatrix_BT709.0 as u32 => true,
        Some(m) if m == MFVideoTransferMatrix_BT601.0 as u32 => false,
        // Unlabelled HD video is BT.709 and SD is BT.601, as in every video standard.
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
    out.clear();
    // A 2D buffer can pad its rows, and copying it out contiguously drops the padding.
    if let Ok(two) = buffer.cast::<IMF2DBuffer>() {
        // SAFETY: the buffer is live.
        let len = unsafe { two.GetContiguousLength()? } as usize;
        out.resize(len, 0);
        // SAFETY: `out` holds exactly the contiguous length.
        return unsafe { two.ContiguousCopyTo(out) };
    }
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
        let mut flags = 0;
        let mut sample = None;
        // SAFETY: the out slots are live locals. This blocks until the next frame.
        unsafe {
            self.reader.ReadSample(
                STREAM,
                0,
                None,
                Some(&raw mut flags),
                None,
                Some(&raw mut sample),
            )
        }
        .map_err(failed("lost the camera's stream"))?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Err(Error::Disconnected);
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
        // Shutdown lets the device go, which turns its light off.
        // SAFETY: the source is live and nothing uses it after this.
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

    /// Checks a real webcam's frames by size and timing only, never what they show.
    /// `VF_TEST_CAMERA` names the camera.
    #[test]
    #[ignore = "turns on a real camera"]
    fn streams_from_a_real_webcam() {
        use std::time::Instant;

        let _mta = Mta::enter().unwrap();
        let _mf = Mf::start().unwrap();
        for a in activates().unwrap() {
            let name = string(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
            let link = string(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK);
            println!(
                "listed {name:?}, virtual source {:?}",
                virtual_source(&link)
            );
        }
        let all = devices().unwrap();
        for d in &all {
            println!("kept {:?}", d.name);
        }
        assert!(all.iter().all(|d| !d.name.starts_with("Viewfinder")));
        let want = std::env::var("VF_TEST_CAMERA").unwrap_or_else(|_| "Integrated Camera".into());
        let device = all.iter().find(|d| d.name == want).expect("no such camera");

        let opened = Instant::now();
        let webcam = Webcam::open(device).unwrap();
        println!("opened in {:?}, mode {:?}", opened.elapsed(), webcam.mode());
        let mut cam: Box<dyn Source> = Box::new(webcam);
        let mut out = Vec::new();
        let mut rgb = Vec::new();
        let mut gaps = Vec::new();
        let (mut frames, mut last) = (0, Instant::now());
        let started = Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(2) {
            let Some(format) = cam.frame(&mut out).unwrap() else {
                continue;
            };
            gaps.push(last.elapsed().as_secs_f32() * 1000.0);
            last = Instant::now();
            frames += 1;
            match format {
                Format::Jpeg => {
                    let mut d = zune_jpeg::JpegDecoder::new(
                        zune_jpeg::zune_core::bytestream::ZCursor::new(&out),
                    );
                    d.decode_headers().unwrap();
                    let (w, h) = d.dimensions().unwrap();
                    assert!(d.decode().is_ok(), "MJPG frame didn't decode");
                    if frames == 1 {
                        println!("MJPG {w}x{h}, {} bytes", out.len());
                    }
                }
                Format::Raw {
                    pixels,
                    width,
                    height,
                    bt709,
                } => {
                    crate::convert::to_rgb(pixels, width, height, bt709, &out, &mut rgb).unwrap();
                    assert_eq!(rgb.len(), width as usize * height as usize * 3);
                    if frames == 1 {
                        println!(
                            "{pixels:?} {width}x{height} bt709={bt709}, {} bytes",
                            out.len()
                        );
                    }
                }
            }
        }
        let secs = started.elapsed().as_secs_f32();
        gaps.remove(0);
        gaps.sort_by(f32::total_cmp);
        let median = gaps.get(gaps.len().div_euclid(2)).copied().unwrap_or(0.0);
        let worst = gaps.last().copied().unwrap_or(0.0);
        println!(
            "{frames} frames in {secs:.2} s, {:.1} fps, median gap {median:.1} ms, worst {worst:.1} ms",
            frames as f32 / secs
        );
        assert!(frames > 10);
        cam.close().unwrap();
    }
}
