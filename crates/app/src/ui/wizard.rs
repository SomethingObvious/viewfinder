//! Setup: each step checks the real state, says what's wrong in plain words, and offers a
//! button where one can fix it.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use canon::values;
use eframe::egui::{
    self, Align2, Context, Pos2, Rect, Sense, Stroke, Ui, UiBuilder, Vec2, ViewportCommand,
};

use super::{
    App, DIM, FAINT, GPU, INK, LABEL, MAT, REPAIR, ROW_RULE, RULE, UNKNOWN, WARN, finish, note,
    primary, readout, section,
};
use crate::setup::{self, Download, Holder};
use crate::worker::{REFUSED, Status, lock};

const DRIVERS: &str = "https://www.nvidia.com/en-us/drivers/";
const MARKER: &str = "setup.txt";
/// Joins a size to its unit, so a wrapped note never leaves MB alone on the next line.
const NBSP: char = '\u{a0}';

pub(super) struct Wizard {
    pub(super) open: bool,
    pub(super) card: Option<String>,
    pub(super) script: Option<PathBuf>,
    holders: Vec<Holder>,
    scanned: f64,
    scanning: Option<Receiver<Vec<Holder>>>,
    download: Arc<Mutex<Download>>,
    asking: Option<Receiver<Result<(), String>>>,
    refused: Option<String>,
    /// When Windows said yes. install.ps1 quits this copy within seconds, so still being here
    /// well after means it failed.
    installing: Option<f64>,
    /// Whether the registered virtual camera is the one beside this exe.
    pub(super) here: bool,
    quit: Arc<Mutex<Quit>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Quit {
    Idle,
    Ending(u32),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Ready,
    ToDo,
    Waiting,
    NotNeeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fix {
    Install,
    Quit(u32, &'static str),
    Download,
    Retry,
    Driver,
    Restart,
}

#[derive(Debug)]
struct Step {
    name: &'static str,
    mark: Mark,
    text: String,
    fix: Option<Fix>,
    progress: Option<f32>,
}

const fn step(name: &'static str, mark: Mark, text: String, fix: Option<Fix>) -> Step {
    Step {
        name,
        mark,
        text,
        fix,
        progress: None,
    }
}

impl Mark {
    const fn word(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::ToDo => "To Do",
            Self::Waiting => "Waiting",
            Self::NotNeeded => "Not Needed",
        }
    }
}

impl Fix {
    fn label(self) -> String {
        match self {
            Self::Install => "Install".into(),
            Self::Quit(_, name) => format!("Quit {name}"),
            Self::Download => "Download".into(),
            Self::Retry => "Try Again".into(),
            Self::Driver => "Update Driver".into(),
            Self::Restart => "Restart Viewfinder".into(),
        }
    }
}

impl Wizard {
    /// The demo's made-up machine, which has an RTX card and the installer beside it.
    pub(super) fn new() -> Self {
        Self {
            open: false,
            card: Some("NVIDIA GeForce RTX 4060 Laptop GPU".into()),
            script: Some("install.ps1".into()),
            holders: Vec::new(),
            scanned: f64::NEG_INFINITY,
            scanning: None,
            download: Arc::new(Mutex::new(Download::Idle)),
            asking: None,
            refused: None,
            installing: None,
            here: true,
            quit: Arc::new(Mutex::new(Quit::Idle)),
        }
    }

    /// Opens by itself until Done is pressed once.
    pub(super) fn first_run() -> bool {
        crate::store::read(MARKER).is_none()
    }

    pub(super) fn rtx(&self) -> bool {
        self.card.as_deref().is_some_and(|c| c.contains("RTX"))
    }

    fn steps(&self, status: &Status) -> [Step; 5] {
        [
            self.install_step(status),
            camera_step(status),
            self.holder_step(status),
            dial_step(status),
            self.effects_step(status),
        ]
    }

    fn install_step(&self, status: &Status) -> Step {
        const NAME: &str = "Virtual Camera";
        if status.vcam.is_none() && self.here {
            return step(
                NAME,
                Mark::Ready,
                "Call apps can pick Viewfinder from their camera list.".into(),
                None,
            );
        }
        if self.script.is_none() {
            return step(
                NAME,
                Mark::ToDo,
                "Call apps can't see Viewfinder until it's installed, and install.ps1 isn't next to viewfinder.exe. Unzip the whole download and start Viewfinder from that folder.".into(),
                None,
            );
        }
        if self.asking.is_some() {
            return step(
                NAME,
                Mark::Waiting,
                "Windows is asking for permission.".into(),
                None,
            );
        }
        if self.installing.is_some() {
            return step(
                NAME,
                Mark::Waiting,
                "Installing. Viewfinder closes while it runs.".into(),
                None,
            );
        }
        let mut text = "Call apps can't see Viewfinder until it's installed. Install copies it to Program Files and adds its camera to Windows, which asks for permission first. Viewfinder closes while it runs and opens again after.".to_owned();
        if let Some(refused) = &self.refused {
            text = format!("{refused} {text}");
        }
        step(NAME, Mark::ToDo, text, Some(Fix::Install))
    }

    fn holder_step(&self, status: &Status) -> Step {
        const NAME: &str = "Other Programs";
        let ready = || {
            step(
                NAME,
                Mark::Ready,
                "No other program is holding the camera.".into(),
                None,
            )
        };
        // Canon's background helpers can run without holding anything once Viewfinder has it.
        if status.camera.is_some() {
            return ready();
        }
        if let Some(&(pid, name)) = self.holders.first() {
            let text =
                format!("{name} is open, and only one program can use the camera at a time.");
            let quit = lock(&self.quit).clone();
            return match quit {
                Quit::Ending(ending) if ending == pid => {
                    step(NAME, Mark::Waiting, format!("Closing {name}."), None)
                }
                Quit::Failed(failed) => step(
                    NAME,
                    Mark::ToDo,
                    format!("{failed} {text}"),
                    Some(Fix::Quit(pid, name)),
                ),
                _ => step(NAME, Mark::ToDo, text, Some(Fix::Quit(pid, name))),
            };
        }
        if status.camera.is_none() && status.problem.as_deref() == Some(REFUSED) {
            return step(
                NAME,
                Mark::ToDo,
                "The camera is plugged in but another program has it. Quit any other camera app, then unplug the camera and plug it back in.".into(),
                None,
            );
        }
        ready()
    }

    fn effects_step(&self, status: &Status) -> Step {
        const NAME: &str = "NVIDIA Video Effects";
        let Some(card) = &self.card else {
            return step(
                NAME,
                Mark::NotNeeded,
                "There's no NVIDIA graphics card, so Viewfinder sharpens on the CPU instead. Everything else works the same.".into(),
                None,
            );
        };
        if !self.rtx() {
            return step(
                NAME,
                Mark::NotNeeded,
                format!(
                    "NVIDIA's effects need an RTX card and the {card} isn't one, so Viewfinder sharpens on the CPU instead."
                ),
                None,
            );
        }
        if let Some(problem) = &status.vsr_problem {
            return step(NAME, Mark::ToDo, problem.clone(), Some(Fix::Driver));
        }
        if status.vsr_available {
            return step(NAME, Mark::Ready, format!("Running on the {card}."), None);
        }
        let mb = |bytes: u64| format!("{:.0}", bytes as f64 / 1e6);
        let download = lock(&self.download).clone();
        match download {
            Download::Idle => step(
                NAME,
                Mark::ToDo,
                format!(
                    "Noise reduction and super resolution need NVIDIA's Video Effects, a {}{NBSP}MB download from NVIDIA.",
                    mb(setup::WHEEL_BYTES)
                ),
                Some(Fix::Download),
            ),
            Download::Fetching(got) => Step {
                progress: Some(got as f32 / setup::WHEEL_BYTES as f32),
                ..step(
                    NAME,
                    Mark::Waiting,
                    format!(
                        "Downloading from NVIDIA, {} of {}{NBSP}MB.",
                        mb(got),
                        mb(setup::WHEEL_BYTES)
                    ),
                    None,
                )
            },
            Download::Unpacking => step(
                NAME,
                Mark::Waiting,
                "Checking the download and unpacking it.".into(),
                None,
            ),
            Download::Done => step(
                NAME,
                Mark::ToDo,
                "Downloaded. Restart Viewfinder to turn on noise reduction and super resolution."
                    .into(),
                Some(Fix::Restart),
            ),
            Download::Failed(e) => step(NAME, Mark::ToDo, e, Some(Fix::Retry)),
        }
    }

    fn give_up_installing(&mut self, now: f64) {
        if self.installing.is_some_and(|t| now - t > 30.0) {
            self.installing = None;
            self.refused =
                Some("The installer stopped before it finished. Its window says why.".into());
        }
    }

    /// Picks up the installer's answer and keeps the program scan fresh.
    fn poll(&mut self, ctx: &Context, real: bool) {
        if let Some(rx) = &self.asking {
            match rx.try_recv() {
                Ok(answer) => {
                    self.installing = answer.is_ok().then(|| ctx.input(|i| i.time));
                    (self.refused, self.asking) = (answer.err(), None);
                }
                Err(TryRecvError::Disconnected) => self.asking = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.installing.is_some() {
            self.give_up_installing(ctx.input(|i| i.time));
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if !real {
            return;
        }
        if let Some(rx) = &self.scanning {
            match rx.try_recv() {
                Ok(found) => {
                    let mut quit = lock(&self.quit);
                    if found != self.holders && matches!(*quit, Quit::Failed(_)) {
                        *quit = Quit::Idle;
                    }
                    drop(quit);
                    (self.holders, self.scanning) = (found, None);
                }
                Err(TryRecvError::Disconnected) => self.scanning = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        let now = ctx.input(|i| i.time);
        if now - self.scanned < 2.0 {
            return;
        }
        self.scanned = now;
        let (tx, rx) = channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            tx.send(setup::holders()).ok();
            c.request_repaint();
        });
        self.scanning = Some(rx);
        ctx.request_repaint_after(Duration::from_secs(2));
    }
}

fn camera_step(status: &Status) -> Step {
    const NAME: &str = "Camera";
    match (&status.camera, &status.problem) {
        (Some(model), _) => step(NAME, Mark::Ready, format!("{model} is connected."), None),
        (None, Some(problem)) => step(NAME, Mark::ToDo, problem.clone(), None),
        (None, None) => step(
            NAME,
            Mark::Waiting,
            "Plug the camera in over USB and turn it on. Viewfinder finds it by itself.".into(),
            None,
        ),
    }
}

fn dial_step(status: &Status) -> Step {
    const NAME: &str = "Mode Dial";
    match (&status.camera, status.mode) {
        (None, _) => step(
            NAME,
            Mark::Waiting,
            "Checked once the camera is connected.".into(),
            None,
        ),
        (Some(_), None) => step(
            NAME,
            Mark::NotNeeded,
            "This camera doesn't report a mode dial, so there's nothing to set.".into(),
            None,
        ),
        (Some(_), Some(m)) if values::is_creative_mode(m) => step(
            NAME,
            Mark::Ready,
            format!(
                "On {}, so Viewfinder can set exposure and white balance.",
                values::ae_mode_label(m)
            ),
            None,
        ),
        (Some(_), Some(m)) => step(
            NAME,
            Mark::ToDo,
            format!(
                "The dial is on {}. Turn it to P, Tv, Av or M so Viewfinder can set exposure and white balance.",
                values::ae_mode_label(m)
            ),
            None,
        ),
    }
}

impl App {
    pub(super) fn setup_sheet(&mut self, ui: &mut Ui, area: Rect, status: &Status) {
        self.setup.poll(ui.ctx(), self.persist);
        let steps = self.setup.steps(status);
        // Keeps the notes near 80 characters a line.
        let width = (area.width() - 64.0).min(440.0);
        let column = Rect::from_min_size(
            Pos2::new((area.center().x - width / 2.0).round(), area.top()),
            Vec2::new(width, area.height()),
        );
        let mut sheet = ui.new_child(
            UiBuilder::new()
                .id_salt("Setup")
                .max_rect(column)
                .accessibility_label("Setup"),
        );
        sheet.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        let mut fix = None;
        let mut done = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(&mut sheet, |ui| {
            // Keeps the marks on the right clear of the scroll bar.
            ui.set_max_width(ui.available_width() - 16.0);
            section(ui, "Setup");
            note(
                ui,
                "Each step checks itself again as you go. Setup stays in General if you need it later.",
                FAINT,
            );
            ui.add_space(4.0);
            for s in &steps {
                fix = draw_step(ui, s).or(fix);
            }
            ui.add_space(18.0);
            done = primary(ui, "Done").clicked();
            ui.add_space(18.0);
        });
        if let Some(fix) = fix {
            self.act(fix, ui.ctx());
        }
        if done {
            self.setup.open = false;
            if self.persist {
                crate::store::write(MARKER, "done=1\n");
            }
        }
    }

    /// The demo and tests never touch the real machine.
    fn act(&mut self, fix: Fix, ctx: &Context) {
        if !self.persist {
            return;
        }
        let repaint = {
            let c = ctx.clone();
            move || c.request_repaint()
        };
        match fix {
            Fix::Install => {
                let Some(script) = self.setup.script.clone() else {
                    return;
                };
                let (tx, rx) = channel();
                std::thread::spawn(move || {
                    tx.send(setup::install(&script)).ok();
                    repaint();
                });
                (self.setup.asking, self.setup.refused) = (Some(rx), None);
            }
            Fix::Quit(pid, name) => {
                let quit = self.setup.quit.clone();
                *lock(&quit) = Quit::Ending(pid);
                std::thread::spawn(move || {
                    *lock(&quit) = setup::end(pid, name).map_or_else(Quit::Failed, |()| Quit::Idle);
                    repaint();
                });
                self.setup.scanned = f64::NEG_INFINITY;
            }
            Fix::Download | Fix::Retry => setup::download(self.setup.download.clone(), repaint),
            Fix::Driver => setup::open(DRIVERS),
            Fix::Restart => {
                if setup::restart() {
                    self.quitting.store(true, Ordering::Relaxed);
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }
}

/// The name and its mark on one line, then what to do, then the button that does it.
fn draw_step(ui: &mut Ui, s: &Step) -> Option<Fix> {
    ui.add_space(14.0);
    let (line, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
    readout(&r, || format!("{}: {}. {}", s.name, s.mark.word(), s.text));
    let p = ui.painter();
    let block = Rect::from_center_size(
        Pos2::new(line.left() + 5.0, line.center().y),
        Vec2::splat(10.0),
    );
    match s.mark {
        Mark::Ready => finish::code(p, block, REPAIR, true),
        Mark::ToDo => finish::warning(p, block.center(), WARN),
        Mark::Waiting | Mark::NotNeeded => finish::code(p, block, UNKNOWN, false),
    }
    let at = Pos2::new(block.right() + 12.0, line.center().y);
    finish::label(p, at, Align2::LEFT_CENTER, s.name, LABEL, INK);
    let ink = match s.mark {
        Mark::Ready => DIM,
        Mark::ToDo => WARN,
        Mark::Waiting | Mark::NotNeeded => FAINT,
    };
    finish::label(
        p,
        line.right_center(),
        Align2::RIGHT_CENTER,
        s.mark.word(),
        LABEL,
        ink,
    );
    let indent = 22.0;
    let mut body = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_max(
        Pos2::new(line.left() + indent, line.bottom() + 6.0),
        Pos2::new(line.right(), f32::INFINITY),
    )));
    note(&mut body, &s.text, DIM);
    if let Some(t) = s.progress {
        body.add_space(4.0);
        meter(&mut body, t);
    }
    let mut clicked = None;
    if let Some(fix) = s.fix {
        body.add_space(8.0);
        if primary(&mut body, &fix.label()).clicked() {
            clicked = Some(fix);
        }
    }
    let used = body.min_rect().bottom();
    ui.allocate_exact_size(
        Vec2::new(ui.available_width(), used - line.bottom() + 14.0),
        Sense::hover(),
    );
    let p = ui.painter();
    let y = p.round_to_pixel_center(ui.cursor().top() - 1.0);
    p.hline(line.x_range(), y, Stroke::new(finish::hair(p), ROW_RULE));
    clicked
}

fn meter(ui: &mut Ui, t: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 0, MAT);
    finish::outline(p, rect, RULE);
    let run = rect.shrink(2.0);
    p.rect_filled(
        run.with_max_x(egui::lerp(run.x_range(), t.clamp(0.0, 1.0))),
        0,
        GPU,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_say_what_to_do() {
        let mut wizard = Wizard::new();
        wizard.holders = vec![(42, "EOS Utility")];
        let status = Status {
            vcam: Some("Windows refused to create the camera".into()),
            camera: Some("Canon EOS REBEL T3".into()),
            mode: Some(0x09),
            ..Status::default()
        };
        let steps = wizard.steps(&status);
        let marks: Vec<Mark> = steps.iter().map(|s| s.mark).collect();
        assert_eq!(
            marks,
            [Mark::ToDo, Mark::Ready, Mark::Ready, Mark::ToDo, Mark::ToDo]
        );
        let fixes: Vec<Option<Fix>> = steps.iter().map(|s| s.fix).collect();
        assert_eq!(
            fixes,
            [Some(Fix::Install), None, None, None, Some(Fix::Download)]
        );
        let without_camera = Status {
            camera: None,
            ..status.clone()
        };
        assert_eq!(
            wizard.holder_step(&without_camera).fix,
            Some(Fix::Quit(42, "EOS Utility")),
            "a holder only matters while Viewfinder hasn't got the camera"
        );
        wizard.here = false;
        let elsewhere = Status {
            vcam: None,
            ..status.clone()
        };
        assert_eq!(
            wizard.install_step(&elsewhere).fix,
            Some(Fix::Install),
            "an old install somewhere else still needs Install"
        );
        wizard.here = true;

        assert_eq!(
            steps.get(3).map(|s| s.text.as_str()),
            Some(
                "The dial is on Full Auto. Turn it to P, Tv, Av or M so Viewfinder can set exposure and white balance."
            )
        );

        wizard.holders.clear();
        wizard.card = Some("NVIDIA GeForce GTX 1650".into());
        let status = Status {
            mode: Some(0x03),
            vsr_available: false,
            ..status
        };
        let steps = wizard.steps(&Status {
            vcam: None,
            ..status
        });
        let marks: Vec<Mark> = steps.iter().map(|s| s.mark).collect();
        assert_eq!(
            marks,
            [
                Mark::Ready,
                Mark::Ready,
                Mark::Ready,
                Mark::Ready,
                Mark::NotNeeded
            ]
        );
    }

    #[test]
    fn steps_wait_while_something_runs() {
        let mut wizard = Wizard::new();
        wizard.holders = vec![(42, "EOS Utility")];
        let status = Status {
            vcam: Some("Windows refused to create the camera".into()),
            ..Status::default()
        };
        *lock(&wizard.quit) = Quit::Ending(42);
        let closing = wizard.holder_step(&status);
        assert_eq!(
            (closing.mark, closing.fix),
            (Mark::Waiting, None),
            "no second Quit while one runs"
        );
        *lock(&wizard.quit) = Quit::Idle;

        wizard.installing = Some(10.0);
        wizard.give_up_installing(20.0);
        assert_eq!(wizard.install_step(&status).mark, Mark::Waiting);
        wizard.give_up_installing(45.0);
        let again = wizard.install_step(&status);
        assert_eq!(
            again.fix,
            Some(Fix::Install),
            "a failed install offers Install again"
        );
        assert!(again.text.starts_with("The installer stopped"));
    }
}
