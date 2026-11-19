//! A headless stand-in for a call, so the pipeline's cost can be measured without the camera.
//! Synthetic live view JPEGs arrive as unevenly as the T3's, and the loop below paces them the
//! way `pipeline_loop` does, rendering into a plain buffer in place of the shared slot.
//!
//! `VF_BENCH` picks `call`, `call+preview`, `preview`, `call+idle`, `stages` or `golden`, and
//! `VF_SECS` the length. `call+idle` goes on to time letting go of the GPU and building it again.
//! `stages` times each step at a call's rhythm with no clock. `golden` renders fixed moments and
//! writes them to `VF_OUT`, so two builds can be compared byte for byte.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use shared::{Format, NV12};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use super::*;
use crate::pipeline::preview_output;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

#[expect(
    unsafe_code,
    reason = "a global allocator has to implement an unsafe trait"
)]
// SAFETY: every method passes the request straight to the system allocator, so its guarantees
// carry over. The counters are atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller keeps the contract of `alloc`, which is the one System needs.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, so from System, with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new as u64, Ordering::Relaxed);
        // SAFETY: `ptr` came from System with this layout, and the caller keeps the contract
        // of `realloc` for `new`.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const W: usize = 1056;
const H: usize = 704;

/// A panning scene of edges, fine texture and soft shapes, with fresh grain on every frame like
/// a warm sensor at high ISO.
#[expect(
    clippy::suboptimal_flops,
    clippy::imprecise_flops,
    reason = "mul_add and hypot change rounding, and the scene must stay the same"
)]
#[expect(
    clippy::modulo_arithmetic,
    reason = "the pan goes negative, and the gradient keeps the sign of a truncating remainder"
)]
fn scene(t: usize) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(W * H * 3);
    let pan = (t as f32 * 2.7).sin() * 40.0 + t as f32 * 1.5;
    for y in 0..H {
        for x in 0..W {
            let fx = x as f32 + pan;
            let fy = y as f32 + (t as f32 * 0.21).sin() * 12.0;
            let mut colour = [70.0 + fy * 0.15, 80.0 + fx * 0.05 % 60.0, 110.0 - fy * 0.05];
            let hash =
                (fx as u32).wrapping_mul(0x9E37_79B1) ^ (fy as u32).wrapping_mul(0x85EB_CA77);
            let weave = ((hash ^ (hash >> 15)).wrapping_mul(0x2C1B_3C6D) >> 26) as f32;
            if (300.0..700.0).contains(&fx.rem_euclid(1400.0)) && (200.0..520.0).contains(&fy) {
                colour = [140.0 + weave, 110.0 + weave, 90.0 + weave];
            }
            for (cx, cy, radius, disc) in [
                (500.0, 300.0, 110.0, [215.0, 170.0, 140.0]),
                (850.0, 420.0, 70.0, [40.0, 150.0, 210.0]),
            ] {
                let dist = ((x as f32 - cx - (t as f32 * 0.13).sin() * 60.0).powi(2)
                    + (fy - cy).powi(2))
                .sqrt();
                let cover = (radius - dist + 0.5).clamp(0.0, 1.0);
                for (ch, target) in colour.iter_mut().zip(disc) {
                    *ch += (target - *ch) * cover;
                }
            }
            if (fx / 11.0).floor() as i32 % 2 == 0 && (600.0..660.0).contains(&fy) {
                colour = [235.0, 235.0, 230.0];
            }
            let grain = (x as u32 * 7919 + y as u32 * 104_729 + t as u32 * 15_485_863)
                .wrapping_mul(0x9E37_79B1)
                >> 27;
            rgb.extend(colour.map(|v| (v + grain as f32 - 16.0).clamp(0.0, 255.0) as u8));
        }
    }
    rgb
}

fn jpegs(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|t| {
            let mut jpeg = Vec::new();
            jpeg_encoder::Encoder::new(&mut jpeg, 85)
                .encode(&scene(t), W as u16, H as u16, jpeg_encoder::ColorType::Rgb)
                .unwrap();
            jpeg
        })
        .collect()
}

/// Gaps between camera frames, 20 to 52 ms around 35.7 ms, the T3's 28 fps.
fn gap(i: usize) -> Duration {
    Duration::from_micros(20_000 + (i as u64 * 7919 % 32) * 1000)
}

#[derive(Clone, Copy)]
struct Usage {
    cpu: Duration,
    working: usize,
    peak: usize,
    faults: u32,
}

