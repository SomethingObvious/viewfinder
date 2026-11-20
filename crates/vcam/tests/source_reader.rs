//! Drives the source in process through a source reader, without Frame Server or a camera.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use shared::{Format, NV12, Shared, YUY2};
use viewfinder_vcam::{DllCanUnloadNow, DllGetClassObject};
use windows::Win32::Foundation::{E_POINTER, ERROR_SET_NOT_FOUND, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IClassFactory};
use windows_core::{GUID, IUnknown, Interface};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Byte 0 is `k`, so a frame names itself.
fn fill(buf: &mut [u8], k: u8) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i % 251) as u8 ^ k;
    }
}

fn is_pattern(buf: &[u8]) -> bool {
    let first = buf.first().copied().unwrap_or_default();
    buf.iter()
        .enumerate()
        .all(|(i, &b)| b == (i % 251) as u8 ^ first)
}

fn is_placeholder(buf: &[u8], f: Format) -> bool {
    if f.fourcc == YUY2 {
        buf.as_chunks::<2>().0.iter().all(|p| *p == [40, 128])
    } else {
        let (luma, chroma) = buf.split_at((f.width * f.height) as usize);
        luma.iter().all(|&b| b == 40) && chroma.iter().all(|&b| b == 128)
    }
}

fn frame_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

/// Plays the app at about 100 fps.
#[expect(
    clippy::disallowed_methods,
    reason = "a test producer thread, not the UI thread"
)]
fn produce(app: Arc<Shared>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut k = 0u64;
        while !stop.load(Ordering::Relaxed) {
            if let Some(f) = app.wanted(2000) {
                k += 1;
                app.write(f, k, |buf| {
                    fill(buf, k as u8);
                    true
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })
}

#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn read(reader: &IMFSourceReader) -> windows_core::Result<(i64, Vec<u8>)> {
    loop {
        let (mut flags, mut sample) = (0, None);
        // SAFETY: both out pointers point at live locals.
        unsafe {
            reader.ReadSample(
                STREAM,
                0,
                None,
                Some(&raw mut flags),
                None,
                Some(&raw mut sample),
            )?;
        }
        let Some(sample) = sample else { continue };
        // SAFETY: the slice covers the `len` locked bytes and is copied out before Unlock.
        unsafe {
            let time = sample.GetSampleTime()?;
            assert_eq!(sample.GetSampleDuration()?, 333_333);
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (mut ptr, mut len) = (std::ptr::null_mut(), 0);
            buffer.Lock(&raw mut ptr, None, Some(&raw mut len))?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            return Ok((time, bytes));
        }
    }
}

#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn reader_for(source: &IMFMediaSource, f: Format) -> windows_core::Result<IMFSourceReader> {
    let subtype = if f.fourcc == YUY2 {
        MFVideoFormat_YUY2
    } else {
        MFVideoFormat_NV12
    };
    let size = frame_size(f.width, f.height);
    // SAFETY: plain COM calls with no raw pointers.
    unsafe {
        let reader = MFCreateSourceReaderFromMediaSource(source, None)?;
        let mut index = 0;
        let ty = loop {
            let t = reader.GetNativeMediaType(STREAM, index)?;
            if t.GetGUID(&MF_MT_SUBTYPE)? == subtype && t.GetUINT64(&MF_MT_FRAME_SIZE)? == size {
                break t;
            }
            index += 1;
        };
        reader.SetCurrentMediaType(STREAM, None, &ty)?;
        reader.SetStreamSelection(STREAM, true)?;
        Ok(reader)
    }
}

#[expect(unsafe_code, reason = "the DLL entry point and COM calls are unsafe")]
fn activation() -> windows_core::Result<(IClassFactory, IMFActivate)> {
    let mut factory = None;
    // SAFETY: a live `Option<IClassFactory>` has the layout of the pointer written.
    let hr = unsafe {
        DllGetClassObject(
            &shared::CLSID,
            &IClassFactory::IID,
            (&raw mut factory).cast::<*mut std::ffi::c_void>(),
        )
    };
    assert_eq!(hr, S_OK);
    let factory: IClassFactory = factory.ok_or_else(|| windows_core::Error::from(E_POINTER))?;
    // SAFETY: a plain COM call with no outer object.
    let activate: IMFActivate = unsafe { factory.CreateInstance(None::<&IUnknown>) }?;
    Ok((factory, activate))
}

