//! The camera and pipeline threads. Live view only runs while an app or the window watches,
//! which keeps the mirror down, the sensor cool and the battery full the rest of the time.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use canon::{Camera, Frame, Prop};
use shared::{Format, Shared};

use crate::exposure::{self, Steady};
use crate::face;
use crate::focus::{self, Search, Watch};
use crate::pipeline::{GPU, Look, OUTPUT, Pipeline, crop_size, preview_output};
use crate::{autoframe, general, white};
use windows::Win32::Media::MediaFoundation::MFGetSystemTime;

/// So switching apps doesn't flap the mirror.
const LINGER: Duration = Duration::from_secs(10);
const CONSUMER_IDLE_MS: u64 = 2000;
/// gphoto waits up to 3 s for the mirror and a focus pass, so a live view that has sent nothing
/// for longer has stopped. On the T3 that usually means the sensor got warm.
const STALL: Duration = Duration::from_secs(6);
const NO_ANSWER: Duration = Duration::from_secs(3);
const REST: Duration = Duration::from_secs(60);
/// Just after power on the T3 can answer busy for a second or two while it cleans its sensor.
const FAILING: Duration = Duration::from_secs(10);
const ERROR_REST: Duration = Duration::from_secs(20);
const ABSENT_CHECK: Duration = Duration::from_secs(10);
const REOPEN: Duration = Duration::from_secs(2);
const ARRIVAL_SETTLE: Duration = Duration::from_millis(500);
const EASE: f32 = 0.04;
/// How much cooler than skin's target the middle of Warmth sits, where it looked right live.
const COOLER: f32 = 0.05;
const SAVE_SHOWN: Duration = Duration::from_secs(10);
/// The face finder looks twice a second and takes a moment to start.
const FACE_WAIT: Duration = Duration::from_millis(1500);
const SHOW_AFTER: Duration = Duration::from_secs(2);
const ASLEEP: &str = "Can't find the camera. Turn it on, or half-press the shutter if it's asleep.";
pub(crate) const REFUSED: &str = "The camera is there but won't connect. Trying again.";

fn plain(e: &canon::Error) -> String {
    match e {
        canon::Error::NotFound | canon::Error::Disconnected => ASLEEP,
        canon::Error::Ptp {
            code: 0x2019 | 0xA102,
            ..
        } => "The camera is busy. Trying again.",
        canon::Error::Ptp { code: 0xA101, .. } => "The camera's battery is too low.",
        canon::Error::Ptp { code: 0xA005, .. } => {
            "The camera said no. Check that it isn't showing a menu."
        }
        canon::Error::NotAllowed { .. } | canon::Error::Unannounced(_) => {
            "The camera won't take that setting in this mode."
        }
        _ => "The camera didn't answer properly. Trying again.",
    }
    .to_owned()
}
const MODE_M: u32 = 3;
const FOCUS_MF: u32 = 3;
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
    pub vcam: Option<String>,
    pub vsr_available: bool,
    pub vsr_problem: Option<String>,
    pub resting: bool,
    pub paused: bool,
    pub frame_ms: f32,
    /// Decode, pixel repair, the GPU and output.
    pub stage_ms: [f32; 4],
    pub stuck: usize,
    pub smoothing: bool,
    pub focus_mode: Option<u32>,
    pub off_target: Option<f32>,
    pub focus: focus::Report,
    pub white_held: Option<u32>,
    pub light: Option<u32>,
    pub white_unseen: bool,
    pub hidden: bool,
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

/// Appends to `%LOCALAPPDATA%\Viewfinder\viewfinder.log`, starting over past 1 MB.
pub(crate) fn log(line: &str) {
    use std::io::Write;
    if cfg!(test) {
        return;
    }
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

#[derive(Default)]
pub(crate) struct Preview {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub fresh: bool,
    pub histogram: Vec<u32>,
    /// The camera's own picture for Compare, kept apart so the processed picture never skips.
    pub still: Vec<u8>,
    pub still_size: (usize, usize),
    pub still_fresh: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Measure {
    frame: u64,
    level: f32,
    sharpness: Option<f32>,
    face_width: Option<f32>,
    cast: Option<white::Cast>,
}

type Repaint = Box<dyn Fn() + Send>;

pub(crate) struct Hub {
    pub(crate) status: Mutex<Status>,
    pub(crate) look: Mutex<Look>,
    pub(crate) preview: Mutex<Preview>,
    pub(crate) preview_wanted: AtomicBool,
    pub(crate) compare: AtomicBool,
    /// Each costs about 0.2 ms a frame.
    want_sharpness: AtomicBool,
    want_cast: AtomicBool,
    pub(crate) want_original: AtomicBool,
    pub(crate) exposure: Mutex<exposure::Prefs>,
    pub(crate) chosen: Mutex<capture::Choice>,
    pub(crate) general: Mutex<general::Prefs>,
    pub(crate) adjust: Mutex<crate::adjust::Adjust>,
    pub(crate) want_auto: AtomicBool,
    /// Never saved, so each start shows the camera.
    hidden: AtomicBool,
    measure: Mutex<Measure>,
    post_white: Mutex<Option<white::Cast>>,
    consumer_active: AtomicBool,
    paused: AtomicBool,
    quit: AtomicBool,
    released: AtomicBool,
    /// The newest frame, whether it's new, its number and its format.
    mailbox: Mutex<(Vec<u8>, bool, u64, capture::Format)>,
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
            want_sharpness: AtomicBool::new(false),
            want_cast: AtomicBool::new(false),
            want_original: AtomicBool::new(false),
            exposure: Mutex::default(),
            chosen: Mutex::default(),
            general: Mutex::default(),
            adjust: Mutex::default(),
            want_auto: AtomicBool::new(false),
            hidden: AtomicBool::new(false),
            measure: Mutex::default(),
            post_white: Mutex::new(None),
            consumer_active: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            released: AtomicBool::new(false),
            mailbox: Mutex::new((Vec::new(), false, 0, capture::Format::Jpeg)),
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

    fn watched(&self) -> bool {
        !self.hidden()
            && (self.consumer_active.load(Ordering::Relaxed)
                || (self.preview_wanted.load(Ordering::Relaxed)
                    && !self.paused.load(Ordering::Relaxed)))
    }

    pub(crate) fn hidden(&self) -> bool {
        self.hidden.load(Ordering::Relaxed)
    }

    pub(crate) fn set_hidden(&self, hidden: bool) {
        if self.hidden.swap(hidden, Ordering::Relaxed) != hidden {
            log(if hidden {
                "camera hidden"
            } else {
                "camera shown"
            });
            self.update(|s| s.hidden = hidden);
        }
    }

    fn post(&self, frame: &mut Vec<u8>, pulled: u64, format: capture::Format) {
        let mut mailbox = lock(&self.mailbox);
        std::mem::swap(&mut mailbox.0, frame);
        (mailbox.1, mailbox.2, mailbox.3) = (true, pulled, format);
        drop(mailbox);
        self.mail.notify_one();
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
    thread::Builder::new()
        .name("camera watch".into())
        .spawn(watch_camera)
        .ok();
}

/// A camera that drains its battery or slows down before it locks up shows here first.
fn tally(asked: &mut (Instant, u32, u32, Duration), cam: &Camera) {
    asked.2 += 1;
    if asked.0.elapsed() >= Duration::from_secs(60) {
        log(&format!(
            "camera, last minute: {} frames from {} requests taking {:.1} ms each, battery {:?}",
            asked.1,
            asked.2,
            asked.3.as_secs_f64() * 1e3 / f64::from(asked.2.max(1)),
            cam.battery().map(canon::values::battery_label)
        ));
        *asked = (Instant::now(), 0, 0, Duration::ZERO);
    }
}

/// A request the camera sits on is the last thing it does before it locks up.
#[expect(
    clippy::disallowed_methods,
    reason = "a thread of its own that only checks once a second"
)]
fn watch_camera() {
    let mut told = false;
    loop {
        thread::sleep(Duration::from_secs(1));
        match canon::wpd::waiting() {
            Some((op, waited)) if waited >= NO_ANSWER => {
                if !told {
                    log(&format!(
                        "the camera hasn't answered {op:#06x} in {:.0} s",
                        waited.as_secs_f32()
                    ));
                    told = true;
                }
            }
            _ if told => {
                log("the camera answered again");
                told = false;
            }
            _ => {}
        }
    }
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
        chosen: capture::Choice::default(),
        camera: None,
        other: None,
        jpeg: Vec::new(),
        last_watched: None,
        arrivals: 0,
        window_alone: None,
        counted: (Instant::now(), 0),
        asked: (Instant::now(), 0, 0, Duration::ZERO),
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
        pulled: 0,
        focus: FocusState::default(),
        white: WhiteState::default(),
    };
    while !hub.quit.load(Ordering::Relaxed) {
        camera.step();
    }
    match camera.release() {
        Ok(()) => log("handed the camera back on the way out"),
        Err(e) => log(&format!(
            "couldn't hand the camera back on the way out ({e})"
        )),
    }
    hub.released.store(true, Ordering::Relaxed);
}