#[expect(unsafe_code, reason = "the process counters come from Win32")]
fn usage() -> Usage {
    // SAFETY: GetCurrentProcess has no preconditions and returns a pseudo handle.
    let process = unsafe { GetCurrentProcess() };
    let mut m = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `m` is a live PROCESS_MEMORY_COUNTERS and `cb` holds its size.
    unsafe { GetProcessMemoryInfo(process, &raw mut m, m.cb).unwrap() };
    let (mut a, mut b, mut kernel, mut user) = Default::default();
    // SAFETY: all four out pointers point at live FILETIME locals.
    unsafe {
        GetProcessTimes(
            process,
            &raw mut a,
            &raw mut b,
            &raw mut kernel,
            &raw mut user,
        )
        .unwrap();
    };
    let ticks = |f: windows::Win32::Foundation::FILETIME| {
        u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)
    };
    Usage {
        cpu: Duration::from_nanos((ticks(kernel) + ticks(user)) * 100),
        working: m.WorkingSetSize,
        peak: m.PeakWorkingSetSize,
        faults: m.PageFaultCount,
    }
}

/// Renders for an app and the window the way `Output::emit` does, with `slot` for the shared slot.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and this matches Output::emit"
)]
fn emit(
    out: &mut Output,
    hub: &Hub,
    pipeline: &mut Pipeline,
    wanted: Option<Format>,
    slot: &mut [u8],
    started: Instant,
) {
    let preview = hub.preview_wanted.load(Ordering::Relaxed);
    let mut previewed = None;
    if let Some(f) = wanted {
        out.frame_no += 1;
        let frame = slot.get_mut(..f.bytes()).unwrap();
        pipeline.render(f, frame).unwrap();
        if preview {
            previewed = Some(preview_output(f, frame, &mut out.rgba));
        }
    }
    if preview && previewed.is_none() {
        out.local.resize(PREVIEW.bytes(), 0);
        pipeline.render(PREVIEW, &mut out.local).unwrap();
        previewed = Some(preview_output(PREVIEW, &out.local, &mut out.rgba));
    }
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    out.frame_ms = if out.frame_ms == 0.0 {
        ms
    } else {
        out.frame_ms * 0.9 + ms * 0.1
    };
    if let Some((w, h)) = previewed {
        let histogram = histogram(&out.rgba);
        let mut p = lock(&hub.preview);
        std::mem::swap(&mut p.rgba, &mut out.rgba);
        (p.width, p.height, p.fresh, p.histogram) = (w, h, true, histogram);
    }
}

#[derive(Default)]
struct Stats {
    first: Option<Duration>,
    emitted: u32,
    distinct: u32,
    last_sum: u64,
    last_emit: Option<Instant>,
    worst_gap: Duration,
}

impl Stats {
    fn tick(&mut self, begun: Instant, sum: u64) {
        let now = Instant::now();
        self.first.get_or_insert_with(|| begun.elapsed());
        if let Some(t) = self
            .last_emit
            .filter(|_| begun.elapsed() > Duration::from_secs(3))
        {
            self.worst_gap = self.worst_gap.max(now - t);
        }
        self.last_emit = Some(now);
        self.emitted += 1;
        self.distinct += u32::from(sum != self.last_sum);
        self.last_sum = sum;
    }
}

fn checksum(frame: &[u8]) -> u64 {
    frame
        .iter()
        .step_by(61)
        .fold(0u64, |h, &b| h.wrapping_mul(0x100_0000_01B3) ^ u64::from(b))
}

/// Usage once the run is warm: when, the counters, allocations, allocated bytes and frames out.
type Warm = (Instant, Usage, u64, u64, u32);

/// Usage taken once the run is warm and then every second, and the trim a GPU build schedules.
#[derive(Default)]
struct Watch {
    warm: Option<Warm>,
    peak_ws: usize,
    samples: Vec<usize>,
    builds: u32,
    trim_at: Option<Instant>,
}

