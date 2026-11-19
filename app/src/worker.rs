//! The camera thread and the pipeline thread, and what they share with the window.
//!
//! Live view only runs while something is watching, either an app using the
//! virtual camera or the preview window. That keeps the mirror down, the
//! sensor cool and the battery full the rest of the time.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use canon::{Camera, Frame, Prop};
use shared::{Format, Shared};

use crate::exposure::{self, Steady};
use crate::pipeline::{GPU, Look, OUTPUT, Pipeline, preview_output};
use windows::Win32::Media::MediaFoundation::MFGetSystemTime;

/// Live view stays up this long after the last watcher leaves, so switching
/// between apps doesn't flap the mirror.
const LINGER: Duration = Duration::from_secs(10);
/// The DLL counts as gone once it hasn't asked for a frame in this long.
const CONSUMER_IDLE_MS: u64 = 2000;
/// gphoto waits up to 3 s for the mirror and a focus pass, so a live view
/// that has sent nothing for longer than this has stopped. On the T3 that
/// usually means the sensor got warm.
const STALL: Duration = Duration::from_secs(6);
/// How long the camera is left alone after a stall.
const REST: Duration = Duration::from_secs(60);
/// How long errors have to go on before the camera is closed and left alone for
/// `ERROR_REST`. Just after power on the T3 can answer busy for a second or two while it
/// cleans its sensor, which shouldn't count.
const FAILING: Duration = Duration::from_secs(10);
const ERROR_REST: Duration = Duration::from_secs(20);
/// How often to look for a camera that isn't there when device arrivals can't be heard,
/// and as a backstop when they can.
const ABSENT_CHECK: Duration = Duration::from_secs(10);
/// How soon to try again when the camera is there but wouldn't open.
const REOPEN: Duration = Duration::from_secs(2);
const ARRIVAL_SETTLE: Duration = Duration::from_millis(500);
/// The mode dial's M, the one mode where the app can hold the exposure.
const MODE_M: u32 = 3;
/// What the window shows when no app has asked for a format.
const PREVIEW: Format = Format {
    width: 1280,
    height: 720,
    fourcc: shared::NV12,
};

#[expect(
    variant_size_differences,
    reason = "a few commands a second, so a boxed property set would cost more than it saves"
)]
pub(crate) enum Command {
    Set(Prop, u32),
    Autofocus,
    Focus(i8),
    /// Ends a cool-down early.
    Retry,
}

#[derive(Clone, Default)]
pub(crate) struct Setting {
    pub prop: Option<Prop>,
    pub value: Option<u32>,
    pub allowed: Vec<u32>,
}

#[derive(Clone, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts the window shows, not states of one machine"
)]
pub(crate) struct Status {
    pub camera: Option<String>,
    pub problem: Option<String>,
    pub live: bool,
    pub fps: f32,
    pub consumer: Option<Format>,
    pub battery: Option<u32>,
    pub mode: Option<u32>,
    pub settings: Vec<Setting>,
    /// Why the virtual camera couldn't be registered, if it couldn't.
    pub vcam: Option<String>,
    /// Whether NVIDIA's super resolution libraries were found.
    pub vsr_available: bool,
    /// Why super resolution was turned off, if it was.
    pub vsr_problem: Option<String>,
    /// Whether the camera is cooling down after a stall, so Retry means something.
    pub resting: bool,
    /// Decode plus render time per frame, averaged.
    pub frame_ms: f32,
    /// The same split by stage, averaged: decode, pixel repair, the GPU and output.
    pub stage_ms: [f32; 4],
    /// Hot and dead pixels being filled.
    pub stuck: usize,
    /// Whether frames are being paced and filled in for an app right now.
    pub smoothing: bool,
    /// The lens's AF/MF state as the body reports it, 3 being MF.
    pub focus_mode: Option<u32>,
    /// How far the frame's level is from steady exposure's target, in stops.
    pub off_target: Option<f32>,
    /// Why frame generation was turned off, if it was.
    pub smooth_problem: Option<String>,
}

impl Status {
    fn summary(&self) -> String {
        format!(
            "camera {:?}, live {}, consumer {:?}, problem {:?}, super resolution {} {:?}, virtual camera {:?}",
            self.camera,
            self.live,
            self.consumer,
            self.problem,
            self.vsr_available,
            self.vsr_problem,
            self.vcam
        )
    }
}