#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn check_frame_server_parts(source: &IMFMediaSourceEx) -> windows_core::Result<()> {
    // SAFETY: KsProperty gets a valid identifier, no buffer and a live out slot.
    unsafe {
        let ks: IKsControl = source.cast()?;
        let mut got = 0;
        let err = ks
            .KsProperty(
                &KSIDENTIFIER::default(),
                size_of::<KSIDENTIFIER>() as u32,
                std::ptr::null_mut(),
                0,
                &raw mut got,
            )
            .err()
            .map(|e| e.code());
        assert_eq!(err, Some(ERROR_SET_NOT_FOUND.to_hresult()));
        let err = source
            .cast::<IMFGetService>()?
            .GetService::<IUnknown>(&GUID::zeroed())
            .err()
            .map(|e| e.code());
        assert_eq!(err, Some(MF_E_UNSUPPORTED_SERVICE));
        assert_eq!(
            source.Pause().err().map(|e| e.code()),
            Some(MF_E_INVALID_STATE_TRANSITION)
        );
        assert_eq!(source.GetCharacteristics()?, MFMEDIASOURCE_IS_LIVE.0 as u32);
        let attrs = source.GetStreamAttributes(0)?;
        assert_eq!(
            attrs.GetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY)?,
            PINNAME_VIDEO_CAPTURE
        );
        assert_eq!(attrs.GetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED)?, 1);
        assert!(
            source
                .GetSourceAttributes()?
                .GetUnknown::<IUnknown>(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION)
                .is_ok()
        );
    }
    Ok(())
}

#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn check_native_types(reader: &IMFSourceReader) -> windows_core::Result<()> {
    // SAFETY: plain COM calls with no raw pointers.
    unsafe {
        let types: Vec<_> = (0..)
            .map_while(|i| reader.GetNativeMediaType(STREAM, i).ok())
            .collect();
        assert_eq!(types.len(), 7);
        if let Some(first) = types.first() {
            assert_eq!(first.GetGUID(&MF_MT_SUBTYPE)?, MFVideoFormat_NV12);
            assert_eq!(first.GetUINT64(&MF_MT_FRAME_SIZE)?, frame_size(1280, 720));
            assert_eq!(
                first.GetUINT32(&MF_MT_YUV_MATRIX)?,
                MFVideoTransferMatrix_BT709.0 as u32
            );
            assert_eq!(
                first.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE)?,
                MFNominalRange_16_235.0 as u32
            );
            assert_eq!(
                first.GetUINT32(&MF_MT_SAMPLE_SIZE)?,
                (1280u32 * 720 * 3).div_euclid(2)
            );
        }
    }
    Ok(())
}

fn check_paced_frames(reader: &IMFSourceReader, nv12: Format) -> windows_core::Result<()> {
    let started = Instant::now();
    let frames = (0..30)
        .map(|_| read(reader))
        .collect::<windows_core::Result<Vec<_>>>()?;
    let elapsed = started.elapsed();
    let steps: Vec<i64> = frames
        .iter()
        .zip(frames.iter().skip(1))
        .map(|(a, b)| b.0 - a.0)
        .collect();
    let live = frames
        .iter()
        .filter(|(_, b)| !is_placeholder(b, nv12))
        .count();
    println!(
        "30 NV12 samples in {elapsed:?}, steps {:?}..{:?}, {live} live",
        steps.iter().min(),
        steps.iter().max()
    );
    assert!(
        elapsed > Duration::from_millis(900) && elapsed < Duration::from_millis(1300),
        "{elapsed:?}"
    );
    assert!(
        steps.iter().all(|&s| (330_000..=340_000).contains(&s)),
        "{steps:?}"
    );
    assert!(
        frames
            .iter()
            .all(|(_, b)| b.len() == nv12.bytes() && (is_pattern(b) || is_placeholder(b, nv12)))
    );
    assert!(live >= 28);
    Ok(())
}

fn check_quiet(reader: &IMFSourceReader, nv12: Format) -> windows_core::Result<()> {
    let silent = Instant::now();
    let mut repeats = Vec::new();
    let placeholder_after = loop {
        let (_, b) = read(reader)?;
        if is_placeholder(&b, nv12) {
            break silent.elapsed();
        }
        assert!(is_pattern(&b));
        repeats.push(b.first().copied());
        assert!(silent.elapsed() < Duration::from_secs(3), "no placeholder");
    };
    println!(
        "placeholder {placeholder_after:?} after the app went quiet, {} repeats",
        repeats.len()
    );
    assert!(
        repeats
            .iter()
            .zip(repeats.iter().skip(1))
            .all(|(a, b)| a == b)
    );
    assert!(
        placeholder_after > Duration::from_millis(1000)
            && placeholder_after < Duration::from_millis(2200)
    );
    Ok(())
}