impl Watch {
    fn check(&mut self, begun: Instant, emitted: u32, pipeline: &Pipeline) {
        if self.warm.is_none() && begun.elapsed() > Duration::from_secs(5) {
            self.warm = Some((
                Instant::now(),
                usage(),
                ALLOCS.load(Ordering::Relaxed),
                ALLOC_BYTES.load(Ordering::Relaxed),
                emitted,
            ));
        }
        if self.warm.is_some() && self.samples.len() < (begun.elapsed().as_secs() as usize) {
            let u = usage();
            self.samples.push(u.working);
            self.peak_ws = self.peak_ws.max(u.working);
        }
        if pipeline.builds() != self.builds {
            self.builds = pipeline.builds();
            self.trim_at = Some(Instant::now() + TRIM_AFTER_BUILD);
        }
        if self.trim_at.is_some_and(|t| Instant::now() >= t) {
            self.trim_at = None;
            trim();
        }
    }
}

/// What a timed run leaves for the report.
struct Run {
    begun: Instant,
    stats: Stats,
    watch: Watch,
    frame_ms: f32,
}

#[test]
#[ignore = "needs NVIDIA's DLLs in VIEWFINDER_VFX and a quiet GPU"]
fn harness() {
    let vfx = std::env::var_os("VIEWFINDER_VFX").map(std::path::PathBuf::from);
    let mode = std::env::var("VF_BENCH").unwrap_or_else(|_| "call".into());
    let secs: u64 = std::env::var("VF_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let call = Format {
        width: 1920,
        height: 1080,
        fourcc: NV12,
    };
    let frames = jpegs(56);
    let look = Look::default();
    let mut pipeline = Pipeline::with_gpu(vfx);
    let mut slot = vec![0u8; shared::SLOT_BYTES];

    if mode == "stages" {
        stages(&mut pipeline, &frames, call, look, &mut slot);
        return;
    }
    if mode == "golden" {
        golden(&mut pipeline, &frames, call, look, &mut slot);
        return;
    }

    let hub = Hub::new(look);
    let wanted = mode.starts_with("call").then_some(call);
    hub.preview_wanted
        .store(mode.contains("preview"), Ordering::Relaxed);
    let feeder = feed(&hub, &frames, secs);
    let run = live(&hub, &mut pipeline, wanted, look, &mut slot, &feeder);
    report(&mode, &pipeline, &run);
    if !mode.ends_with("idle") {
        return;
    }
    idle_then_rebuild(&mut pipeline, &frames, call, look, &mut slot, run.begun);
}

/// The real call's rhythm without the clock: about 0.93 camera frames per output frame.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and the frame times must stay the same"
)]
fn stages(pipeline: &mut Pipeline, frames: &[Vec<u8>], call: Format, look: Look, slot: &mut [u8]) {
    let (mut decode, mut show, mut render) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut seq, mut decoded, n) = (0usize, 0u32, 50u32);
    let mut u0 = None;
    let mb = |u: Usage| u.working as f64 / 1e6;
    println!("working set before the GPU {:.0} MB", mb(usage()));
    for i in 0..n + 10 {
        if i == 1 {
            println!("after the first frame {:.0} MB", mb(usage()));
        }
        if i == 10 {
            (decode, show, render, decoded) = (Duration::ZERO, Duration::ZERO, Duration::ZERO, 0);
            u0 = Some(usage());
        }
        let at = 3.0 + f64::from(i) * 0.93;
        // The last whole frame at or before `at + 2.2`.
        let last = (at + 2.2).floor() as usize;
        while seq <= last {
            let t = Instant::now();
            pipeline
                .decode(frames.get(seq % frames.len()).unwrap(), look)
                .unwrap();
            decode += t.elapsed();
            pipeline.hold(seq as u64);
            seq += 1;
            decoded += 1;
        }
        let t = Instant::now();
        pipeline.show_at(at);
        show += t.elapsed();
        let t = Instant::now();
        pipeline
            .render(call, slot.get_mut(..call.bytes()).unwrap())
            .unwrap();
        render += t.elapsed();
    }
    println!("after {} frames {:.0} MB", n + 10, mb(usage()));
    let cpu = usage().cpu.saturating_sub(u0.expect("warmed up").cpu);
    let ms = |d: Duration, n: u32| d.as_secs_f64() * 1e3 / f64::from(n);
    println!(
        "decode {:.2} ms, show_at {:.2} ms, render {:.2} ms, CPU {:.2} ms per output frame",
        ms(decode, decoded),
        ms(show, n),
        ms(render, n),
        ms(cpu, n)
    );
    assert!(pipeline.gpu_problem().is_none() && pipeline.smooth_problem().is_none());
}