/// Appends to `%LOCALAPPDATA%\Viewfinder\viewfinder.log`, since the app has no console and
/// the window may never be opened. Starts over past 1 MB.
pub(crate) fn log(line: &str) {
    use std::io::Write;
    let Some(dir) =
        std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Viewfinder"))
    else {
        return;
    };
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("viewfinder.log");
    let fresh = std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(!fresh)
        .write(true)
        .truncate(fresh)
        .open(path);
    if let Ok(mut file) = file {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        writeln!(file, "{secs} {line}").ok();
    }
}

/// The newest preview frame, RGBA.
#[derive(Default)]
pub(crate) struct Preview {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub fresh: bool,
    /// Luma in 64 bins, from the same frame.
    pub histogram: Vec<u32>,
    /// Whether this is the camera's own picture rather than what apps get.
    pub original: bool,
    /// A camera original asked for with `Hub::want_original`, kept apart from the frames
    /// above so the processed picture never skips one.
    pub still: Vec<u8>,
    pub still_size: (usize, usize),
    pub still_fresh: bool,
}

/// What the window's repaint hook looks like.
type Repaint = Box<dyn Fn() + Send>;

pub(crate) struct Hub {
    pub(crate) status: Mutex<Status>,
    pub(crate) look: Mutex<Look>,
    pub(crate) preview: Mutex<Preview>,
    pub(crate) preview_wanted: AtomicBool,
    /// Show the camera's picture before any processing, for comparison.
    pub(crate) compare: AtomicBool,
    /// Send one camera original into `Preview::still`. Cleared once it's sent.
    pub(crate) want_original: AtomicBool,
    pub(crate) exposure: Mutex<exposure::Prefs>,
    /// The latest frame's level for steady exposure, numbered so the camera thread sees each once.
    brightness: Mutex<(u64, f32)>,
    consumer_active: AtomicBool,
    mailbox: Mutex<(Vec<u8>, bool)>,
    mail: Condvar,
    repaint: Mutex<Option<Repaint>>,
}

impl Hub {
    pub(crate) fn new(look: Look) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::default(),
            look: Mutex::new(look),
            preview: Mutex::default(),
            preview_wanted: AtomicBool::new(false),
            compare: AtomicBool::new(false),
            want_original: AtomicBool::new(false),
            exposure: Mutex::default(),
            brightness: Mutex::default(),
            consumer_active: AtomicBool::new(false),
            mailbox: Mutex::default(),
            mail: Condvar::new(),
            repaint: Mutex::new(None),
        })
    }

    pub(crate) fn on_repaint(&self, f: impl Fn() + Send + 'static) {
        *lock(&self.repaint) = Some(Box::new(f));
    }

    pub(crate) fn repaint(&self) {
        if let Some(f) = lock(&self.repaint).as_ref() {
            f();
        }
    }

    fn measured(&self, level: f32) {
        let mut b = lock(&self.brightness);
        *b = (b.0 + 1, level);
    }

    fn watched(&self) -> bool {
        self.consumer_active.load(Ordering::Relaxed) || self.preview_wanted.load(Ordering::Relaxed)
    }

    pub(crate) fn update(&self, f: impl FnOnce(&mut Status)) {
        let mut status = lock(&self.status);
        let before = status.summary();
        f(&mut status);
        let after = status.summary();
        drop(status);
        if after != before {
            log(&after);
        }
        self.repaint();
    }
}

/// A poisoned lock only means another thread panicked mid-update, and every
/// value here is safe to keep using after that.
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn spawn(hub: Arc<Hub>, commands: Receiver<Command>) {
    let h = hub.clone();
    thread::Builder::new()
        .name("camera".into())
        .spawn(move || camera_loop(&h, &commands))
        .ok();
    thread::Builder::new()
        .name("pipeline".into())
        .spawn(move || pipeline_loop(&hub))
        .ok();
}

fn camera_loop(hub: &Hub, commands: &Receiver<Command>) {
    crate::mta();
    let watching = canon::watch_arrivals();
    if !watching {
        log("can't register for device arrivals, so the camera is looked for every few seconds");
    }
    let mut camera = CameraThread {
        hub,
        commands,
        watching,
        camera: None,
        jpeg: Vec::new(),
        last_watched: None,
        counted: (Instant::now(), 0),
        last_frame: Instant::now(),
        rest_until: None,
        failing_since: None,
        exposure: SteadyState {
            started: Instant::now(),
            steady: Steady::default(),
            on: false,
            brightness: f32::NAN,
            seen: 0,
        },
    };
    loop {
        camera.step();
    }
}

