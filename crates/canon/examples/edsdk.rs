//! Times live view through Canon's EDSDK.dll the way Canon's sample does, to compare with the
//! probe. `EDSDK_DLL` overrides the DLL and `VIEWFINDER_SECS` the length.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use canon::jpeg_size;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

type Ref = *mut c_void;

#[link(name = "kernel32")]
#[expect(unsafe_code, reason = "EDSDK is a plain DLL loaded by hand")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

const EVF_OUTPUT_DEVICE: u32 = 0x500;
const EVF_MODE: u32 = 0x501;
const OUTPUT_PC: u32 = 2;
const NOT_READY: u32 = 0xA102;
const BUSY: u32 = 0x81;

struct Sdk {
    initialize: unsafe extern "system" fn() -> u32,
    terminate: unsafe extern "system" fn() -> u32,
    camera_list: unsafe extern "system" fn(*mut Ref) -> u32,
    child_count: unsafe extern "system" fn(Ref, *mut u32) -> u32,
    child_at: unsafe extern "system" fn(Ref, i32, *mut Ref) -> u32,
    device_info: unsafe extern "system" fn(Ref, *mut [u8; 520]) -> u32,
    open: unsafe extern "system" fn(Ref) -> u32,
    close: unsafe extern "system" fn(Ref) -> u32,
    get: unsafe extern "system" fn(Ref, u32, i32, u32, *mut c_void) -> u32,
    set: unsafe extern "system" fn(Ref, u32, i32, u32, *const c_void) -> u32,
    memory_stream: unsafe extern "system" fn(u64, *mut Ref) -> u32,
    evf_ref: unsafe extern "system" fn(Ref, *mut Ref) -> u32,
    download_evf: unsafe extern "system" fn(Ref, Ref) -> u32,
    pointer: unsafe extern "system" fn(Ref, *mut *const u8) -> u32,
    length: unsafe extern "system" fn(Ref, *mut u64) -> u32,
    release: unsafe extern "system" fn(Ref) -> u32,
    event: unsafe extern "system" fn() -> u32,
}

#[expect(unsafe_code, reason = "EDSDK is a plain DLL loaded by hand")]
fn load() -> Result<Sdk, String> {
    let path = std::env::var("EDSDK_DLL")
        .unwrap_or_else(|_| r"C:\Program Files\Canon\EOS Webcam Utility Pro\EDSDK.dll".into());
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(format!("couldn't load {path}"));
    }
    macro_rules! f {
        ($name:literal) => {{
            // SAFETY: `module` is a loaded library and the name is a NUL-terminated literal.
            let p = unsafe { GetProcAddress(module, concat!($name, "\0").as_ptr()) };
            if p.is_null() {
                return Err(format!("{path} has no {}", $name));
            }
            #[expect(
                clippy::missing_transmute_annotations,
                reason = "each field of Sdk gives the target type"
            )]
            // SAFETY: `p` is a non-null export of EDSDK, and the field it fills declares the
            // signature from EDSDK.h.
            unsafe {
                std::mem::transmute(p)
            }
        }};
    }
    Ok(Sdk {
        initialize: f!("EdsInitializeSDK"),
        terminate: f!("EdsTerminateSDK"),
        camera_list: f!("EdsGetCameraList"),
        child_count: f!("EdsGetChildCount"),
        child_at: f!("EdsGetChildAtIndex"),
        device_info: f!("EdsGetDeviceInfo"),
        open: f!("EdsOpenSession"),
        close: f!("EdsCloseSession"),
        get: f!("EdsGetPropertyData"),
        set: f!("EdsSetPropertyData"),
        memory_stream: f!("EdsCreateMemoryStream"),
        evf_ref: f!("EdsCreateEvfImageRef"),
        download_evf: f!("EdsDownloadEvfImage"),
        pointer: f!("EdsGetPointer"),
        length: f!("EdsGetLength"),
        release: f!("EdsRelease"),
        event: f!("EdsGetEvent"),
    })
}

fn check(what: &str, code: u32) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed with {code:#x}"))
    }
}

#[expect(unsafe_code, reason = "EDSDK is a plain DLL loaded by hand")]
fn main() -> Result<(), String> {
    let sdk = load()?;
    // SAFETY: COM is set up once on this thread before EDSDK is used.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().ok() };
    // SAFETY: EdsInitializeSDK takes no arguments.
    check("EdsInitializeSDK", unsafe { (sdk.initialize)() })?;
    let result = run(&sdk);
    // SAFETY: matches the EdsInitializeSDK above, after every EDSDK object is released.
    unsafe { (sdk.terminate)() };
    result
}