/// Fixed moments rendered at three sizes and written to `VF_OUT`.
fn golden(pipeline: &mut Pipeline, frames: &[Vec<u8>], call: Format, look: Look, slot: &mut [u8]) {
    let dir = std::path::PathBuf::from(std::env::var_os("VF_OUT").expect("VF_OUT"));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(pipeline.can_smooth(look));
    for (seq, jpeg) in frames.iter().enumerate().take(12) {
        pipeline.decode(jpeg, look).unwrap();
        pipeline.hold(seq as u64);
    }
    for (i, at) in [4.0, 4.25, 4.5, 5.1, 5.6, 6.0, 6.93, 7.4, 8.75, 9.0]
        .into_iter()
        .enumerate()
    {
        pipeline.show_at(at);
        for f in [
            call,
            Format {
                width: 1280,
                height: 720,
                fourcc: shared::YUY2,
            },
            Format {
                width: 640,
                height: 360,
                fourcc: NV12,
            },
        ] {
            let frame = slot.get_mut(..f.bytes()).unwrap();
            pipeline.render(f, frame).unwrap();
            std::fs::write(
                dir.join(format!("{i}-{}x{}.raw", f.width, f.height)),
                &*frame,
            )
            .unwrap();
        }
    }
    assert!(pipeline.gpu_problem().is_none() && pipeline.smooth_problem().is_none());
}

/// Posts the JPEGs to the mailbox as unevenly as the T3 does, for `secs` seconds.
#[expect(
    clippy::disallowed_methods,
    reason = "a bench thread standing in for the camera thread"
)]
fn feed(hub: &Arc<Hub>, frames: &[Vec<u8>], secs: u64) -> thread::JoinHandle<()> {
    let hub = hub.clone();
    let frames = frames.to_vec();
    thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut i = 0;
        while Instant::now() < end {
            thread::sleep(gap(i));
            let mut mailbox = lock(&hub.mailbox);
            mailbox.0.clone_from(frames.get(i % frames.len()).unwrap());
            mailbox.1 = true;
            hub.mail.notify_one();
            drop(mailbox);
            i += 1;
        }
    })
}

/// Waits up to `wait` for the next JPEG and swaps it into `jpeg`. Returns whether one came.
fn take_mail(hub: &Hub, wait: Duration, jpeg: &mut Vec<u8>) -> bool {
    let (mut mailbox, _) = hub
        .mail
        .wait_timeout_while(lock(&hub.mailbox), wait, |m| !m.1)
        .unwrap();
    let got = mailbox.1;
    if got {
        std::mem::swap(&mut mailbox.0, jpeg);
        mailbox.1 = false;
    }
    drop(mailbox);
    got
}

/// The loop of `pipeline_loop`, run until the feeder stops.
#[expect(
    clippy::disallowed_methods,
    reason = "a bench thread standing in for the pipeline thread"
)]
fn live(
    hub: &Hub,
    pipeline: &mut Pipeline,
    wanted: Option<Format>,
    look: Look,
    slot: &mut [u8],
    feeder: &thread::JoinHandle<()>,
) -> Run {
    let begun = Instant::now();
    let mut jpeg = Vec::new();
    let mut out = Output::default();
    let mut pacer = Pacer::default();
    let mut seq = 0u64;
    let pacing = wanted.is_some() && pipeline.can_smooth(look);
    let mut stats = Stats::default();
    let mut watch = Watch::default();
    while !feeder.is_finished() {
        watch.check(begun, stats.emitted, pipeline);
        let until = if pacing {
            pacer.until_tick(None, out.frame_ms)
        } else {
            Duration::from_millis(200)
        };
        let close = pacing && until <= COARSE_WAKE;
        let wait = if close {
            Duration::ZERO
        } else if pacing {
            until.saturating_sub(COARSE_WAKE)
        } else {
            until
        };
        if take_mail(hub, wait, &mut jpeg) {
            let started = Instant::now();
            pipeline.decode(&jpeg, look).unwrap();
            if pacing {
                pipeline.hold(seq);
                pacer.arrived(seq, started);
                seq += 1;
            } else {
                emit(&mut out, hub, pipeline, wanted, slot, started);
                stats.tick(
                    begun,
                    checksum(if wanted.is_some() { &*slot } else { &out.rgba }),
                );
            }
        }
        if close {
            thread::sleep(pacer.remaining());
            let started = Instant::now();
            if let Some(at) = pacer.advance() {
                pipeline.show_at(at);
                emit(&mut out, hub, pipeline, wanted, slot, started);
                pacer.served();
                stats.tick(
                    begun,
                    checksum(if wanted.is_some() { &*slot } else { &out.rgba }),
                );
            }
        }
    }
    Run {
        begun,
        stats,
        watch,
        frame_ms: out.frame_ms,
    }
}