#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn check_shut_down(source: &IMFMediaSourceEx) {
    // SAFETY: plain COM calls with no raw pointers.
    unsafe {
        assert_eq!(
            source.GetCharacteristics().err().map(|e| e.code()),
            Some(MF_E_SHUTDOWN)
        );
        assert_eq!(
            source.Shutdown().err().map(|e| e.code()),
            Some(MF_E_SHUTDOWN)
        );
        assert_eq!(
            source
                .CreatePresentationDescriptor()
                .err()
                .map(|e| e.code()),
            Some(MF_E_SHUTDOWN)
        );
    }
}

#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
fn open_yuy2(
    activate: &IMFActivate,
    yuy2: Format,
) -> windows_core::Result<(IMFMediaSource, IMFSourceReader)> {
    // SAFETY: a plain COM call on an activation object the DLL made.
    let source: IMFMediaSource = unsafe { activate.ActivateObject() }?;
    let reader = reader_for(&source, yuy2)?;
    let (_, first) = read(&reader)?;
    assert_eq!(first.len(), yuy2.bytes());
    Ok((source, reader))
}

#[test]
#[expect(unsafe_code, reason = "Media Foundation is a COM API")]
#[expect(
    clippy::disallowed_methods,
    reason = "a test thread, not the UI thread"
)]
fn serves_paced_frames_from_shared_memory() {
    // SAFETY: the only test here, so nothing else reads the environment yet. The DLL and
    // `Shared` read the name, so the installed app's buffer is never touched.
    unsafe { std::env::set_var("VIEWFINDER_SHARED", "Viewfinder-test") };
    // SAFETY: called once on the test thread before any other COM or Media Foundation call.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
        MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
    }
    let (factory, activate) = activation().unwrap();
    // SAFETY: a plain COM call on an activation object the DLL made.
    let source: IMFMediaSourceEx = unsafe { activate.ActivateObject() }.unwrap();
    check_frame_server_parts(&source).unwrap();

    let nv12 = Format {
        width: 1280,
        height: 720,
        fourcc: NV12,
    };
    let reader = reader_for(&source.cast().unwrap(), nv12).unwrap();
    check_native_types(&reader).unwrap();

    let (_, first) = read(&reader).unwrap();
    assert_eq!(first.len(), nv12.bytes());
    assert!(is_placeholder(&first, nv12));

    let app = Arc::new(Shared::open().unwrap());
    assert_eq!(app.wanted(1000), Some(nv12));
    let stop = Arc::new(AtomicBool::new(false));
    let producer = produce(app.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(50));
    check_paced_frames(&reader, nv12).unwrap();

    stop.store(true, Ordering::Relaxed);
    producer.join().unwrap();
    check_quiet(&reader, nv12).unwrap();

    drop(reader);
    check_shut_down(&source);
    assert_eq!(app.wanted(1000), None);
    drop(source);
    // The DACL stops a source running as the user reopening the mapping, as Frame Server's
    // LOCAL SERVICE can, so the next source makes a new one.
    drop(app);

    let yuy2 = Format {
        width: 640,
        height: 360,
        fourcc: YUY2,
    };
    let (source, reader) = open_yuy2(&activate, yuy2).unwrap();
    let app = Arc::new(Shared::open().unwrap());
    assert_eq!(app.wanted(1000), Some(yuy2));
    stop.store(false, Ordering::Relaxed);
    let producer = produce(app.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(50));
    let frames: Vec<_> = (0..10).map(|_| read(&reader).unwrap()).collect();
    assert!(
        frames
            .iter()
            .all(|(_, b)| b.len() == yuy2.bytes() && (is_pattern(b) || is_placeholder(b, yuy2)))
    );
    assert!(frames.iter().filter(|(_, b)| is_pattern(b)).count() >= 8);
    stop.store(true, Ordering::Relaxed);
    producer.join().unwrap();

    // SAFETY: plain COM calls, and MFShutdown pairs with the MFStartup above.
    unsafe {
        source.Shutdown().unwrap();
        assert_eq!(source.Shutdown().unwrap_err().code(), MF_E_SHUTDOWN);
        drop((reader, source, activate, factory));
        MFShutdown().unwrap();
    }
    assert_eq!(DllCanUnloadNow(), S_OK);
    drop(app);
}