struct Stats {
    frames: u32,
    not_ready: u32,
    busy: u32,
    repeats: u32,
    bytes: usize,
    fastest: Duration,
    slowest: Duration,
    total: Duration,
    sizes: BTreeMap<Option<(u16, u16)>, u32>,
    failure: Option<String>,
    first_at: Option<Duration>,
    last_at: Duration,
    secs: f64,
}

fn run(sdk: &Sdk) -> Result<(), String> {
    let camera = first_camera(sdk)?;
    let (before, mode_before) = start_live_view(sdk, camera)?;
    let stats = pull(sdk, camera);
    report(&stats);
    restore(sdk, camera, before, mode_before);
    println!("closed");
    stats.failure.map_or(Ok(()), Err)
}

#[expect(unsafe_code, reason = "EDSDK is a plain DLL loaded by hand")]
fn first_camera(sdk: &Sdk) -> Result<Ref, String> {
    let mut list: Ref = std::ptr::null_mut();
    // SAFETY: the out pointer points at a live local.
    check("EdsGetCameraList", unsafe {
        (sdk.camera_list)(&raw mut list)
    })?;
    let mut count = 0;
    // SAFETY: `list` came from EdsGetCameraList and the out pointer points at a live local.
    unsafe { (sdk.child_count)(list, &raw mut count) };
    if count == 0 {
        // SAFETY: `list` came from EdsGetCameraList and is released once.
        unsafe { (sdk.release)(list) };
        return Err("EDSDK sees no camera".into());
    }
    let mut camera: Ref = std::ptr::null_mut();
    // SAFETY: index 0 is within the `count` children and the out pointer points at a live local.
    check("EdsGetChildAtIndex", unsafe {
        (sdk.child_at)(list, 0, &raw mut camera)
    })?;
    // SAFETY: `list` came from EdsGetCameraList and is released once.
    unsafe { (sdk.release)(list) };

    // EdsDeviceInfo is two 256-byte strings then two u32s.
    let mut info = [0u8; 520];
    // SAFETY: `info` has the 520 bytes of an EdsDeviceInfo.
    unsafe { (sdk.device_info)(camera, &raw mut info) };
    let name = String::from_utf8_lossy(&info[256..512]);
    println!("EDSDK found {}", name.trim_end_matches('\0'));
    Ok(camera)
}