fn report(mode: &str, pipeline: &Pipeline, run: &Run) {
    let (t0, u0, a0, b0, e0) = run.watch.warm.expect("ran past warm-up");
    let u1 = usage();
    let wall = t0.elapsed().as_secs_f64();
    let n = f64::from(run.stats.emitted - e0);
    let samples = &run.watch.samples;
    let steady = samples.iter().sum::<usize>() as f64 / samples.len().max(1) as f64;
    assert!(
        pipeline.gpu_problem().is_none(),
        "{:?}",
        pipeline.gpu_problem()
    );
    assert!(
        pipeline.smooth_problem().is_none(),
        "{:?}",
        pipeline.smooth_problem()
    );
    println!("pid {}", std::process::id());
    println!(
        "mode {mode}, first frame {:.0} ms, worst gap after 3 s {:.1} ms",
        run.stats.first.unwrap_or_default().as_secs_f64() * 1e3,
        run.stats.worst_gap.as_secs_f64() * 1e3
    );
    println!(
        "emitted {:.1}/s, distinct {:.1}/s",
        n / wall,
        f64::from(run.stats.distinct) / run.begun.elapsed().as_secs_f64()
    );
    println!(
        "CPU {:.2} ms per frame, {:.1}% of a core",
        u1.cpu.saturating_sub(u0.cpu).as_secs_f64() * 1e3 / n,
        u1.cpu.saturating_sub(u0.cpu).as_secs_f64() / wall * 100.0
    );
    println!("render {:.2} ms average", run.frame_ms);
    println!(
        "working set steady {:.0} MB, peak in run {:.0} MB, process peak {:.0} MB",
        steady / 1e6,
        run.watch.peak_ws as f64 / 1e6,
        u1.peak as f64 / 1e6
    );
    println!(
        "page faults {:.0}/s, {:.0} per frame",
        f64::from(u1.faults - u0.faults) / wall,
        f64::from(u1.faults - u0.faults) / n
    );
    let (a1, b1) = (
        ALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    );
    println!(
        "allocations {:.1} per frame, {:.0} KB per frame",
        (a1 - a0) as f64 / n,
        (b1 - b0) as f64 / n / 1e3
    );
}