/// Without it the T3 kept streaming with its sensor on, and with the lighting optimizer and
/// white balance as live view set them.
#[expect(
    clippy::disallowed_methods,
    reason = "runs once on the way out, after the window has closed"
)]
pub(crate) fn let_go(hub: &Hub) {
    hub.quit.store(true, Ordering::Relaxed);
    let started = Instant::now();
    while !hub.released.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(20));
    }
}

struct CameraThread<'a> {
    hub: &'a Hub,
    commands: &'a Receiver<Command>,
    watching: bool,
    chosen: capture::Choice,
    camera: Option<Camera>,
    other: Option<Box<dyn capture::Source>>,
    jpeg: Vec<u8>,
    last_watched: Option<Instant>,
    arrivals: u64,
    window_alone: Option<Instant>,
    counted: (Instant, u32),
    /// When the minute started, its frames, the requests for one and their time.
    asked: (Instant, u32, u32, Duration),
    last_frame: Instant,
    rest_until: Option<Instant>,
    failing_since: Option<Instant>,
    exposure: SteadyState,
    pulled: u64,
    focus: FocusState,
    white: WhiteState,
}

struct SteadyState {
    started: Instant,
    steady: Steady,
    on: bool,
    brightness: f32,
    seen: u64,
}

impl CameraThread<'_> {
    fn step(&mut self) {
        if canon::arrivals() != self.arrivals {
            self.arrivals = canon::arrivals();
            log("the camera turned on or was plugged in");
        }
        self.follow_choice();
        self.pause_if_alone();
        if self.hub.watched() {
            self.last_watched = Some(Instant::now());
        }
        let wanted = !self.hub.hidden() && self.last_watched.is_some_and(|t| t.elapsed() < LINGER);
        if !wanted {
            if self.hub.paused.load(Ordering::Relaxed) {
                if resume_asked(self.commands, Duration::from_millis(250)) {
                    self.window_alone = Some(Instant::now());
                    self.pause_if_alone();
                }
                if self.camera.is_none() {
                    return;
                }
            }
            if let Some(other) = self.other.take() {
                let closed = other.close();
                self.hub.update(|s| {
                    s.live = false;
                    s.problem = closed.err().map(|e| e.to_string());
                });
            }
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
        if self.camera.is_none() && self.other.is_none() {
            self.open();
            return;
        }
        if self.other.is_some() {
            self.pass_other();
            return;
        }
        let mut stalled = false;
        let result = self.pass(&mut stalled);
        self.settle(result, stalled);
    }

    fn release(&mut self) -> Result<(), String> {
        match (self.camera.take(), self.other.take()) {
            (Some(cam), _) => cam.close().map_err(|e| e.to_string()),
            (_, Some(other)) => other.close().map_err(|e| e.to_string()),
            (None, None) => Ok(()),
        }
    }

    fn follow_choice(&mut self) {
        let chosen = lock(&self.hub.chosen).clone();
        if chosen == self.chosen {
            return;
        }
        log(&format!("switching to {}", chosen.label()));
        self.chosen = chosen;
        let closed = self.release();
        self.rest_until = None;
        self.failing_since = None;
        self.focus = FocusState::default();
        self.white = WhiteState::default();
        self.hub.want_sharpness.store(false, Ordering::Relaxed);
        self.hub.want_cast.store(false, Ordering::Relaxed);
        self.hub.update(|s| {
            s.camera = None;
            s.live = false;
            s.fps = 0.0;
            s.resting = false;
            s.problem = closed.err();
        });
    }

    /// Live view warms the T3 and runs its battery down.
    fn pause_if_alone(&mut self) {
        let alone = !self.hub.consumer_active.load(Ordering::Relaxed)
            && self.hub.preview_wanted.load(Ordering::Relaxed);
        self.window_alone = alone.then(|| self.window_alone.unwrap_or_else(Instant::now));
        let after = lock(&self.hub.general).pause();
        let paused = self
            .window_alone
            .zip(after)
            .is_some_and(|(t, after)| t.elapsed() >= after);
        if self.hub.paused.swap(paused, Ordering::Relaxed) != paused {
            self.hub.update(|s| s.paused = paused);
        }
    }

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
        canon::arrived();
        let opened = match &self.chosen {
            capture::Choice::Ptp {
                vendor: capture::Vendor::Canon,
                model,
            } if model.is_empty() => Camera::open(),
            capture::Choice::Ptp {
                vendor: capture::Vendor::Canon,
                ..
            } => capture::ptp_id(&self.chosen)
                .map_or(Err(canon::Error::NotFound), |id| Camera::open_at(&id)),
            _ => return self.open_other(),
        };
        match opened {
            Ok(c) => {
                crate::vcam::remember_model(c.model());
                self.hub.update(|s| {
                    s.camera = Some(c.model().to_owned());
                    s.problem = None;
                });
                self.camera = Some(c);
                self.failing_since = None;
            }
            Err(e) => self.unreachable(&e.to_string(), e == canon::Error::NotFound),
        }
    }

    fn open_other(&mut self) {
        match capture::open(&self.chosen) {
            Ok(other) => {
                crate::vcam::remember_model(other.model());
                self.hub.update(|s| {
                    s.camera = Some(other.model().to_owned());
                    s.problem = None;
                });
                self.other = Some(other);
                self.failing_since = None;
            }
            Err(e) => self.unreachable(&e.to_string(), e == capture::Error::NotFound),
        }
    }

    fn unreachable(&self, why: &str, absent: bool) {
        let said = if absent { ASLEEP } else { REFUSED };
        if lock(&self.hub.status).problem.as_deref() != Some(said) {
            log(&format!("can't open the camera: {why}"));
        }
        self.hub.update(|s| {
            s.camera = None;
            s.problem = Some(said.to_owned());
        });
        let wait = if absent && self.watching {
            ABSENT_CHECK
        } else {
            REOPEN
        };
        wait_for_camera(self.commands, wait);
    }

    fn run_commands(&mut self) -> Result<(), canon::Error> {
        let hub = self.hub;
        let Some(cam) = self.camera.as_mut() else {
            return Ok(());
        };
        while let Ok(cmd) = self.commands.try_recv() {
            // Someone at the window counts as watching, so the pause waits for them.
            self.window_alone = self.window_alone.map(|_| Instant::now());
            let manual = cam.get(Prop::FocusMode) == Some(FOCUS_MF);
            match cmd {
                Command::Set(prop, value) => cam.set(prop, value)?,
                Command::Autofocus if manual || !lock(&hub.exposure).autofocus => {}
                Command::Autofocus if lock(&hub.exposure).camera_af => {
                    cam.autofocus()?;
                    self.focus.forget();
                }
                Command::Autofocus => self.focus.start(hub, focus::SEARCH, self.pulled),
                Command::Focus(step) => {
                    self.focus
                        .stop(hub, self.exposure.started.elapsed().as_secs_f64());
                    self.focus.drive(cam, step)?;
                    self.focus.keep();
                }
                Command::Retry => {}
            }
        }
        Ok(())
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn pass(&mut self, stalled: &mut bool) -> Result<(), canon::Error> {
        let hub = self.hub;
        self.run_commands()?;
        let Some(cam) = self.camera.as_mut() else {
            return Ok(());
        };
        if !lock(&hub.status).live {
            cam.start_live_view()?;
            let prefs = *lock(&hub.exposure);
            if self.focus.lost(&prefs, cam) {
                self.focus.restore(hub, cam, self.pulled)?;
            } else if prefs.autofocus
                && prefs.hold_focus
                && !prefs.camera_af
                && cam.get(Prop::FocusMode) != Some(FOCUS_MF)
            {
                // Live view starts again whenever an app opens the camera.
                self.focus.start(hub, focus::NUDGE, self.pulled);
            }
            self.last_frame = Instant::now();
            log(&format!(
                "live view: dial {:?}, mode {:?}, ISO {:?}, shutter {:?}, lighting optimizer {:?}, exposure simulation {:?}, auto power off {:?}",
                cam.get(Prop::AeModeDial),
                cam.get(Prop::AeMode),
                cam.get(Prop::Iso).map(canon::values::iso_label),
                cam.get(Prop::Shutter).map(canon::values::shutter_label),
                cam.get(Prop::Alo).map(|v| format!("{v:#x}")),
                cam.get(Prop::ExposureSim),
                cam.get(Prop::AutoPowerOff),
            ));
            log(&format!(
                "white balance {:?}, colour temperature {:?}, allowed {:?} and {:?}, unnamed {:?}",
                cam.get(Prop::WhiteBalance),
                cam.get(Prop::ColorTemp),
                cam.allowed(Prop::WhiteBalance),
                cam.allowed(Prop::ColorTemp),
                cam.unnamed(),
            ));
            hub.update(|s| s.live = true);
            publish_settings(hub, cam);
            self.exposure.on = false;
            self.white.on = false;
        }
        tally(&mut self.asked, cam);
        let asked_at = Instant::now();
        let pulled = cam.frame(&mut self.jpeg);
        self.asked.3 += asked_at.elapsed();
        match pulled? {
            Frame::Jpeg => {
                self.asked.1 += 1;
                self.pulled += 1;
                hub.post(&mut self.jpeg, self.pulled, capture::Format::Jpeg);
                self.counted.1 += 1;
                self.last_frame = Instant::now();
            }
            Frame::NotReady if self.last_frame.elapsed() > STALL => *stalled = true,
            Frame::NotReady => thread::sleep(Duration::from_millis(5)),
        }

        let measure = *lock(&hub.measure);
        let (seq, level, prefs) = self.exposure.update(hub, cam, &measure)?;
        let t = self.exposure.started.elapsed().as_secs_f64();
        self.focus.update(hub, cam, &measure, self.pulled, t)?;
        self.white.update(hub, cam, &measure, t)?;

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

    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn pass_other(&mut self) {
        let hub = self.hub;
        let Some(src) = self.other.as_mut() else {
            return;
        };
        let starting = !lock(&hub.status).live;
        let frame =
            if starting { src.start() } else { Ok(()) }.and_then(|()| src.frame(&mut self.jpeg));
        match frame {
            Ok(Some(format)) => {
                self.pulled += 1;
                hub.post(&mut self.jpeg, self.pulled, format);
                self.counted.1 += 1;
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                // Leaves the body out of live view and remote control, which a plain drop doesn't.
                if let Some(src) = self.other.take() {
                    src.close().ok();
                }
                hub.update(|s| {
                    s.camera = None;
                    s.live = false;
                    s.fps = 0.0;
                    s.problem = Some("The camera stopped sending pictures. Trying again.".into());
                });
                log(&format!("the camera stopped: {e}"));
                wait_for_camera(self.commands, REOPEN);
                return;
            }
        }
        if starting {
            hub.update(|s| s.live = true);
        }
        if self.counted.0.elapsed() >= Duration::from_secs(1) {
            let fps = self.counted.1 as f32 / self.counted.0.elapsed().as_secs_f32();
            self.counted = (Instant::now(), 0);
            hub.update(|s| s.fps = fps);
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn settle(&mut self, result: Result<(), canon::Error>, stalled: bool) {
        // Any error with the camera missing from WPD's list is a disconnect, not a reason to rest.
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
                s.problem = Some(ASLEEP.into());
            });
            return;
        }
        if let Err(e) = &result {
            if self.failing_since.is_none() {
                log(&format!("camera error: {e}"));
            }
            self.failing_since.get_or_insert_with(Instant::now);
        } else if self.failing_since.take().is_some() {
            self.hub.update(|s| s.problem = None);
        }
        let failing = self.failing_since.is_some_and(|t| t.elapsed() >= FAILING);
        if stalled || failing {
            if let Some(cam) = self.camera.take() {
                cam.close().ok();
            }
            self.rest_until = Some(Instant::now() + if stalled { REST } else { ERROR_REST });
            self.failing_since = None;
            let why = if stalled {
                "The camera stopped sending live view, probably from heat. Trying again in a minute."
            } else {
                "The camera keeps failing. Trying again in 20 seconds."
            }
            .to_owned();
            self.hub.update(|s| {
                s.problem = Some(why);
                s.live = false;
                s.fps = 0.0;
                s.resting = true;
            });
            return;
        }

        if let Err(e) = result {
            if self
                .failing_since
                .is_some_and(|t| t.elapsed() >= SHOW_AFTER)
            {
                self.hub.update(|s| s.problem = Some(plain(&e)));
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

impl SteadyState {
    #[expect(
        clippy::float_cmp,
        reason = "any change to the brightness setting at all starts metering again"
    )]
    fn update(
        &mut self,
        hub: &Hub,
        cam: &mut Camera,
        measure: &Measure,
    ) -> Result<(u64, f32, exposure::Prefs), canon::Error> {
        let prefs = *lock(&hub.exposure);
        let t = self.started.elapsed().as_secs_f64();
        let active = prefs.steady && cam.mode() == Some(MODE_M);
        if active != self.on || prefs.brightness != self.brightness {
            (self.on, self.brightness) = (active, prefs.brightness);
            self.steady.remeter(t);
            self.seen = measure.frame;
        }
        let (seq, level) = (measure.frame, measure.level);
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

#[derive(Default)]
struct FocusState {
    search: Option<Search>,
    watch: Watch,
    seen: u64,
    manual: bool,
    lens: Option<(Lens, u64)>,
    nudging: bool,
}

/// Where the lens is, in small steps nearer than its far end.
type Lens = f32;

const SWEEP: u32 = 25;
/// The whole travel in small steps, since the sweep from the near end takes `SWEEP` big ones.
const TRAVEL: Lens = SWEEP as f32 * 19.0;
/// Each `DriveLens` size, how far it moves the T3's kit lens in small steps (measured by
/// focusing the same face from the far end), and the wait after it, since a step sent while the
/// lens still moves can be lost. The body never says when the lens hits an end.
const SIZES: [(i8, f32, Duration); 3] = [
    (3, 19.0, Duration::from_millis(250)),
    (2, 2.1, Duration::from_millis(120)),
    (1, 1.0, Duration::from_millis(100)),
];

fn lens_step(step: i8) -> Option<(f32, Duration)> {
    SIZES
        .iter()
        .find(|s| s.0 == step.saturating_abs())
        .map(|s| (s.1, s.2))
}

/// Positive steps go farther, and a step past the far end goes nowhere.
fn record(lens: &mut Lens, step: i8) {
    if let Some((unit, _)) = lens_step(step) {
        *lens = f32::from(step.signum())
            .mul_add(-unit, *lens)
            .clamp(0.0, TRAVEL);
    }
}

/// The steps that take the lens from its far end to `at`, biggest first, as size and count.
fn steps_to(at: Lens) -> [(i8, u32); 3] {
    let mut left = if at.is_finite() {
        at.clamp(0.0, TRAVEL)
    } else {
        0.0
    };
    SIZES.map(|(size, unit, _)| {
        let n = if size == 1 {
            left.round()
        } else {
            (left / unit).floor()
        };
        left -= n * unit;
        (size, n as u32)
    })
}

fn lens_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join("viewfinder-focus-test.txt"));
    }
    crate::store::path("focus.txt")
}

fn load_lens() -> Option<Lens> {
    let text = std::fs::read_to_string(lens_path()?).ok()?;
    let mut words = text.split_whitespace();
    let at: Lens = words.next()?.parse().ok()?;
    (words.next().is_none() && at.is_finite()).then(|| at.clamp(0.0, TRAVEL))
}

fn save_lens(lens: Option<Lens>) {
    let Some(path) = lens_path() else { return };
    match lens {
        Some(at) => drop(crate::store::write_at(&path, &format!("{at:.1}\n"))),
        None => drop(std::fs::remove_file(path)),
    }
}

impl FocusState {
    fn drive(&mut self, cam: &mut Camera, step: i8) -> Result<(), canon::Error> {
        cam.drive_lens(step)?;
        if let Some((lens, _)) = self.lens.as_mut() {
            record(lens, step);
        }
        Ok(())
    }

    fn lost(&self, prefs: &exposure::Prefs, cam: &Camera) -> bool {
        prefs.remember_focus
            && cam.get(Prop::FocusMode) != Some(FOCUS_MF)
            && self
                .lens
                .is_none_or(|(_, since)| since != canon::arrivals())
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the lens is fine"
    )]
    fn restore(&mut self, hub: &Hub, cam: &mut Camera, pulled: u64) -> Result<(), canon::Error> {
        let saved = load_lens();
        log(&format!(
            "remember focus: putting the lens back at {saved:?}"
        ));
        for _ in 0..SWEEP {
            if let Err(e @ canon::Error::Disconnected) = cam.drive_lens(3) {
                return Err(e);
            }
            thread::sleep(lens_step(3).map_or(Duration::ZERO, |s| s.1));
        }
        let mut lens = 0.0;
        for (size, n) in steps_to(saved.unwrap_or_default()) {
            for _ in 0..n {
                cam.drive_lens(-size)?;
                record(&mut lens, -size);
                thread::sleep(lens_step(size).map_or(Duration::ZERO, |s| s.1));
            }
        }
        self.lens = Some((lens, canon::arrivals()));
        if saved.is_none() {
            save_lens(Some(lens));
        }
        // The lens doesn't step the same way twice, so the face finishes the job in small steps.
        if lock(&hub.exposure).autofocus {
            let kind = if saved.is_some() {
                focus::NUDGE
            } else {
                focus::SEARCH
            };
            self.start(hub, kind, pulled);
        }
        Ok(())
    }

    fn keep(&self) {
        if let Some((lens, _)) = self.lens {
            save_lens(Some(lens));
        }
    }

    fn forget(&mut self) {
        self.lens = None;
        save_lens(None);
    }

    fn start(&mut self, hub: &Hub, kind: (&'static [i8], u32), pulled: u64) {
        self.search = Some(Search::new(kind).after(pulled));
        self.nudging = kind == focus::NUDGE;
        hub.want_sharpness.store(true, Ordering::Relaxed);
        hub.update(|s| s.focus = focus::Report::Searching);
    }

    fn stop(&mut self, hub: &Hub, t: f64) {
        self.watch.rebase(t);
        if self.search.take().is_some() {
            hub.update(|s| s.focus = focus::Report::Idle);
        }
    }

    fn update(
        &mut self,
        hub: &Hub,
        cam: &mut Camera,
        measure: &Measure,
        pulled: u64,
        t: f64,
    ) -> Result<(), canon::Error> {
        let manual = cam.get(Prop::FocusMode) == Some(FOCUS_MF);
        if manual != self.manual {
            self.manual = manual;
            self.stop(hub, t);
        }
        let prefs = *lock(&hub.exposure);
        let holding = prefs.autofocus && prefs.hold_focus && !prefs.camera_af && !manual;
        hub.want_sharpness
            .store(holding || self.search.is_some(), Ordering::Relaxed);
        if manual || measure.frame == self.seen {
            return Ok(());
        }
        self.seen = measure.frame;
        let Some(search) = self.search.as_mut() else {
            if holding
                && self
                    .watch
                    .update(t, measure.sharpness.zip(measure.face_width))
            {
                log(
                    "always focus: the face blurred or moved nearer or farther, so nudging the lens",
                );
                self.start(hub, focus::NUDGE, pulled);
            }
            return Ok(());
        };
        let step = search.update(measure.frame, measure.sharpness, pulled);
        let done = search.done().then(|| search.report());
        if let Some(step) = step
            && let Err(e) = self.drive(cam, step)
        {
            self.stop(hub, t);
            return Err(e);
        }
        if let Some(report) = done {
            log(&format!("focus: {report:?}"));
            self.end(report, t);
            // Small steps can't see a face that's far out of focus, so the whole range is tried.
            if report == focus::Report::Flat && self.nudging {
                self.start(hub, focus::SEARCH, pulled);
            } else {
                hub.update(|s| s.focus = report);
            }
        }
        Ok(())
    }

    fn end(&mut self, report: focus::Report, t: f64) {
        if report == focus::Report::Focused {
            self.keep();
        }
        self.search = None;
        self.watch.rebase(t);
    }
}

#[derive(Default)]
struct WhiteState {
    steady: white::Steady,
    on: bool,
    seen: u64,
    told: f64,
    left: Option<white::Cast>,
}

impl WhiteState {
    fn update(
        &mut self,
        hub: &Hub,
        cam: &mut Camera,
        measure: &Measure,
        t: f64,
    ) -> Result<(), canon::Error> {
        let prefs = *lock(&hub.exposure);
        let active = prefs.white && cam.mode().is_some_and(canon::values::is_creative_mode);
        hub.want_cast.store(active, Ordering::Relaxed);
        // While it measures again the last correction stays, rather than flashing off.
        let left = self.steady.residual().or(self.left).filter(|_| active);
        if left != self.left {
            self.left = left;
            *lock(&hub.post_white) = left;
            if let Some(c) = left {
                log(&format!(
                    "steady white balance: taking out what the camera left, warm {:.2} and tint {:.2}",
                    c.warm, c.tint
                ));
            }
        }
        if active != self.on {
            self.on = active;
            self.steady.remeter(t);
            if active
                && let Some((wb, kelvin)) = last_white()
                && cam.get(Prop::WhiteBalance) != Some(wb)
                && cam.allowed(Prop::WhiteBalance).contains(&wb)
            {
                cam.hold_white_balance(wb, kelvin)?;
                publish_settings(hub, cam);
            }
            if !active {
                cam.restore_white_balance()?;
                hub.update(|s| (s.white_held, s.light, s.white_unseen) = (None, None, false));
                publish_settings(hub, cam);
            }
        }
        if !active || measure.frame == self.seen {
            return Ok(());
        }
        self.seen = measure.frame;
        let Some(wb) = cam.get(Prop::WhiteBalance) else {
            return Ok(());
        };
        let settings = white::Settings {
            wb,
            kelvin: cam.get(Prop::ColorTemp),
            wbs: cam.allowed(Prop::WhiteBalance),
            kelvins: cam.allowed(Prop::ColorTemp),
        };
        if let Some(c) = self.steady.update(t, measure.cast, &settings) {
            log(&format!(
                "steady white balance: light {:?} K, tint {:?}, setting {} {:?}",
                self.steady.light().map(f32::round),
                self.steady.tint().map(|v| (v * 100.0).round() / 100.0),
                canon::values::white_balance_label(c.wb),
                c.kelvin
            ));
            cam.hold_white_balance(c.wb, c.kelvin)?;
            save_white(c.wb, c.kelvin);
            publish_settings(hub, cam);
        }
        if t - self.told >= 1.0 {
            self.told = t;
            let held = white::held(cam.get(Prop::WhiteBalance), cam.get(Prop::ColorTemp));
            let light = self
                .steady
                .light()
                .map(|k| ((k / 50.0).round() * 50.0) as u32);
            let unseen = self.steady.unseen();
            hub.update(|s| (s.white_held, s.light, s.white_unseen) = (held, light, unseen));
        }
        Ok(())
    }
}

fn last_white() -> Option<(u32, Option<u32>)> {
    let text = crate::store::read("white-setting.txt")?;
    let mut v = text.split_whitespace().map(|n| n.parse::<u32>().ok());
    let wb = v.next()??;
    Some((wb, v.next().flatten().filter(|&k| k > 0)))
}

fn save_white(wb: u32, kelvin: Option<u32>) {
    crate::store::write(
        "white-setting.txt",
        &format!("{wb} {}\n", kelvin.unwrap_or(0)),
    );
}

#[expect(
    clippy::disallowed_methods,
    reason = "runs on the camera thread, where waiting for the body is fine"
)]
fn wait_for_camera(commands: &Receiver<Command>, wait: Duration) {
    let until = Instant::now() + wait;
    let seen = capture::changes();
    while Instant::now() < until {
        if canon::arrived() || capture::changes() != seen {
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

fn resume_asked(commands: &Receiver<Command>, wait: Duration) -> bool {
    let mut resume = match commands.recv_timeout(wait) {
        Ok(cmd) => matches!(cmd, Command::Retry),
        Err(RecvTimeoutError::Timeout) => false,
        Err(RecvTimeoutError::Disconnected) => {
            drain(commands, wait);
            return false;
        }
    };
    while let Ok(cmd) = commands.try_recv() {
        resume |= matches!(cmd, Command::Retry);
    }
    resume
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

const SHOWN: [Prop; 8] = [
    Prop::Iso,
    Prop::Shutter,
    Prop::Aperture,
    Prop::ExposureComp,
    Prop::WhiteBalance,
    Prop::ColorTemp,
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
    let vfx = crate::setup::vfx_dir();
    hub.update(|s| s.vsr_available = vfx.is_some());
    let mut pipeline = PipelineThread {
        hub,
        pipeline: Pipeline::with_gpu(vfx),
        jpeg: Vec::new(),
        format: capture::Format::Jpeg,
        shared: None,
        last_open: None,
        open_failed: false,
        out: Output::default(),
        pacer: Pacer::default(),
        tally: Tally::new(),
        seq: 0,
        pacing: false,
        watching: false,
        last_watched: Instant::now(),
        builds: 0,
        trim_at: None,
        greys: white::Greys::default(),
        faces: face::Finder::default(),
        steer: None,
        skin: white::Skin::new(white::load_shown()),
        room: None,
        auto_asked: None,
        shown: white::load_shown(),
        shown_saved: Instant::now(),
        card: None,
    };
    loop {
        pipeline.step();
    }
}

struct PipelineThread<'a> {
    hub: &'a Hub,
    pipeline: Pipeline,
    jpeg: Vec<u8>,
    format: capture::Format,
    shared: Option<Shared>,
    last_open: Option<Instant>,
    open_failed: bool,
    out: Output,
    pacer: Pacer,
    tally: Tally,
    seq: u64,
    pacing: bool,
    watching: bool,
    last_watched: Instant,
    builds: u32,
    trim_at: Option<Instant>,
    greys: white::Greys,
    faces: face::Finder,
    steer: Option<(Instant, autoframe::Steer)>,
    skin: white::Skin,
    room: Option<white::Cast>,
    auto_asked: Option<Instant>,
    shown: Option<white::Cast>,
    shown_saved: Instant,
    card: Option<(Format, Vec<u8>)>,
}

impl PipelineThread<'_> {
    fn step(&mut self) {
        self.open_shared();
        let wanted = self
            .shared
            .as_ref()
            .and_then(|s| s.wanted(CONSUMER_IDLE_MS));
        let active = wanted.is_some();
        self.hub.consumer_active.store(active, Ordering::Relaxed);
        if lock(&self.hub.status).consumer != wanted {
            self.hub.update(|s| s.consumer = wanted);
        }
        let look = self.auto_frame(*lock(&self.hub.look));
        self.manage_gpu(wanted, look);
        if self.hub.hidden() {
            self.hide(wanted);
            return;
        }
        let pace = active && self.pipeline.can_smooth(look);
        if pace != self.pacing {
            self.pacing = pace;
            self.pipeline.forget();
            self.pacer = Pacer::default();
            self.tally = Tally::new();
            self.hub.update(|s| s.smoothing = pace);
        }

        // Condvar waits wake on the 15.6 ms system tick, which made 4 in 10 frames miss the
        // DLL's read, so the last stretch is slept on the high-resolution timer instead.
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
        if let Some(frame) = take_mail(self.hub, wait, &mut self.jpeg, &mut self.format)
            && !self.decode(look, wanted, frame)
        {
            return;
        }
        if close {
            self.serve(wanted);
        }
    }

    fn auto_frame(&mut self, mut look: Look) -> Look {
        if !look.auto_frame {
            self.steer = None;
            return look;
        }
        look.zoom = look.zoom.max(autoframe::MIN_ZOOM);
        let (pixels, width, _) = self.pipeline.frame();
        let size = (
            width,
            pixels.len().checked_div(width * 3).unwrap_or_default(),
        );
        let own = (look.pan, look.framing);
        let (then, steer) = self
            .steer
            .unwrap_or_else(|| (Instant::now(), autoframe::Steer::new(own)));
        let crop = crop_size(size, look);
        let dt = then.elapsed().as_secs_f32();
        let steer = steer.next(self.faces.face(), size, crop, own, dt);
        self.steer = Some((Instant::now(), steer));
        (look.pan, look.framing) = steer.at();
        look
    }

    fn hide(&mut self, wanted: Option<Format>) {
        if self.pacing {
            self.pacing = false;
            self.hub.update(|s| s.smoothing = false);
        }
        take_mail(self.hub, HIDDEN_EVERY, &mut self.jpeg, &mut self.format);
        let (Some(shared), Some(f)) = (self.shared.as_ref(), wanted) else {
            return;
        };
        if self.card.as_ref().is_none_or(|(at, _)| *at != f) {
            self.card = Some((f, crate::ui::hidden_card(f)));
        }
        if let Some((_, card)) = &self.card {
            self.out.frame_no += 1;
            shared.write(f, self.out.frame_no, |buf| {
                buf.copy_from_slice(card);
                true
            });
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

    fn manage_gpu(&mut self, wanted: Option<Format>, look: Look) {
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

    /// False if the frame was broken.
    fn decode(&mut self, look: Look, wanted: Option<Format>, frame: u64) -> bool {
        let started = Instant::now();
        self.pipeline.avoid_eyes(self.faces.face());
        if let Err(e) = self.pipeline.decode_frame(&self.jpeg, self.format, look) {
            self.out.raise(self.hub, DECODING, e);
            return false;
        }
        self.out.settle(self.hub, DECODING);
        let (pixels, width, rgb) = self.pipeline.frame();
        let want_cast = self.hub.want_cast.load(Ordering::Relaxed);
        let want_sharpness = self.hub.want_sharpness.load(Ordering::Relaxed);
        // In mixed light the face is what has to look right, so its skin goes before the room.
        if want_cast || want_sharpness || look.auto_frame || look.fix_pixels {
            self.faces.look(pixels, width, rgb);
        }
        let skin = if want_cast {
            self.faces
                .face()
                .and_then(|f| self.greys.skin(pixels, width, rgb, f))
        } else {
            None
        };
        let face = self.faces.face().filter(|_| want_sharpness);
        let measure = Measure {
            frame,
            level: exposure::brightness(pixels, width, rgb),
            sharpness: focus::sharpness(pixels, width, rgb, face),
            face_width: face.map(|f| f.w as f32),
            // The room's greys alone, so a face moving around can't look like the light changing.
            cast: if want_cast {
                self.greys.cast(pixels, width, rgb)
            } else {
                None
            },
        };
        *lock(&self.hub.measure) = measure;
        let balance = self.white(want_cast, skin);
        self.auto(balance);
        let adjust = *lock(&self.hub.adjust);
        let keep = self.hub.compare.load(Ordering::Relaxed)
            || self.hub.want_original.load(Ordering::Relaxed);
        self.pipeline.finish(balance, &adjust, keep);
        // Sent as each camera frame is decoded, since paced output repeats some.
        if self.hub.preview_wanted.load(Ordering::Relaxed)
            && (self.hub.compare.load(Ordering::Relaxed)
                || self.hub.want_original.swap(false, Ordering::Relaxed))
        {
            self.out.send_still(self.hub, &self.pipeline);
        }
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

    fn white(&mut self, want_cast: bool, skin: Option<white::Cast>) -> Option<white::Cast> {
        let held = *lock(&self.hub.post_white);
        let post_white = held.or_else(|| self.shown.filter(|_| want_cast));
        if held != self.room {
            self.skin.reset(self.room, held);
            self.room = held;
        }
        let room = post_white?;
        let deciding = self.skin.deciding();
        let face = self.skin.correction(skin);
        if deciding && let Some(c) = face {
            log(&format!(
                "white balance: set by the face, taking out warm {:.2} and tint {:.2}",
                c.warm, c.tint
            ));
        }
        let target = face
            .or_else(|| self.shown.filter(|_| self.skin.deciding()))
            .unwrap_or(room);
        let shown = self.shown.map_or(target, |s| s.toward(target, EASE));
        self.shown = Some(shown);
        if self.shown_saved.elapsed() >= SAVE_SHOWN {
            self.shown_saved = Instant::now();
            white::save_shown(shown);
        }
        let mut cast = shown;
        cast.warm -= lock(&self.hub.exposure).warmth - COOLER;
        Some(cast)
    }

    fn auto(&mut self, cast: Option<white::Cast>) {
        if self.hub.want_auto.swap(false, Ordering::Relaxed) {
            self.auto_asked = Some(Instant::now());
        }
        let Some(asked) = self.auto_asked else {
            return;
        };
        let (pixels, width, rgb) = self.pipeline.frame();
        self.faces.look(pixels, width, rgb);
        let face = self.faces.face();
        if face.is_none() && asked.elapsed() < FACE_WAIT {
            return;
        }
        self.auto_asked = None;
        let Some(picked) = self.pipeline.auto(cast, face) else {
            return;
        };
        let mut adjust = lock(&self.hub.adjust);
        *adjust = crate::adjust::Adjust {
            clarity: adjust.clarity,
            fade: adjust.fade,
            vignette: adjust.vignette,
            ..picked
        };
        log(&format!(
            "auto adjust, face {}: {:?}",
            face.is_some(),
            *adjust
        ));
        drop(adjust);
        self.hub.repaint();
    }

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
            self.tally
                .count(at, self.pacer.newest, self.pacer.missed(), started);
            self.pacer.served = self.pacer.target;
        }
    }
}

fn take_mail(
    hub: &Hub,
    wait: Duration,
    jpeg: &mut Vec<u8>,
    format: &mut capture::Format,
) -> Option<u64> {
    let (mut mailbox, _) = hub
        .mail
        .wait_timeout_while(lock(&hub.mailbox), wait, |m| !m.1)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let got = mailbox.1.then_some(mailbox.2);
    if got.is_some() {
        std::mem::swap(&mut mailbox.0, jpeg);
        mailbox.1 = false;
        *format = mailbox.3;
    }
    drop(mailbox);
    got
}

/// The DLL counts a feed quiet for 250 ms as stopped.
const HIDDEN_EVERY: Duration = Duration::from_millis(100);

/// The GPU passes hold 460 MB of video memory and take 1.1 s to build again.
const RELEASE_AFTER: Duration = Duration::from_secs(60);

/// Building the GPU passes touches about 210 MB that frames never touch again. Trimmed after the
/// first frames, a call's working set settled at 52 MB rather than 268 at the same frame times.
const TRIM_AFTER_BUILD: Duration = Duration::from_secs(3);

#[expect(unsafe_code, reason = "trimming the working set is a Win32 call")]
fn trim() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    // SAFETY: the pseudo handle for this process, and MAX for both sizes means trim what can be.
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX).ok() };
}

#[derive(Default)]
struct Output {
    frame_no: u64,
    rgba: Vec<u8>,
    local: Vec<u8>,
    frame_ms: f32,
    stage_ms: [f32; 4],
    still: Vec<u8>,
    published: Option<Instant>,
    /// The problem a broken frame put up while decoding or rendering, taken down by the next
    /// frame that gets through that same step.
    raised: [Option<String>; 2],
}

const DECODING: usize = 0;
const RENDERING: usize = 1;

impl Output {
    fn raise(&mut self, hub: &Hub, step: usize, problem: String) {
        if let Some(slot) = self.raised.get_mut(step) {
            *slot = Some(problem.clone());
        }
        hub.update(|s| s.problem = Some(problem));
    }

    fn settle(&mut self, hub: &Hub, step: usize) {
        if let Some(old) = self.raised.get_mut(step).and_then(Option::take) {
            hub.update(|s| {
                if s.problem.as_ref() == Some(&old) {
                    s.problem = None;
                }
            });
        }
    }

    fn send_still(&mut self, hub: &Hub, pipeline: &Pipeline) {
        let mut still = std::mem::take(&mut self.still);
        let size = pipeline.preview(&mut still);
        let mut p = lock(&hub.preview);
        std::mem::swap(&mut p.still, &mut still);
        (p.still_size, p.still_fresh) = (size, true);
        drop(p);
        self.still = still;
    }

    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn time_stages(&mut self, pipeline: &mut Pipeline, begun: Instant, gpu_before: f32) {
        pipeline.times[OUTPUT] +=
            (begun.elapsed().as_secs_f32() * 1000.0 - (pipeline.times[GPU] - gpu_before)).max(0.0);
        let times = std::mem::take(&mut pipeline.times);
        for (avg, t) in self.stage_ms.iter_mut().zip(times) {
            *avg = if *avg == 0.0 { t } else { *avg * 0.9 + t * 0.1 };
        }
    }

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
        let begun = Instant::now();
        let gpu_before = pipeline.times[GPU];
        let mut previewed = None;
        let gpu_was_fine = pipeline.gpu_problem().is_none();
        if let (Some(s), Some(f)) = (shared, wanted) {
            self.frame_no += 1;
            let mut failed = None;
            let rgba = &mut self.rgba;
            s.write(f, self.frame_no, |buf| match pipeline.render(f, buf) {
                Ok(()) => {
                    if preview {
                        previewed = Some(preview_output(f, buf, rgba));
                    }
                    true
                }
                Err(e) => {
                    failed = Some(e);
                    false
                }
            });
            // A render that broke the GPU just now is the GPU's fault, reported below.
            match failed {
                Some(_) if gpu_was_fine && pipeline.gpu_problem().is_some() => {}
                Some(e) => self.raise(hub, RENDERING, e),
                None => self.settle(hub, RENDERING),
            }
        } else {
            // Nothing is going to apps, so a problem sending to them is over.
            self.settle(hub, RENDERING);
        }
        if preview && previewed.is_none() {
            self.local.resize(PREVIEW.bytes(), 0);
            if pipeline.render(PREVIEW, &mut self.local).is_ok() {
                previewed = Some(preview_output(PREVIEW, &self.local, &mut self.rgba));
            }
        }
        // Either render can break the GPU, the preview's too when zoomed.
        if gpu_was_fine && let Some(g) = pipeline.gpu_problem().map(str::to_owned) {
            hub.update(|s| s.vsr_problem = Some(g));
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
            (p.width, p.height, p.fresh, p.histogram) = (w, h, true, histogram);
            drop(p);
            hub.repaint();
        }
    }
}

