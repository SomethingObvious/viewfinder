//! Opens "Viewfinder" through Media Foundation the way a call app does, behind Frame
//! Server, and reports frame rate and timing. It never keeps or shows a picture, only
//! counts frames and checks whether they're the grey placeholder.
//!
//! `cargo run --release -p vcam --example consumer -- 1920 1080 15`

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSource, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_VERSION, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows_core::PWSTR;

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// What the read loop saw.
struct Stats {
    samples: u32,
    placeholder: u32,
    distinct: u32,
    first_real: Option<Duration>,
    gaps: Vec<f64>,
    secs: f64,
}

#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn main() -> windows_core::Result<()> {
    let args: Vec<u32> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let (width, height, secs) = (
        *args.first().unwrap_or(&1920),
        *args.get(1).unwrap_or(&1080),
        *args.get(2).unwrap_or(&15),
    );
    // SAFETY: called once on this thread before any other COM or Media Foundation call.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let device = find_viewfinder()?;
    let opened = Instant::now();
    let (source, reader) = open(&device, width, height)?;
    println!("opened {width}x{height} NV12 in {:?}", opened.elapsed());
    let stats = measure(&reader, (width * height) as usize, secs)?;
    report(stats);
    // SAFETY: the source and reader came from Media Foundation above, and MFShutdown matches
    // the MFStartup at the top.
    unsafe {
        source.Shutdown()?;
        drop(reader);
        MFShutdown()?;
    }
    Ok(())
}

/// Lists every video capture device and returns the one named Viewfinder.
#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn find_viewfinder() -> windows_core::Result<IMFActivate> {
    // SAFETY: every out pointer points at a live local, the device list is read only for the
    // `count` entries Media Foundation reported, and each string and the list are freed once
    // with CoTaskMemFree as the API requires.
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&raw mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| {
            windows_core::Error::new(windows::Win32::Foundation::E_POINTER, "no attributes")
        })?;
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        let (mut list, mut count) = (std::ptr::null_mut(), 0);
        MFEnumDeviceSources(&attrs, &raw mut list, &raw mut count)?;
        let devices = std::slice::from_raw_parts(list, count as usize);
        let mut found = None;
        for d in devices.iter().flatten() {
            let (mut name, mut len) = (PWSTR::null(), 0);
            d.GetAllocatedString(
                &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
                &raw mut name,
                &raw mut len,
            )?;
            let text = name.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name.0.cast()));
            println!("camera: {text}");
            if text.contains("Viewfinder") {
                found = Some(d.clone());
            }
        }
        CoTaskMemFree(Some(list.cast()));
        found.ok_or_else(|| {
            windows_core::Error::new(windows::Win32::Foundation::E_FAIL, "no Viewfinder camera")
        })
    }
}

/// Activates the source and asks its reader for NV12 at the given size.
#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn open(
    device: &IMFActivate,
    width: u32,
    height: u32,
) -> windows_core::Result<(IMFMediaSource, IMFSourceReader)> {
    // SAFETY: plain COM calls on interfaces that Media Foundation handed back, with no raw
    // pointers involved.
    unsafe {
        let source: IMFMediaSource = device.ActivateObject()?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, None)?;
        let wanted = MFCreateMediaType()?;
        wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        wanted.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        reader.SetCurrentMediaType(STREAM, None, &wanted)?;
        Ok((source, reader))
    }
}

/// Reads samples for `secs` seconds, counting placeholders and distinct frames.
#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn measure(reader: &IMFSourceReader, luma: usize, secs: u32) -> windows_core::Result<Stats> {
    let (mut samples, mut placeholder, mut distinct) = (0u32, 0u32, 0u32);
    let (mut first_real, mut last_hash) = (None, 0u64);
    let mut gaps = Vec::new();
    let mut last_ts = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs.into()) {
        let (mut flags, mut ts, mut sample) = (0u32, 0i64, None);
        // SAFETY: every out pointer points at a live local.
        unsafe {
            reader.ReadSample(
                STREAM,
                0,
                None,
                Some(&raw mut flags),
                Some(&raw mut ts),
                Some(&raw mut sample),
            )?;
        }
        let Some(sample) = sample else { continue };
        samples += 1;
        if let Some(prev) = last_ts {
            gaps.push((ts - prev) as f64 / 1e4);
        }
        last_ts = Some(ts);
        // SAFETY: the buffer stays locked while `y` is read, `y` covers no more than the `len`
        // bytes Lock reported, and it is not used after Unlock.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (mut ptr, mut len) = (std::ptr::null_mut(), 0);
            buffer.Lock(&raw mut ptr, None, Some(&raw mut len))?;
            let y = std::slice::from_raw_parts(ptr, (len as usize).min(luma));
            if y.iter().step_by(97).all(|&b| b == 40) {
                placeholder += 1;
            } else {
                first_real.get_or_insert_with(|| start.elapsed());
                let mut h = DefaultHasher::new();
                y.iter().step_by(13).collect::<Vec<_>>().hash(&mut h);
                let h = h.finish();
                if h != last_hash {
                    distinct += 1;
                    last_hash = h;
                }
            }
            buffer.Unlock()?;
        }
    }
    Ok(Stats {
        samples,
        placeholder,
        distinct,
        first_real,
        gaps,
        secs: start.elapsed().as_secs_f64(),
    })
}

fn report(mut stats: Stats) {
    let secs = stats.secs;
    stats.gaps.sort_by(f64::total_cmp);
    println!(
        "{} samples in {secs:.1} s ({:.1}/s), {} placeholder",
        stats.samples,
        f64::from(stats.samples) / secs,
        stats.placeholder
    );
    if let Some(at) = stats.first_real {
        let live = secs - at.as_secs_f64();
        println!(
            "first camera frame after {:.2} s, then {:.1} distinct frames/s",
            at.as_secs_f64(),
            f64::from(stats.distinct) / live
        );
    }
    let median = stats.gaps.get(stats.gaps.len().div_euclid(2));
    if let (Some(median), Some(max)) = (median, stats.gaps.last()) {
        println!("timestamp gaps: median {median:.1} ms, max {max:.1} ms");
    }
}