/// Sits idle, lets go of the GPU and builds it again unless `VF_KEEP` is set, then times the
/// first generated frame.
#[expect(
    clippy::disallowed_methods,
    reason = "a bench thread, not the UI thread"
)]
fn idle_then_rebuild(
    pipeline: &mut Pipeline,
    frames: &[Vec<u8>],
    call: Format,
    look: Look,
    slot: &mut [u8],
    begun: Instant,
) {
    let mb = |u: Usage| u.working as f64 / 1e6;
    let idle = |what: &str| {
        let u = usage();
        thread::sleep(Duration::from_secs(4));
        let v = usage();
        println!(
            "{:.1} s {what}: working set {:.0} MB, CPU {:.2}%",
            begun.elapsed().as_secs_f64(),
            mb(v),
            v.cpu.saturating_sub(u.cpu).as_secs_f64() / 4.0 * 100.0
        );
    };
    idle("idle, passes built");
    let keep = std::env::var("VF_KEEP").is_ok();
    if keep {
        idle("still idle");
        idle("still idle");
    }
    if !keep {
        let t = Instant::now();
        pipeline.release();
        println!("release took {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
        idle("released");
        trim();
        idle("trimmed");
        let t = Instant::now();
        pipeline.prepare(call, look);
        println!(
            "{:.1} s prepare took {:.0} ms",
            begun.elapsed().as_secs_f64(),
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    for (i, jpeg) in frames.iter().enumerate().take(4) {
        pipeline.decode(jpeg, look).unwrap();
        pipeline.hold(1000 + i as u64);
    }
    let t = Instant::now();
    pipeline.show_at(1001.5);
    let show = t.elapsed();
    pipeline
        .render(call, slot.get_mut(..call.bytes()).unwrap())
        .unwrap();
    println!(
        "then the first generated frame took {:.0} ms, {:.0} of it drawing the frame",
        t.elapsed().as_secs_f64() * 1e3,
        show.as_secs_f64() * 1e3
    );
    assert!(pipeline.gpu_problem().is_none() && pipeline.smooth_problem().is_none());
}

/// Eight scenes scaled to 1280x720 RGBA, the preview's size.
fn preview_frames() -> Vec<Vec<u8>> {
    (0..8)
        .map(|t| {
            let rgb = scene(t);
            let mut rgba = Vec::with_capacity(1280 * 720 * 4);
            for y in 0..720 {
                for x in 0..1280 {
                    let at = ((y * H).div_euclid(720) * W + (x * W).div_euclid(1280)) * 3;
                    rgba.extend_from_slice(rgb.get(at..at + 3).unwrap());
                    rgba.push(255);
                }
            }
            rgba
        })
        .collect()
}

/// The window's side of the preview: the real `ui::App` in a glow window parked off screen, fed a
/// moving 1280x720 picture at the camera's rate. Set `VF_SECS` for the length.
#[test]
#[ignore = "opens a window off screen and uses the GPU"]
#[expect(
    clippy::disallowed_methods,
    reason = "bench threads feeding and closing the window, not the UI thread"
)]
fn window() {
    use eframe::egui;
    use winit::platform::windows::EventLoopBuilderExtWindows;

    let secs: u64 = std::env::var("VF_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let hub = Hub::new(Look::default());
    let quitting = Arc::new(AtomicBool::new(false));
    let (tx, _rx) = std::sync::mpsc::channel();
    let frames = preview_frames();
    let feed_hub = hub.clone();
    let done = quitting.clone();
    let measured = Arc::new(Mutex::new(None));
    let result = measured.clone();
    let feeder = thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        let (t0, u0, a0) = (Instant::now(), usage(), ALLOCS.load(Ordering::Relaxed));
        let mut i = 0;
        while t0.elapsed() < Duration::from_secs(secs) {
            thread::sleep(gap(i));
            let mut preview = lock(&feed_hub.preview);
            preview
                .rgba
                .clone_from(frames.get(i % frames.len()).unwrap());
            (
                preview.width,
                preview.height,
                preview.fresh,
                preview.histogram,
            ) = (1280, 720, true, vec![0; 64]);
            drop(preview);
            feed_hub.repaint();
            i += 1;
        }
        let u1 = usage();
        let wall = t0.elapsed().as_secs_f64();
        *lock(&result) = Some(format!(
            "window: {:.1} frames/s, CPU {:.1}% of a core, page faults {:.0}/s, allocations {:.0}/s, working set {:.0} MB",
            i as f64 / wall,
            u1.cpu.saturating_sub(u0.cpu).as_secs_f64() / wall * 100.0,
            f64::from(u1.faults - u0.faults) / wall,
            (ALLOCS.load(Ordering::Relaxed) - a0) as f64 / wall,
            u1.working as f64 / 1e6
        ));
        done.store(true, Ordering::Relaxed);
        feed_hub.repaint();
    });
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 640.0])
            .with_position([-4000.0, -4000.0])
            .with_taskbar(false),
        event_loop_builder: Some(Box::new(|b| {
            b.with_any_thread(true);
        })),
        ..Default::default()
    };
    let closing = quitting.clone();
    eframe::run_native(
        "Viewfinder bench",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            thread::spawn(move || {
                while !closing.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            });
            Ok(Box::new(crate::ui::App::new(cc, hub, tx, quitting, true)))
        }),
    )
    .unwrap();
    feeder.join().unwrap();
    println!("pid {}", std::process::id());
    println!("{}", lock(&measured).take().expect("measured"));
}

#[test]
#[ignore = "timing only"]
fn decode_only() {
    let frames = jpegs(20);
    for (name, fix, gpu) in [
        ("rgb + repair", true, true),
        ("rgb", false, true),
        ("ycc + repair", true, false),
    ] {
        let mut p = Pipeline::with_gpu(gpu.then(|| "nowhere".into()));
        let look = Look {
            fix_pixels: fix,
            ..Look::default()
        };
        for f in &frames {
            p.decode(f, look).unwrap();
        }
        let t = Instant::now();
        for _ in 0..5 {
            for f in &frames {
                p.decode(f, look).unwrap();
            }
        }
        println!(
            "{name}: {:.2} ms, jpeg {} KB",
            t.elapsed().as_secs_f64() * 1e3 / 100.0,
            frames.first().unwrap().len().div_euclid(1000)
        );
    }
}