/// Plays camera frames out at an even 30 fps. The T3's frames land 16 to 70 ms apart, so
/// reading the newest every 33 ms showed apps only 21 to 24 different frames a second. The
/// pacer runs about two frames behind and moves through them at the camera's average rate.
#[derive(Default)]
struct Pacer {
    arrivals: VecDeque<Instant>,
    newest: Option<u64>,
    /// In frame numbers, 4.25 being a quarter of the way from frame 4 to 5.
    at: f64,
    target: i64,
    served: i64,
    lead: i64,
}

/// Frames the pacer stays behind the newest. In a replay of recorded arrival times, 2 behind
/// repeated 1.5% of frames and 2.5 none, for about 60 and 70 ms of delay.
const BUFFER: f64 = 2.2;
/// In a model of the T3's polling, 0.2 with the rate over 8 arrivals spaced the moments shown
/// 3.8 to 4.4 ms apart in standard deviation, and 0.1 over 32 arrivals 1.6 to 1.7 ms, with the
/// same delay and no repeats.
const GAIN: f64 = 0.1;
const RATE_OVER: usize = 32;
const INTERVAL_100NS: i64 = 333_333;
const COARSE_WAKE: Duration = Duration::from_millis(20);

impl Pacer {
    fn arrived(&mut self, seq: u64, at: Instant) {
        self.arrivals.push_back(at);
        if self.arrivals.len() > RATE_OVER {
            self.arrivals.pop_front();
        }
        let first = self.newest.is_none();
        self.newest = Some(seq);
        if first || seq as f64 - self.at > 8.0 {
            self.at = seq as f64 - BUFFER;
        }
    }

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