/// What the camera thread keeps from one pass of its loop to the next.
struct CameraThread<'a> {
    hub: &'a Hub,
    commands: &'a Receiver<Command>,
    /// Whether device arrivals can be heard.
    watching: bool,
    camera: Option<Camera>,
    jpeg: Vec<u8>,
    /// When something last watched, or None if nothing has yet.
    last_watched: Option<Instant>,
    counted: (Instant, u32),
    last_frame: Instant,
    rest_until: Option<Instant>,
    failing_since: Option<Instant>,
    exposure: SteadyState,
}

/// Steady exposure as the camera thread runs it.
struct SteadyState {
    started: Instant,
    steady: Steady,
    on: bool,
    brightness: f32,
    /// The number of the last frame level fed to `steady`.
    seen: u64,
}

impl CameraThread<'_> {
    /// One pass of the loop: let the camera go, wait out a rest, open it, or pull a frame.
    fn step(&mut self) {
        if self.hub.watched() {
            self.last_watched = Some(Instant::now());
        }
        let wanted = self.last_watched.is_some_and(|t| t.elapsed() < LINGER);
        if !wanted {
            if let Some(cam) = self.camera.take() {
                let closed = cam.close();
                self.hub.update(|s| {
                    s.live = false;
                    s.fps = 0.0;
                    s.problem = closed.err().map(|e| e.to_string());
                });
            }
            drain(self.commands, Duration::from_millis(250));
            return;
        }
        if self.resting() {
            return;
        }
        if self.camera.is_none() {
            self.open();
            return;
        }
        let mut stalled = false;
        let result = self.pass(&mut stalled);
        self.settle(result, stalled);
    }

    /// Whether the camera is still being left alone after a stall or errors.
    fn resting(&mut self) -> bool {
        if self.rest_until.is_some_and(|t| Instant::now() < t) {
            // Turning the camera off and on again is the usual cure, so that ends the rest too.
            let retry = matches!(
                self.commands.recv_timeout(Duration::from_millis(500)),
                Ok(Command::Retry)
            );
            if retry || canon::arrived() {
                self.rest_until = None;
                self.hub.update(|s| {
                    s.resting = false;
                    s.problem = None;
                });
            }
            return true;
        }
        if self.rest_until.take().is_some() {
            self.hub.update(|s| s.resting = false);
        }
        false
    }

    fn open(&mut self) {
        // This try covers any arrival already flagged.
        canon::arrived();
        match Camera::open() {
            Ok(c) => {
                crate::vcam::remember_model(c.model());
                self.hub.update(|s| {
                    s.camera = Some(c.model().to_owned());
                    s.problem = None;
                });
                self.camera = Some(c);
                self.failing_since = None;
            }
            Err(e) => {
                self.hub.update(|s| {
                    s.camera = None;
                    s.problem = Some(format!(
                        "Can't reach the camera. Check that it's on and plugged in. ({e})"
                    ));
                });
                // A camera that's off comes back as a device arrival, so the slow check is
                // only a backstop. One that's there but won't open gets another go soon.
                let wait = if e == canon::Error::NotFound && self.watching {
                    ABSENT_CHECK
                } else {
                    REOPEN
                };
                wait_for_camera(self.commands, wait);
            }
        }
    }

    /// Runs queued commands, keeps live view going and hands the next frame to the pipeline.
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn pass(&mut self, stalled: &mut bool) -> Result<(), canon::Error> {
        let hub = self.hub;
        let Some(cam) = self.camera.as_mut() else {
            return Ok(());
        };
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                Command::Set(prop, value) => cam.set(prop, value)?,
                Command::Autofocus => cam.autofocus()?,
                Command::Focus(step) => cam.drive_lens(step)?,
                Command::Retry => {}
            }
        }
        if !lock(&hub.status).live {
            cam.start_live_view()?;
            self.last_frame = Instant::now();
            hub.update(|s| s.live = true);
            publish_settings(hub, cam);
            self.exposure.on = false;
        }
        match cam.frame(&mut self.jpeg)? {
            Frame::Jpeg => {
                let mut mailbox = lock(&hub.mailbox);
                std::mem::swap(&mut mailbox.0, &mut self.jpeg);
                mailbox.1 = true;
                hub.mail.notify_one();
                drop(mailbox);
                self.counted.1 += 1;
                self.last_frame = Instant::now();
            }
            Frame::NotReady if self.last_frame.elapsed() > STALL => *stalled = true,
            Frame::NotReady => thread::sleep(Duration::from_millis(5)),
        }

        let (seq, level, prefs) = self.exposure.update(hub, cam)?;

        if self.counted.0.elapsed() >= Duration::from_secs(1) {
            let fps = self.counted.1 as f32 / self.counted.0.elapsed().as_secs_f32();
            self.counted = (Instant::now(), 0);
            let off_target = (seq > 0).then_some(level - exposure::TARGET - prefs.brightness);
            hub.update(|s| {
                s.fps = fps;
                s.off_target = off_target.map(|l| (l * 10.0).round() / 10.0);
            });
            publish_settings(hub, cam);
        }
        Ok(())
    }

    /// Acts on how a pass went: a camera that's gone is dropped, one that stalled or keeps
    /// failing gets a rest, and any other error is shown.
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn settle(&mut self, result: Result<(), canon::Error>, stalled: bool) {
        // Any error with the camera missing from WPD's list is a disconnect whatever its
        // code, so it shouldn't count toward a rest.
        let gone = match &result {
            Err(canon::Error::Disconnected) => true,
            Err(_) => !canon::present(),
            Ok(()) => false,
        };
        if gone {
            self.camera = None;
            self.failing_since = None;
            self.hub.update(|s| {
                s.camera = None;
                s.live = false;
                s.fps = 0.0;
                s.problem = Some("The camera was turned off or unplugged. Live view comes back when it's on again.".into());
            });
            return;
        }
        if result.is_err() {
            self.failing_since.get_or_insert_with(Instant::now);
        } else {
            self.failing_since = None;
        }
        let failing = self.failing_since.is_some_and(|t| t.elapsed() >= FAILING);
        if stalled || failing {
            if let Some(cam) = self.camera.take() {
                cam.close().ok();
            }
            self.rest_until = Some(Instant::now() + if stalled { REST } else { ERROR_REST });
            self.failing_since = None;
            let why = if stalled {
                "The camera stopped sending live view, probably because it's warm. It gets a minute to cool down.".to_owned()
            } else {
                let last = result
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                format!(
                    "The camera kept failing ({last}), so it gets 20 seconds' rest before the next try."
                )
            };
            self.hub.update(|s| {
                s.problem = Some(why);
                s.live = false;
                s.fps = 0.0;
                s.resting = true;
            });
            return;
        }

        if let Err(e) = result {
            self.hub.update(|s| s.problem = Some(e.to_string()));
            thread::sleep(Duration::from_millis(250));
        }
    }
}

