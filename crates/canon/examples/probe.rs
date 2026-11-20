//! Prints the camera's state and times live view for `VIEWFINDER_SECS` (10 s).
//! `cargo run -p canon --example probe --release -- [--save <dir>]`

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use canon::{Camera, Frame, Prop, jpeg_size, values};

fn show(camera: &Camera, name: &str, prop: Prop, label: fn(u32) -> String) {
    let current = camera
        .get(prop)
        .map_or_else(|| "not announced".into(), label);
    let allowed: Vec<String> = camera.allowed(prop).iter().map(|&v| label(v)).collect();
    println!("{name}: {current}");
    println!(
        "  allowed: {}",
        if allowed.is_empty() {
            "no list".into()
        } else {
            allowed.join(", ")
        }
    );
}

/// Start ms, duration ms, whether a frame came, JPEG length.
type Call = (f64, f64, bool, usize);

struct Stats {
    frames: u32,
    not_ready: u32,
    repeats: u32,
    bytes: usize,
    min_size: usize,
    max_size: usize,
    fastest: Duration,
    slowest: Duration,
    total: Duration,
    first_at: Option<Duration>,
    last_at: Duration,
    first_frame: Option<(Duration, Vec<u8>)>,
    sizes: BTreeMap<Option<(u16, u16)>, u32>,
    timeline: Option<Vec<Call>>,
    secs: f64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let save = save_dir()?;

    let started = Instant::now();
    let mut camera = Camera::open()?;
    println!("opened {} in {:?}", camera.model(), started.elapsed());
    print_state(&camera);

    let iso_before = camera.get(Prop::Iso);
    if let Some(iso) = std::env::var("VIEWFINDER_ISO")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok())
    {
        camera.set(Prop::Iso, iso)?;
        println!("ISO set to {}", values::iso_label(iso));
    }

    let started = Instant::now();
    camera.start_live_view()?;
    println!("live view started in {:?}", started.elapsed());

    let stats = pull(&mut camera)?;
    if let (Some(log), Some(path)) = (&stats.timeline, std::env::var_os("VIEWFINDER_TIMELINE")) {
        let mut text = String::new();
        for (at, took, f, n) in log {
            writeln!(text, "{at:.2},{took:.2},{},{n}", u8::from(*f))?;
        }
        std::fs::write(path, text)?;
    }
    report(&stats);
    if let Some((at, data)) = &stats.first_frame {
        println!("first frame after {at:?}");
        if let Some(dir) = &save {
            std::fs::create_dir_all(dir)?;
            let path = dir.join("first-frame.jpg");
            std::fs::write(&path, data)?;
            println!("saved {}", path.display());
        }
    }

    camera.stop_live_view()?;
    if let Some(iso) = iso_before.filter(|_| std::env::var_os("VIEWFINDER_ISO").is_some()) {
        camera.set(Prop::Iso, iso)?;
    }
    camera.close()?;
    println!("closed cleanly");
    Ok(())
}

fn save_dir() -> Result<Option<PathBuf>, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut save = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--save" => save = Some(PathBuf::from(args.next().ok_or("--save needs a folder")?)),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    Ok(save)
}

fn print_state(camera: &Camera) {
    match camera.mode() {
        Some(m) if values::is_creative_mode(m) => println!("mode: {}", values::ae_mode_label(m)),
        Some(m) => println!(
            "mode: {} (scene mode, exposure settings are locked)",
            values::ae_mode_label(m)
        ),
        None => println!("mode: not announced"),
    }
    println!(
        "battery: {}",
        camera
            .battery()
            .map_or_else(|| "not announced".into(), values::battery_label)
    );
    show(camera, "ISO", Prop::Iso, values::iso_label);
    show(camera, "aperture", Prop::Aperture, values::aperture_label);
    show(camera, "shutter", Prop::Shutter, values::shutter_label);
    show(
        camera,
        "white balance",
        Prop::WhiteBalance,
        values::white_balance_label,
    );
    show(
        camera,
        "live view AF",
        Prop::AfMethod,
        values::af_method_label,
    );
    println!(
        "raw: mode {:?}, mode dial {:?}, focus mode {:?}, exposure simulation {:?}",
        camera.get(Prop::AeMode),
        camera.get(Prop::AeModeDial),
        camera.get(Prop::FocusMode),
        camera.get(Prop::ExposureSim)
    );
    println!(
        "EVF output: {:?}, EVF mode: {:?}",
        camera.get(Prop::EvfOutput),
        camera.get(Prop::EvfMode)
    );

    for (code, value) in camera.unnamed() {
        println!("unnamed property {code:#06x} = {value:#x}");
    }
}

fn pull(camera: &mut Camera) -> Result<Stats, Box<dyn Error>> {
    let mut jpeg = Vec::new();
    let mut previous = Vec::new();
    let mut s = Stats {
        frames: 0,
        not_ready: 0,
        repeats: 0,
        bytes: 0,
        min_size: usize::MAX,
        max_size: 0,
        fastest: Duration::MAX,
        slowest: Duration::ZERO,
        total: Duration::ZERO,
        first_at: None,
        last_at: Duration::ZERO,
        first_frame: None,
        sizes: BTreeMap::new(),
        timeline: std::env::var_os("VIEWFINDER_TIMELINE").map(|_| Vec::new()),
        secs: 0.0,
    };
    let secs_wanted = std::env::var("VIEWFINDER_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let run = Instant::now();
    while run.elapsed() < Duration::from_secs(secs_wanted) {
        let call = Instant::now();
        let result = camera.frame(&mut jpeg)?;
        let took = call.elapsed();
        if let Some(log) = s.timeline.as_mut() {
            log.push((
                run.elapsed().as_secs_f64() * 1e3,
                took.as_secs_f64() * 1e3,
                matches!(result, Frame::Jpeg),
                jpeg.len(),
            ));
        }
        s.fastest = s.fastest.min(took);
        s.slowest = s.slowest.max(took);
        s.total += took;
        match result {
            Frame::NotReady => s.not_ready += 1,
            Frame::Jpeg => {
                s.frames += 1;
                s.bytes += jpeg.len();
                s.min_size = s.min_size.min(jpeg.len());
                s.max_size = s.max_size.max(jpeg.len());
                *s.sizes.entry(jpeg_size(&jpeg)).or_insert(0u32) += 1;
                if jpeg == previous {
                    s.repeats += 1;
                } else {
                    previous.clone_from(&jpeg);
                }
                s.last_at = run.elapsed();
                s.first_at.get_or_insert(s.last_at);
                if s.first_frame.is_none() {
                    s.first_frame = Some((run.elapsed(), jpeg.clone()));
                }
            }
        }
    }
    s.secs = run.elapsed().as_secs_f64();
    Ok(s)
}

fn report(s: &Stats) {
    let (frames, secs) = (s.frames, s.secs);
    let calls = frames + s.not_ready;

    println!(
        "{calls} calls in {secs:.1} s: {frames} frames, {} not ready",
        s.not_ready
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
        println!(
            "JPEG bytes: min {}, avg {}, max {}",
            s.min_size,
            s.bytes.div_euclid(frames as usize),
            s.max_size
        );
        for (dims, n) in &s.sizes {
            match dims {
                Some((w, h)) => println!("  {w}x{h}: {n} frames"),
                None => println!("  no SOF marker: {n} frames"),
            }
        }
    }
}