#[expect(unsafe_code, reason = "EDSDK is a plain DLL loaded by hand")]
#[expect(
    clippy::disallowed_methods,
    reason = "a command-line example, not the UI thread"
)]
fn start_live_view(sdk: &Sdk, camera: Ref) -> Result<(u32, u32), String> {
    let started = Instant::now();
    // SAFETY: `camera` came from EdsGetChildAtIndex.
    check("EdsOpenSession", unsafe { (sdk.open)(camera) })?;
    println!("opened in {:?}", started.elapsed());
    let mut before = 0u32;
    // SAFETY: the property is a u32 and the pointer is to a live 4-byte local.
    unsafe { (sdk.get)(camera, EVF_OUTPUT_DEVICE, 0, 4, (&raw mut before).cast()) };
    let started = Instant::now();
    let pc = OUTPUT_PC;
    let (mut mode_before, on) = (0u32, 1u32);
    // SAFETY: the property is a u32 and both pointers are to live 4-byte locals.
    unsafe {
        (sdk.get)(camera, EVF_MODE, 0, 4, (&raw mut mode_before).cast());
        (sdk.set)(camera, EVF_MODE, 0, 4, (&raw const on).cast());
    }
    let mut code = BUSY;
    for _ in 0..40 {
        // SAFETY: the property is a u32 and the pointer is to a live 4-byte local.
        code = unsafe { (sdk.set)(camera, EVF_OUTPUT_DEVICE, 0, 4, (&raw const pc).cast()) };
        if code != BUSY {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check("setting EVF output", code)?;
    println!("live view started in {:?}", started.elapsed());
    Ok((before, mode_before))
}

#[expect(unsafe_code, reason = "EDSDK is a plain DLL loaded by hand")]
fn pull(sdk: &Sdk, camera: Ref) -> Stats {
    let mut s = Stats {
        frames: 0,
        not_ready: 0,
        busy: 0,
        repeats: 0,
        bytes: 0,
        fastest: Duration::MAX,
        slowest: Duration::ZERO,
        total: Duration::ZERO,
        sizes: BTreeMap::new(),
        failure: None,
        first_at: None,
        last_at: Duration::ZERO,
        secs: 0.0,
    };
    let mut previous = Vec::new();
    let secs_wanted = std::env::var("VIEWFINDER_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let run = Instant::now();
    while run.elapsed() < Duration::from_secs(secs_wanted) {
        // SAFETY: EdsGetEvent takes no arguments.
        unsafe { (sdk.event)() };
        let call = Instant::now();
        let (mut stream, mut evf): (Ref, Ref) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: both out pointers point at live locals.
        unsafe {
            (sdk.memory_stream)(0, &raw mut stream);
            (sdk.evf_ref)(stream, &raw mut evf);
        }
        // SAFETY: `camera` has an open session and `evf` came from EdsCreateEvfImageRef.
        let code = unsafe { (sdk.download_evf)(camera, evf) };
        if code == 0 {
            let (mut ptr, mut len) = (std::ptr::null(), 0u64);
            // SAFETY: `stream` came from EdsCreateMemoryStream and both out pointers point at
            // live locals.
            unsafe {
                (sdk.pointer)(stream, &raw mut ptr);
                (sdk.length)(stream, &raw mut len);
            }
            // SAFETY: the stream holds `len` bytes at `ptr` until it is released below, and
            // `jpeg` is not used after that.
            let jpeg = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            s.frames += 1;
            s.last_at = run.elapsed();
            s.first_at.get_or_insert(s.last_at);
            s.bytes += jpeg.len();
            *s.sizes.entry(jpeg_size(jpeg)).or_insert(0u32) += 1;
            if jpeg == previous.as_slice() {
                s.repeats += 1;
            } else {
                previous = jpeg.to_vec();
            }
        }
        // SAFETY: both refs came from EDSDK this pass and are released once.
        unsafe {
            (sdk.release)(evf);
            (sdk.release)(stream);
        }
        let took = call.elapsed();
        s.fastest = s.fastest.min(took);
        s.slowest = s.slowest.max(took);
        s.total += took;
        match code {
            0 => {}
            NOT_READY => s.not_ready += 1,
            BUSY => s.busy += 1,
            other => {
                s.failure = Some(format!("EdsDownloadEvfImage failed with {other:#x}"));
                break;
            }
        }
    }
    s.secs = run.elapsed().as_secs_f64();
    s
}

fn report(s: &Stats) {
    let (frames, secs) = (s.frames, s.secs);
    let calls = frames + s.not_ready + s.busy;

    println!(
        "{calls} calls in {secs:.1} s: {frames} frames, {} not ready, {} busy",
        s.not_ready, s.busy
    );
    println!(
        "{:.1} frames/s, {:.1} new frames/s ({} repeats)",
        f64::from(frames) / secs,
        f64::from(frames - s.repeats) / secs,
        s.repeats
    );
    if let Some(first) = s.first_at.filter(|_| frames > 1) {
        let span = s.last_at.saturating_sub(first).as_secs_f64();
        println!(
            "steady: {:.1} frames/s after the first frame at {:.2} s",
            f64::from(frames - 1) / span,
            first.as_secs_f64()
        );
    }
    if calls > 0 {
        println!(
            "per call: min {:.1} ms, avg {:.1} ms, max {:.1} ms",
            s.fastest.as_secs_f64() * 1e3,
            s.total.as_secs_f64() * 1e3 / f64::from(calls),
            s.slowest.as_secs_f64() * 1e3,
        );
    }
    if frames > 0 {
        println!("JPEG bytes: avg {}", s.bytes.div_euclid(frames as usize));
        for (dims, n) in &s.sizes {
            if let Some((w, h)) = dims {
                println!("  {w}x{h}: {n} frames");
            }
        }
    }
}

#[expect(unsafe_code, reason = "EDSDK is a plain DLL loaded by hand")]
fn restore(sdk: &Sdk, camera: Ref, before: u32, mode_before: u32) {
    // SAFETY: the properties are u32s, both pointers are to live 4-byte locals, and `camera`
    // is closed and released once.
    unsafe {
        (sdk.set)(camera, EVF_OUTPUT_DEVICE, 0, 4, (&raw const before).cast());
        (sdk.set)(camera, EVF_MODE, 0, 4, (&raw const mode_before).cast());
        (sdk.close)(camera);
        (sdk.release)(camera);
    }
}