impl SteadyState {
    /// Feeds the newest frame level to steady exposure while the dial is on M, setting ISO and
    /// shutter when it asks. Returns the level's number and value and the preferences used.
    #[expect(
        clippy::float_cmp,
        reason = "any change to the brightness setting at all starts metering again"
    )]
    fn update(
        &mut self,
        hub: &Hub,
        cam: &mut Camera,
    ) -> Result<(u64, f32, exposure::Prefs), canon::Error> {
        let prefs = *lock(&hub.exposure);
        let t = self.started.elapsed().as_secs_f64();
        let active = prefs.steady && cam.mode() == Some(MODE_M);
        if active != self.on || prefs.brightness != self.brightness {
            (self.on, self.brightness) = (active, prefs.brightness);
            self.steady.remeter(t);
            self.seen = lock(&hub.brightness).0;
        }
        let (seq, level) = *lock(&hub.brightness);
        if active
            && seq != self.seen
            && let (Some(iso), Some(shutter)) = (cam.get(Prop::Iso), cam.get(Prop::Shutter))
        {
            self.seen = seq;
            let settings = exposure::Settings {
                iso,
                shutter,
                isos: cam.allowed(Prop::Iso),
                shutters: cam.allowed(Prop::Shutter),
            };
            if let Some(c) =
                self.steady
                    .update(t, level, &settings, exposure::TARGET + prefs.brightness)
            {
                log(&format!(
                    "steady exposure: level {level:.2}, target {:.2}, setting ISO {} and {}",
                    exposure::TARGET + prefs.brightness,
                    canon::values::iso_label(c.iso),
                    canon::values::shutter_label(c.shutter)
                ));
                cam.set(Prop::Shutter, c.shutter)?;
                cam.set(Prop::Iso, c.iso)?;
                publish_settings(hub, cam);
            }
        }
        Ok((seq, level, prefs))
    }
}