    #[expect(unsafe_code, reason = "MFGetSystemTime is a Media Foundation call")]
    fn missed(&self) -> bool {
        // SAFETY: takes nothing.
        unsafe { MFGetSystemTime() > self.target }
    }

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

struct Tally {
    since: Instant,
    served: u32,
    /// Finished after the DLL's read, which then sent the previous frame again.
    late: u32,
    /// Shown at the same moment as the frame before, having caught up with the camera.
    caught_up: u32,
    last_at: f64,
    render: [u32; 80],
}

impl Tally {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            served: 0,
            late: 0,
            caught_up: 0,
            last_at: -1.0,
            render: [0; 80],
        }
    }

    fn count(&mut self, at: f64, newest: Option<u64>, late: bool, started: Instant) {
        let slowest = self.render.len() - 1;
        let bin = ((started.elapsed().as_secs_f64() * 2000.0) as usize).min(slowest);
        if let Some(n) = self.render.get_mut(bin) {
            *n += 1;
        }
        self.served += 1;
        if late {
            self.late += 1;
        } else if (at - self.last_at).abs() < 1e-9 && newest.is_some_and(|n| at >= n as f64) {
            self.caught_up += 1;
        }
        self.last_at = at;
        if self.since.elapsed() >= Duration::from_secs(60) {
            log(&format!(
                "last minute: {} frames, {} distinct, {} late for their read, {} caught up with the camera, render p99 {:.1} ms",
                self.served,
                self.served - self.late - self.caught_up,
                self.late,
                self.caught_up,
                self.p99_ms()
            ));
            *self = Self::new();
        }
    }

    fn p99_ms(&self) -> f32 {
        let want = u64::from(self.served) * 99;
        let mut seen = 0u64;
        for (i, n) in self.render.iter().enumerate() {
            seen += u64::from(*n) * 100;
            if seen >= want {
                return (i + 1) as f32 * 0.5;
            }
        }
        self.render.len() as f32 * 0.5
    }
}

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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{FocusState, Tally, load_lens, record, save_lens, steps_to};
    use crate::focus::{self, Search};

    #[test]
    fn focusing_on_a_face_saves_the_spot() {
        let mut state = FocusState {
            lens: Some((42.0, 0)),
            search: Some(Search::new(focus::NUDGE)),
            ..FocusState::default()
        };
        state.end(focus::Report::Focused, 0.0);
        assert_eq!(load_lens(), Some(42.0));
        state.lens = Some((5.0, 0));
        for report in [focus::Report::NoFace, focus::Report::Flat] {
            state.search = Some(Search::new(focus::SEARCH));
            state.end(report, 0.0);
            assert_eq!(load_lens(), Some(42.0));
        }
        save_lens(None);
    }

    #[test]
    fn steps_past_the_far_end_are_not_counted() {
        let mut lens = 0.0;
        record(&mut lens, 3);
        assert!(lens.abs() < 1e-6, "from the far end, farther goes nowhere");
        record(&mut lens, -3);
        record(&mut lens, 2);
        record(&mut lens, -1);
        assert!((lens - 17.9).abs() < 1e-4, "{lens}");
    }

    #[test]
    fn the_way_back_adds_up_to_the_spot() {
        assert_eq!(steps_to(58.0), [(3, 3), (2, 0), (1, 1)]);
        assert_eq!(steps_to(0.0), [(3, 0), (2, 0), (1, 0)]);
        let mut lens = 0.0;
        for (size, n) in steps_to(73.4) {
            for _ in 0..n {
                record(&mut lens, -size);
            }
        }
        assert!((lens - 73.4).abs() <= 0.5, "{lens}");
    }

    #[test]
    fn tally_counts_repeats_by_cause_and_the_render_tail() {
        let mut tally = Tally::new();
        let started = Instant::now();
        tally.count(2.0, Some(2), false, started);
        tally.count(2.0, Some(2), false, started);
        tally.count(2.9, Some(4), true, started);
        tally.count(3.8, Some(4), false, started);
        assert_eq!((tally.served, tally.late, tally.caught_up), (4, 1, 1));

        let mut tally = Tally::new();
        tally.served = 100;
        tally.render[26] = 99;
        tally.render[40] = 1;
        assert!((tally.p99_ms() - 13.5).abs() < 1e-6);
        tally.render[26] = 98;
        tally.render[40] = 2;
        assert!((tally.p99_ms() - 20.5).abs() < 1e-6);
    }
}