/// Waits up to `wait` for a camera to arrive or for Retry, dropping other commands since
/// there's no camera to send them to.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the camera thread, where waiting for the body is fine"
)]
fn wait_for_camera(commands: &Receiver<Command>, wait: Duration) {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if canon::arrived() {
            // WPD lists the camera a moment before its PTP session will open.
            thread::sleep(ARRIVAL_SETTLE);
            return;
        }
        match commands.recv_timeout(Duration::from_millis(250)) {
            Ok(Command::Retry) => return,
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(250)),
        }
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "runs on the camera thread, where waiting for the body is fine"
)]
fn drain(commands: &Receiver<Command>, wait: Duration) {
    match commands.recv_timeout(wait) {
        Ok(_) | Err(RecvTimeoutError::Timeout) => while commands.try_recv().is_ok() {},
        Err(RecvTimeoutError::Disconnected) => thread::sleep(wait),
    }
}

const SHOWN: [Prop; 7] = [
    Prop::Iso,
    Prop::Shutter,
    Prop::Aperture,
    Prop::ExposureComp,
    Prop::WhiteBalance,
    Prop::PictureStyle,
    Prop::AfMethod,
];

fn publish_settings(hub: &Hub, cam: &Camera) {
    let settings = SHOWN
        .iter()
        .map(|&p| Setting {
            prop: Some(p),
            value: cam.get(p),
            allowed: cam.allowed(p).to_vec(),
        })
        .collect();
    hub.update(|s| {
        s.settings = settings;
        s.battery = cam.battery();
        s.mode = cam.mode();
        s.focus_mode = cam.get(Prop::FocusMode);
    });
}

fn pipeline_loop(hub: &Hub) {
    let vfx = vfx_dir();
    hub.update(|s| s.vsr_available = vfx.is_some());
    let mut pipeline = PipelineThread {
        hub,
        pipeline: Pipeline::with_gpu(vfx),
        jpeg: Vec::new(),
        shared: None,
        last_open: None,
        open_failed: false,
        out: Output::default(),
        pacer: Pacer::default(),
        seq: 0,
        pacing: false,
        watching: false,
        last_watched: Instant::now(),
        builds: 0,
        trim_at: None,
    };
    loop {
        pipeline.step();
    }
}

/// What the pipeline thread keeps from one pass of its loop to the next.
struct PipelineThread<'a> {
    hub: &'a Hub,
    pipeline: Pipeline,
    jpeg: Vec<u8>,
    shared: Option<Shared>,
    /// When opening the shared frame buffer was last tried, if it has been.
    last_open: Option<Instant>,
    open_failed: bool,
    out: Output,
    pacer: Pacer,
    seq: u64,
    pacing: bool,
    watching: bool,
    last_watched: Instant,
    builds: u32,
    trim_at: Option<Instant>,
}

impl PipelineThread<'_> {
    /// One pass of the loop: wait for a camera frame or the next paced read, and serve it.
    fn step(&mut self) {
        self.open_shared();
        let wanted = self
            .shared
            .as_ref()
            .and_then(|s| s.wanted(CONSUMER_IDLE_MS));
        let active = wanted.is_some();
        if self.hub.consumer_active.swap(active, Ordering::Relaxed) != active {
            self.hub.update(|s| s.consumer = wanted);
        }
        let look = *lock(&self.hub.look);
        self.manage_gpu(wanted, look);
        let pace = active && self.pipeline.can_smooth(look);
        if pace != self.pacing {
            self.pacing = pace;
            self.pipeline.forget();
            self.pacer = Pacer::default();
            self.hub.update(|s| s.smoothing = pace);
        }

        // Condvar waits wake on the 15.6 ms system tick, which made 4 in 10 frames miss the
        // DLL's read. The last stretch before a frame is slept instead, since the standard
        // library sleeps on a high-resolution timer.
        let until = if self.pacing {
            self.pacer
                .until_tick(self.shared.as_ref(), self.out.frame_ms)
        } else {
            Duration::from_millis(200)
        };
        let close = self.pacing && until <= COARSE_WAKE;
        let wait = if close {
            Duration::ZERO
        } else if self.pacing {
            until.saturating_sub(COARSE_WAKE)
        } else {
            until
        };
        if take_mail(self.hub, wait, &mut self.jpeg) && !self.decode(look, wanted) {
            return;
        }
        if close {
            self.serve(wanted);
        }
    }

    fn open_shared(&mut self) {
        if self.shared.is_some()
            || self
                .last_open
                .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
        {
            return;
        }
        self.last_open = Some(Instant::now());
        self.shared = match Shared::open() {
            Ok(s) => {
                log("opened the shared frame buffer");
                Some(s)
            }
            Err(e) => {
                if !self.open_failed {
                    log(&format!("can't open the shared frame buffer yet ({e})"));
                }
                self.open_failed = true;
                None
            }
        };
    }

    /// Builds the GPU passes again when something starts watching, lets them go after a quiet
    /// spell, and trims memory after a build.
    fn manage_gpu(&mut self, wanted: Option<Format>, look: Look) {
        // Whatever was let go is built again while the camera starts up.
        let watched = self.hub.watched();
        if watched {
            if !self.watching {
                self.pipeline.prepare(wanted.unwrap_or(PREVIEW), look);
                if let Some(p) = self.pipeline.gpu_problem().map(str::to_owned) {
                    self.hub.update(|s| s.vsr_problem = Some(p));
                }
            }
            self.last_watched = Instant::now();
        } else if self.pipeline.on_gpu() && self.last_watched.elapsed() >= RELEASE_AFTER {
            self.pipeline.release();
            trim();
            log("let go of the GPU after a quiet spell");
        }
        self.watching = watched;
        if self.pipeline.builds() != self.builds {
            self.builds = self.pipeline.builds();
            self.trim_at = Some(Instant::now() + TRIM_AFTER_BUILD);
        }
        if self.trim_at.is_some_and(|t| Instant::now() >= t) {
            self.trim_at = None;
            trim();
        }
    }

    /// Decodes the frame just taken from the mailbox and holds or emits it. False if it was
    /// broken.
    fn decode(&mut self, look: Look, wanted: Option<Format>) -> bool {
        let started = Instant::now();
        if let Err(e) = self.pipeline.decode(&self.jpeg, look) {
            self.hub.update(|s| s.problem = Some(e));
            return false;
        }
        self.hub.measured(self.pipeline.brightness());
        if self.pacing {
            self.pipeline.hold(self.seq);
            self.pacer.arrived(self.seq, started);
            self.seq += 1;
        } else {
            self.out.emit(
                self.hub,
                &mut self.pipeline,
                self.shared.as_ref(),
                wanted,
                started,
            );
        }
        true
    }

    /// Sleeps the last stretch before the DLL's read and serves the paced frame for it.
    #[expect(
        clippy::disallowed_methods,
        reason = "the pipeline thread sleeps on the high-resolution timer to land before the read"
    )]
    fn serve(&mut self, wanted: Option<Format>) {
        thread::sleep(self.pacer.remaining());
        let started = Instant::now();
        if let Some(at) = self.pacer.advance() {
            self.pipeline.show_at(at);
            self.out.emit(
                self.hub,
                &mut self.pipeline,
                self.shared.as_ref(),
                wanted,
                started,
            );
            self.pacer.served();
            let problem = self.pipeline.smooth_problem().map(str::to_owned);
            if problem != self.out.smooth_problem {
                self.out.smooth_problem.clone_from(&problem);
                if let Some(p) = &problem {
                    log(&format!("frame generation is off ({p})"));
                }
                self.hub.update(|s| s.smooth_problem = problem);
            }
        }
    }
}

/// Takes the newest JPEG into `jpeg` if one arrives within `wait`.
fn take_mail(hub: &Hub, wait: Duration, jpeg: &mut Vec<u8>) -> bool {
    let (mut mailbox, _) = hub
        .mail
        .wait_timeout_while(lock(&hub.mailbox), wait, |m| !m.1)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let got = mailbox.1;
    if got {
        std::mem::swap(&mut mailbox.0, jpeg);
        mailbox.1 = false;
    }
    drop(mailbox);
    got
}

/// How long nothing has watched before the GPU passes and big buffers are let go. They hold
/// 460 MB of video memory and take 1.1 s to build again.
const RELEASE_AFTER: Duration = Duration::from_secs(60);

/// Building the GPU passes touches about 210 MB that frames never touch again. Trimmed once the
/// first frames have run, a call's working set settled at 52 MB rather than 268, with the same
/// frame times.
const TRIM_AFTER_BUILD: Duration = Duration::from_secs(3);

/// Hands pages nothing is using back to Windows. Frames fault back what they need.
#[expect(unsafe_code, reason = "trimming the working set is a Win32 call")]
fn trim() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    // SAFETY: the pseudo handle for this process, and MAX for both sizes means trim what can be.
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX).ok() };
}

/// Everything about getting a rendered frame to apps and the window.
#[derive(Default)]
struct Output {
    frame_no: u64,
    rgba: Vec<u8>,
    local: Vec<u8>,
    frame_ms: f32,
    stage_ms: [f32; 4],
    still: Vec<u8>,
    published: Option<Instant>,
    smooth_problem: Option<String>,
}

impl Output {
    /// Sends one camera original to the window if it asked for one. Only a decoded frame, since
    /// reading a generated one would come back from the GPU.
    fn send_still(&mut self, hub: &Hub, pipeline: &mut Pipeline) {
        if !pipeline.on_cpu() || !hub.want_original.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut still = std::mem::take(&mut self.still);
        let size = pipeline.preview(&mut still);
        let mut p = lock(&hub.preview);
        std::mem::swap(&mut p.still, &mut still);
        (p.still_size, p.still_fresh) = (size, true);
        drop(p);
        self.still = still;
    }

    /// Adds the stage times the pipeline gathered for this frame to the running averages.
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn time_stages(&mut self, pipeline: &mut Pipeline, begun: Instant, gpu_before: f32) {
        // Output is the rest of `emit`, with the GPU's share inside it taken out.
        pipeline.times[OUTPUT] +=
            (begun.elapsed().as_secs_f32() * 1000.0 - (pipeline.times[GPU] - gpu_before)).max(0.0);
        let times = std::mem::take(&mut pipeline.times);
        for (avg, t) in self.stage_ms.iter_mut().zip(times) {
            *avg = if *avg == 0.0 { t } else { *avg * 0.9 + t * 0.1 };
        }
    }

    /// Renders the pipeline's current frame for the app reading the camera, if any, and the
    /// window, if it's open.
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn emit(
        &mut self,
        hub: &Hub,
        pipeline: &mut Pipeline,
        shared: Option<&Shared>,
        wanted: Option<Format>,
        started: Instant,
    ) {
        let preview = hub.preview_wanted.load(Ordering::Relaxed);
        let original = hub.compare.load(Ordering::Relaxed);
        if preview && !original {
            self.send_still(hub, pipeline);
        }
        let begun = Instant::now();
        let gpu_before = pipeline.times[GPU];
        let mut previewed = None;
        if let (Some(s), Some(f)) = (shared, wanted) {
            self.frame_no += 1;
            let mut failed = None;
            let rgba = &mut self.rgba;
            s.write(f, self.frame_no, |buf| match pipeline.render(f, buf) {
                Ok(()) => {
                    if preview && !original {
                        previewed = Some(preview_output(f, buf, rgba));
                    }
                    true
                }
                Err(e) => {
                    failed = Some(e);
                    false
                }
            });
            if let Some(e) = failed {
                let gpu = pipeline.gpu_problem().map(str::to_owned);
                hub.update(|s| match gpu {
                    Some(g) => s.vsr_problem = Some(g),
                    None => s.problem = Some(e),
                });
            }
        }
        // With no app using the camera, the window still shows the processed picture, so
        // what gets set up is what calls will see.
        if preview && !original && previewed.is_none() {
            self.local.resize(PREVIEW.bytes(), 0);
            if pipeline.render(PREVIEW, &mut self.local).is_ok() {
                previewed = Some(preview_output(PREVIEW, &self.local, &mut self.rgba));
            }
        }
        if !preview {
            self.local = Vec::new();
        }
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        self.frame_ms = if self.frame_ms == 0.0 {
            ms
        } else {
            self.frame_ms * 0.9 + ms * 0.1
        };
        self.time_stages(pipeline, begun, gpu_before);
        if self
            .published
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.published = Some(Instant::now());
            let stuck = pipeline.stuck_pixels();
            let frame_ms = (self.frame_ms * 10.0).round() / 10.0;
            let stage_ms = self.stage_ms.map(|t| (t * 10.0).round() / 10.0);
            hub.update(|s| {
                s.frame_ms = frame_ms;
                s.stage_ms = stage_ms;
                s.stuck = stuck;
            });
        }
        if preview {
            let (w, h) = previewed.unwrap_or_else(|| pipeline.preview(&mut self.rgba));
            let histogram = histogram(&self.rgba);
            let mut p = lock(&hub.preview);
            std::mem::swap(&mut p.rgba, &mut self.rgba);
            (p.width, p.height, p.fresh, p.histogram, p.original) =
                (w, h, true, histogram, previewed.is_none());
            drop(p);
            hub.repaint();
        }
    }
}

/// Plays camera frames out at the virtual camera's even 30 fps. The T3's frames land anywhere
/// from 16 to 70 ms apart, so reading the newest one every 33 ms showed apps only 21 to 24
/// different frames a second. Instead the pacer runs about two frames behind the newest and
/// moves through them at the camera's own average rate, so every read lands at a fresh moment.
#[derive(Default)]
struct Pacer {
    arrivals: VecDeque<Instant>,
    newest: Option<u64>,
    /// Position in frame numbers, 4.25 being a quarter of the way from frame 4 to 5.
    at: f64,
    /// `MFGetSystemTime` of the DLL read the next frame is for, and of the last one served.
    target: i64,
    served: i64,
    /// How long before `target` to start, in the same units.
    lead: i64,
}

/// Frames the pacer stays behind the newest. In a replay of recorded arrival times, 2 behind
/// repeated 1.5% of frames and 2.5 none, for about 60 and 70 ms of delay.
const BUFFER: f64 = 2.2;
/// How hard the pacer speeds up or slows down to hold `BUFFER`, as a fraction per frame of error.
const GAIN: f64 = 0.2;
const INTERVAL_100NS: i64 = 333_333;
/// How early to stop waiting on the mailbox before a frame is due, a tick of the system timer
/// plus some slack.
const COARSE_WAKE: Duration = Duration::from_millis(20);

impl Pacer {
    fn arrived(&mut self, seq: u64, at: Instant) {
        self.arrivals.push_back(at);
        if self.arrivals.len() > 8 {
            self.arrivals.pop_front();
        }
        let first = self.newest.is_none();
        self.newest = Some(seq);
        if first || seq as f64 - self.at > 8.0 {
            self.at = seq as f64 - BUFFER;
        }
    }

    /// How long until work on the next frame should start, so it lands just before the DLL
    /// reads. Falls back to its own 30 Hz clock when the DLL doesn't say.
    #[expect(unsafe_code, reason = "MFGetSystemTime is a Media Foundation call")]
    fn until_tick(&mut self, shared: Option<&Shared>, frame_ms: f32) -> Duration {
        // SAFETY: takes nothing.
        let now = unsafe { MFGetSystemTime() };
        let next = shared.map_or(0, Shared::next_read);
        self.target = if next > now && next > self.served {
            next
        } else {
            (self.served + INTERVAL_100NS).max(now)
        };
        // The render plus room for a camera frame decoding just before it. With 3 ms of room,
        // 1 frame in 7 still missed the read.
        self.lead = ((frame_ms + 9.0) * 10_000.0) as i64;
        let wait = (self.target - self.lead - now).max(0) as u64;
        Duration::from_nanos(wait * 100).min(Duration::from_millis(200))
    }

    #[expect(unsafe_code, reason = "MFGetSystemTime is a Media Foundation call")]
    fn remaining(&self) -> Duration {
        // SAFETY: takes nothing.
        let now = unsafe { MFGetSystemTime() };
        Duration::from_nanos((self.target - self.lead - now).max(0) as u64 * 100)
    }

    const fn served(&mut self) {
        self.served = self.target;
    }

    /// Moves on by one output frame and says where to draw.
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn advance(&mut self) -> Option<f64> {
        let newest = self.newest? as f64;
        let rate = match (self.arrivals.front(), self.arrivals.back()) {
            (Some(a), Some(b)) if self.arrivals.len() >= 4 => {
                (self.arrivals.len() - 1) as f64 / (*b - *a).as_secs_f64().max(1e-3)
            }
            _ => 28.0,
        };
        let level = newest - self.at;
        let step = rate.clamp(5.0, 30.0) / 30.0 * (1.0 + GAIN * (level - BUFFER)).clamp(0.5, 1.5);
        self.at = (self.at + step).min(newest);
        Some(self.at)
    }
}

/// Where NVIDIA's VideoSuperRes DLLs live: `VIEWFINDER_VFX` if set, otherwise
/// `vfx` next to the exe. They're NVIDIA's to distribute, so install.ps1
/// fetches them rather than the repo carrying them.
fn vfx_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("VIEWFINDER_VFX")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()
                .map(|p| p.join("vfx"))
        })?;
    dir.join("NVVideoEffects.dll").is_file().then_some(dir)
}

/// Luma in 64 bins from RGBA, every second pixel, which is plenty for a histogram.
fn histogram(rgba: &[u8]) -> Vec<u32> {
    let mut bins = vec![0u32; 64];
    for px in rgba.as_chunks::<4>().0.iter().step_by(2) {
        let y = (u32::from(px[0]) * 54 + u32::from(px[1]) * 183 + u32::from(px[2]) * 19) >> 8;
        if let Some(bin) = bins.get_mut((y >> 2) as usize) {
            *bin += 1;
        }
    }
    bins
}

#[cfg(test)]
#[path = "bench.rs"]
mod bench;
