use std::sync::Arc;
use std::sync::atomic::Ordering;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Galley, Id, Layout, Painter, Pos2, Rect, Response, Role,
    Sense, Stroke, StrokeKind, TextFormat, TextureHandle, Ui, UiBuilder, Vec2, WidgetInfo,
};
use shared::YUY2;

use super::finish::{self, Key};
use super::{
    App, BG, CUE, DARK_GLASS, MUTED, PRINT, SIGNAL, TALLY, TEXT, WARN, chrome, clipped,
    diagnostics, histogram, primary, readout,
};
use crate::pipeline::Look;
use crate::worker::{Command, Status};

/// What one frame may take at the virtual camera's 30 fps.
const BUDGET_MS: f32 = 1000.0 / 30.0;
/// Where the frame budget meter tops out.
const METER_MS: f32 = 40.0;
/// Where the meter turns amber, short of the budget.
const NEAR_MS: f32 = 30.0;
/// The light of a camera key, which is a source rather than a switch.
const SOURCE: Color32 = Color32::from_rgb(214, 218, 228);
/// The patch between stages and its jacks, unlit.
const PATCH: Color32 = Color32::from_gray(70);
const JACK: Color32 = Color32::from_gray(30);
/// The meter's run when only the total was measured, with no split by stage.
const UNSPLIT: Color32 = Color32::from_gray(200);
/// A dark monitor's legend.
const REST: Color32 = Color32::from_gray(128);
/// How long the frame budget's peak holds before it falls back, in seconds.
const PEAK_HOLD: f64 = 2.0;
/// How often Preview's still is refreshed while live, in seconds. It's a copy of the decoded
/// frame on the CPU and leaves the GPU alone.
const STILL_EVERY: f64 = 2.0;
/// The stages as `Status::stage_ms` orders them, with the tint each runs in on the meter.
const STAGES: [(&str, Color32); 4] = [
    ("Decode", Color32::from_rgb(96, 165, 250)),
    ("Repair", Color32::from_rgb(45, 200, 180)),
    ("GPU", Color32::from_rgb(168, 132, 255)),
    ("Output", Color32::from_rgb(150, 160, 182)),
];

/// What the switcher keeps between frames.
#[derive(Default)]
pub(super) struct Bus {
    /// The last processed frame. The window gets one picture at a time, so while Compare
    /// switches it to the camera original, Program holds this.
    held: Option<TextureHandle>,
    /// The levels Denoise and Super Res come back at when switched on again.
    denoise: u32,
    vsr: u32,
    /// The highest time per frame lately, and when it was seen.
    peak: f32,
    peak_at: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    On,
    Off,
    Fault,
}

struct Stage {
    name: &'static str,
    detail: String,
    state: State,
    /// The light when on: yellow for a switch, red for an app taking the picture, neutral
    /// for the camera.
    lit: Color32,
    toggle: bool,
    hint: String,
}

impl Bus {
    /// Keeps the highest total for `PEAK_HOLD` seconds, then falls back to the current one.
    /// Nothing is held while there is no reading.
    fn hold(&mut self, total: f32, now: f64) {
        if total <= 0.0 || total >= self.peak || now - self.peak_at > PEAK_HOLD {
            (self.peak, self.peak_at) = (total, now);
        }
    }
}

impl App {
    pub(super) fn switcher(&mut self, ui: &mut Ui, status: &Status, look: &mut Look) {
        let narrow = ui.available_width() < 1000.0;
        let side = if narrow { 330.0 } else { 344.0 };
        let whole = ui.max_rect();
        let bar = Rect::from_min_size(whole.min, Vec2::new(whole.width(), 52.0));
        let palette = Rect::from_min_max(
            Pos2::new(whole.right() - side, bar.bottom() + 1.0),
            whole.max,
        );
        let mixer = Rect::from_min_max(
            Pos2::new(whole.left(), bar.bottom()),
            Pos2::new(palette.left(), whole.bottom()),
        );
        {
            let p = ui.painter();
            finish::chassis(p, whole, BG);
            finish::module(p, bar, 0.0);
            finish::module(p, palette, 0.0);
        }
        // Made in reading order, which is the order Tab takes too: the bar, the monitors and the
        // chain, the settings, and last the window's own buttons.
        let region = |ui: &mut Ui, name: &str, rect: Rect| {
            ui.new_child(
                UiBuilder::new()
                    .id_salt(name)
                    .max_rect(rect)
                    .accessibility_label(name),
            )
        };
        Self::top_bar(
            &mut region(ui, "Title Bar", bar.with_min_x(bar.left() + 16.0)),
            status,
        );
        self.mixer(
            &mut region(ui, "Monitors", mixer.shrink(16.0)),
            status,
            look,
        );
        let mut settings = region(ui, "Settings", palette.shrink2(Vec2::new(16.0, 4.0)));
        // A bar that's always there when the settings run past the bottom, so it's plain there
        // is more.
        settings.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        let out = egui::ScrollArea::vertical().show(&mut settings, |ui| {
            Self::picture_section(ui, status, look);
            self.exposure_section(ui, status);
            self.colour_and_focus(ui, status);
            diagnostics(ui, status);
            ui.add_space(14.0);
            // egui leaves a control Tab moved to wherever it is, so the panel follows it.
            if let Some(r) = ui
                .memory(egui::Memory::focused)
                .and_then(|id| ui.ctx().read_response(id))
                && r.gained_focus()
                && ui.min_rect().contains_rect(r.rect)
            {
                // Clear of the fades at the top and bottom edges.
                ui.scroll_to_rect(r.rect.expand2(Vec2::new(0.0, 26.0)), None);
            }
        });
        let view = out.inner_rect;
        let p = ui.painter();
        if out.state.clamped_offset().y > 0.5 {
            finish::fade(
                p,
                Rect::from_min_size(view.min, Vec2::new(view.width(), 18.0)),
                palette,
                false,
            );
        }
        if out.content_size.y - out.state.clamped_offset().y > view.height() + 0.5 {
            finish::fade(
                p,
                Rect::from_min_max(Pos2::new(view.left(), view.bottom() - 22.0), view.max),
                palette,
                true,
            );
        }
        chrome::window_buttons(ui, whole);
    }

    fn mixer(&mut self, ui: &mut Ui, status: &Status, look: &mut Look) {
        let area = ui.available_rect_before_wrap();
        let (gap, bar_h, scopes_min, scopes_max) = (14.0, 44.0, 128.0, 180.0);
        let fixed = bar_h + gap + 58.0 + gap;
        let mon_w = ((area.width() - gap) / 2.0)
            .min((area.height() - fixed - scopes_min) * 16.0 / 9.0)
            .floor();
        let mon_h = (mon_w * 9.0 / 16.0).round();
        // Height the monitors can't use goes to the keys first, then the scopes up to a
        // point, and the rest is shared above and below.
        let chain_h = 58.0 + (area.height() - mon_h - fixed - scopes_max).clamp(0.0, 10.0);
        let scopes_h =
            (area.height() - mon_h - fixed - (chain_h - 58.0)).clamp(scopes_min, scopes_max);
        let total = mon_h + fixed + (chain_h - 58.0) + scopes_h;
        let width = mon_w * 2.0 + gap;
        let origin = Pos2::new(
            (area.center().x - width / 2.0).round(),
            area.top() + ((area.height() - total) / 2.0).max(0.0).round(),
        );

        let preview = Rect::from_min_size(origin, Vec2::new(mon_w, mon_h));
        let program = preview.translate(Vec2::new(mon_w + gap, 0.0));
        let preview_bar = Rect::from_min_size(preview.left_bottom(), Vec2::new(mon_w, bar_h));
        let program_bar = Rect::from_min_size(program.left_bottom(), Vec2::new(mon_w, bar_h));
        let chain = Rect::from_min_size(
            Pos2::new(origin.x, preview_bar.bottom() + gap),
            Vec2::new(width, chain_h),
        );
        let scopes = Rect::from_min_size(
            Pos2::new(origin.x, chain.bottom() + gap),
            Vec2::new(width, scopes_h),
        );
        // The glass sits inside the bezel, which carries the name strip under it.
        let glass =
            |r: Rect| Rect::from_min_max(r.min + Vec2::splat(6.0), r.max - Vec2::new(6.0, 0.0));
        let (preview_glass, program_glass) = (glass(preview), glass(program));

        // Compare swaps the window's one picture between processed and original, so the
        // processed texture is set aside for Program as it starts and put back as it ends.
        let was = self.hub.compare.load(Ordering::Relaxed);
        let live_original = was && self.original && status.live;
        let on_air = status.consumer.is_some();
        bezel(
            ui.painter(),
            Id::unique("preview tally"),
            preview.union(preview_bar),
            preview_glass,
            CUE,
            live_original,
        );
        bezel(
            ui.painter(),
            Id::unique("program tally"),
            program.union(program_bar),
            program_glass,
            TALLY,
            on_air,
        );

        let (comparing, now) = self.preview_strip(ui, preview_bar, status, was);
        self.program_strip(ui, program_bar, comparing, on_air);
        self.preview_glass(ui, preview_glass, status, comparing, live_original);
        self.program_glass(ui, program_glass, status, comparing, on_air);

        self.chain(ui, chain, status, look);
        let half = (width - gap) / 2.0;
        let card = Rect::from_min_size(
            scopes.min + Vec2::new(half + gap, 0.0),
            Vec2::new(half, scopes_h),
        );
        self.scopes(
            ui,
            status,
            Rect::from_min_size(scopes.min, Vec2::new(half, scopes_h)),
            card,
            now,
        );
    }

    /// Preview's name strip with the Compare button, then the swap Compare makes and the
    /// still Preview keeps. Returns whether Compare is held now, and the frame's time.
    fn preview_strip(&mut self, ui: &mut Ui, bar: Rect, status: &Status, was: bool) -> (bool, f64) {
        self.monitor_bar(ui, bar, "Preview", |app, ui, text_room| {
            let mut child = ui.new_child(
                UiBuilder::new()
                    .max_rect(bar.shrink2(Vec2::new(10.0, 0.0)))
                    .layout(Layout::right_to_left(Align::Center)),
            );
            let used = app.compare_button(&mut child, status.live);
            (["Camera Original", "Original"], text_room - used - 8.0)
        });
        let comparing = self.hub.compare.load(Ordering::Relaxed);
        if comparing && !was {
            self.bus.held = self.texture.take();
        }
        if was && !comparing {
            let shown = std::mem::replace(&mut self.texture, self.bus.held.take());
            if self.original {
                self.still = shown;
                self.still_asked = ui.input(|i| i.time);
            }
            self.original = false;
        }
        // Preview keeps a still of the camera's own picture, asked for as live view starts
        // and refreshed every couple of seconds.
        let now = ui.input(|i| i.time);
        if !status.live {
            self.still_asked = f64::NEG_INFINITY;
        } else if !comparing && (self.still.is_none() || now - self.still_asked > STILL_EVERY) {
            self.hub.want_original.store(true, Ordering::Relaxed);
            self.still_asked = now;
        }
        (comparing, now)
    }

    /// Program's name strip, with its tally lamp and whether it's on air.
    fn program_strip(&mut self, ui: &mut Ui, bar: Rect, comparing: bool, on_air: bool) {
        let app_held = self.bus.held.is_some();
        self.monitor_bar(ui, bar, "Program", |_, ui, text_room| {
            let at = bar.right_center() - Vec2::new(14.0, 0.0);
            let (state, ink) = if on_air {
                ("On Air", finish::bright(TALLY))
            } else {
                ("Off Air", MUTED)
            };
            let used = finish::legend(ui.painter(), at, Align2::RIGHT_CENTER, state, 11.0, ink);
            tally_lamp(
                ui.painter(),
                Id::unique("program lamp"),
                Pos2::new(used.left() - 11.0, at.y),
                TALLY,
                on_air,
            );
            let what = if comparing && app_held {
                ["Held While Comparing", "Held"]
            } else {
                ["What Apps Get", "To Apps"]
            };
            (what, text_room - used.width() - 30.0)
        });
    }

    /// Preview's picture: the camera original while comparing, else the still.
    fn preview_glass(
        &self,
        ui: &Ui,
        preview_glass: Rect,
        status: &Status,
        comparing: bool,
        live_original: bool,
    ) {
        let painter = ui.painter_at(preview_glass);
        let shown = match (comparing && self.original, status.live) {
            (true, true) => self.texture.as_ref(),
            (false, true) => self.still.as_ref(),
            _ => None,
        };
        let state = shown.map_or_else(
            || {
                let state = match (comparing, status.live) {
                    (_, false) => "No Picture",
                    (true, true) => "Switching",
                    (false, true) => "Starting",
                };
                rest(&painter, preview_glass, &format!("Preview · {state}"));
                state
            },
            |t| {
                fit(&painter, preview_glass, t);
                let plate = if live_original { "Live" } else { "Still" };
                super::plate(&painter, preview_glass, plate, live_original.then_some(CUE));
                plate
            },
        );
        readout(
            &ui.interact(preview_glass, Id::unique("preview glass"), Sense::hover()),
            || format!("Preview: {state}, the camera's own picture"),
        );
    }

    /// Program's picture, what apps get, or why there is none yet.
    fn program_glass(
        &self,
        ui: &mut Ui,
        program_glass: Rect,
        status: &Status,
        comparing: bool,
        on_air: bool,
    ) {
        let on_air = if on_air { "On Air" } else { "Off Air" };
        let program = ui.interact(program_glass, Id::unique("program glass"), Sense::hover());

        match (&self.bus.held, comparing) {
            (Some(t), true) => {
                readout(&program, || {
                    format!("Program: {on_air}, held while comparing")
                });
                let painter = ui.painter_at(program_glass);
                fit(&painter, program_glass, t);
                super::plate(&painter, program_glass, "Held", None);
            }
            _ if status.live && self.texture.is_some() => {
                readout(&program, || format!("Program: {on_air}, what apps get"));
                self.picture(ui, program_glass, status);
            }
            _ => {
                let problem = status.problem.clone().unwrap_or_default();
                let (title, detail) = match (&status.camera, status.live, status.resting) {
                    (_, _, true) => ("The Camera Is Cooling Down", problem),
                    (None, _, _) => {
                        let why = if problem.is_empty() {
                            "Turn the T3 on and plug it in over USB. Live view starts by itself."
                        } else {
                            &problem
                        };
                        ("Looking for the Camera", why.to_owned())
                    }
                    (Some(_), false, _) => (
                        "Starting Live View",
                        "The mirror flips up and the first frame takes about a second.".into(),
                    ),
                    (Some(_), true, _) => ("Waiting for the First Frame", String::new()),
                };
                readout(&program, || {
                    if detail.is_empty() {
                        format!("Program: {on_air}, {title}")
                    } else {
                        format!("Program: {on_air}, {title}. {detail}")
                    }
                });
                if notice(
                    ui,
                    program_glass,
                    title,
                    &detail,
                    status.resting.then_some("Try Again Now"),
                ) {
                    self.send(Command::Retry);
                }
            }
        }
    }

    /// The frame budget meter and the histogram, side by side under the chain.
    fn scopes(&mut self, ui: &mut Ui, status: &Status, meter: Rect, card: Rect, now: f64) {
        // The last readings stay in the status after live view stops, but they are no longer
        // current, so nothing is shown then.
        let stages = if status.live {
            status.stage_ms
        } else {
            [0.0; 4]
        };
        let sum: f32 = stages.iter().sum();
        let total = match (status.live, sum > 0.0) {
            (false, _) => 0.0,
            (true, true) => sum,
            (true, false) => status.frame_ms,
        };
        self.bus.hold(total, now);
        budget(ui, meter, stages, total, self.bus.peak);
        let inner = card_frame(ui.painter(), card, "Histogram");
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner));
        histogram(
            &mut child,
            if status.live { &self.histogram } else { &[] },
            inner.height(),
        );
    }

    /// The name strip under a monitor. `extra` draws whatever sits on the right and returns
    /// the line under the name, long and short, plus how much width is left for it.
    fn monitor_bar(
        &mut self,
        ui: &mut Ui,
        bar: Rect,
        name: &str,
        extra: impl FnOnce(&mut Self, &mut Ui, f32) -> ([&'static str; 2], f32),
    ) {
        let (what, room) = extra(self, ui, bar.width() - 28.0);
        let p = ui.painter();
        finish::legend(
            p,
            bar.left_top() + Vec2::new(14.0, 8.0),
            Align2::LEFT_TOP,
            name,
            11.5,
            TEXT,
        );
        let font = FontId::proportional(12.0);
        let fits = |t: &str| {
            ui.fonts_mut(|f| f.layout_no_wrap(t.to_owned(), font.clone(), TEXT).size().x) <= room
        };
        // A cut-off word reads worse than none, so the line goes when even the short one won't fit.
        let what = [what[0], what[1]]
            .into_iter()
            .find(|t| fits(t))
            .unwrap_or_default();
        ui.painter().text(
            bar.left_bottom() + Vec2::new(14.0, -7.0),
            Align2::LEFT_BOTTOM,
            what,
            font,
            MUTED,
        );
    }

    /// The signal chain from the camera to the app, each processing stage a switch.
    fn chain(&mut self, ui: &Ui, rect: Rect, status: &Status, look: &mut Look) {
        let link = if rect.width() < 640.0 { 8.0 } else { 20.0 };
        let w = (rect.width() - link * 5.0) / 6.0;
        let short = w < 100.0;
        let camera = camera_stage(status, short);
        let repair = switch(
            "Repair",
            look.fix_pixels,
            match status.stuck {
                0 => "None Yet".into(),
                1 => "1 Pixel".into(),
                n => format!("{n} Pixels"),
            },
            "Fills hot and dead pixels from their neighbours",
        );
        let [denoise, super_res, smooth] = gpu_stages(status, look, short);
        let output = output_stage(status, short);
        let stages = [camera, repair, denoise, super_res, smooth, output];

        let boxes: Vec<Rect> = (0..6)
            .map(|i| {
                Rect::from_min_size(
                    rect.min + Vec2::new(i as f32 * (w + link), 0.0),
                    Vec2::new(w, rect.height()),
                )
            })
            .collect();
        let reach = if link < 12.0 { 5.0 } else { 9.0 };
        let mut lit = [0.0; 6];
        for (i, ((s, r), slot)) in stages.iter().zip(&boxes).zip(&mut lit).enumerate() {
            let (response, t) = stage(ui, *r, s, reach);
            *slot = t;
            if response.clicked() {
                match i {
                    1 => look.fix_pixels = !look.fix_pixels,
                    2 => flip(
                        &mut look.denoise,
                        &mut self.bus.denoise,
                        Look::default().denoise,
                    ),
                    3 => flip(
                        &mut look.vsr_quality,
                        &mut self.bus.vsr,
                        Look::default().vsr_quality,
                    ),
                    4 => look.smooth = !look.smooth,
                    _ => {}
                }
            }
        }
        // Over the keys, so the dots sit on their edges like jacks.
        for (i, (([prev, next], [left, right]), [here, there])) in stages
            .array_windows()
            .zip(boxes.array_windows())
            .zip(lit.array_windows())
            .enumerate()
        {
            let cut = next.state == State::Fault || (i == 0 && prev.state == State::Fault);
            connector(
                ui.painter(),
                left.right(),
                right.left(),
                rect.center().y,
                cut,
                here.min(*there),
            );
        }
    }
}

/// A key that switches a stage on and off.
fn switch(name: &'static str, on: bool, detail: String, hint: &str) -> Stage {
    Stage {
        name,
        detail: if on { detail } else { "Off".into() },
        state: if on { State::On } else { State::Off },
        lit: SIGNAL,
        toggle: true,
        hint: hint.into(),
    }
}

/// A stage that can't run, with why in its hint.
fn fault(name: &'static str, detail: &str, hint: String, toggle: bool) -> Stage {
    Stage {
        name,
        detail: detail.into(),
        state: State::Fault,
        lit: SIGNAL,
        toggle,
        hint,
    }
}

/// A narrow key takes the short word, since a cut-off one reads worse.
fn says(short: bool, long: &str, brief: &str) -> String {
    if short {
        brief.to_owned()
    } else {
        long.to_owned()
    }
}

/// The name of a level, or Custom for one that isn't named.
fn level(v: u32, names: &[(u32, &str)]) -> String {
    names
        .iter()
        .find(|n| n.0 == v)
        .map_or("Custom", |n| n.1)
        .to_owned()
}

fn no_gpu(status: &Status) -> String {
    status.vsr_problem.clone().unwrap_or_else(|| {
        "NVIDIA's libraries aren't installed. Run install.ps1 to fetch them.".into()
    })
}

/// The camera at the head of the chain, a source rather than a switch.
fn camera_stage(status: &Status, short: bool) -> Stage {
    match (&status.camera, &status.problem) {
        (None, problem) => Stage {
            name: "Camera",
            detail: "Not Found".into(),
            state: State::Fault,
            lit: SOURCE,
            toggle: false,
            hint: problem
                .clone()
                .unwrap_or_else(|| "Turn the T3 on and plug it in over USB.".into()),
        },
        (Some(model), problem) => Stage {
            name: "Camera",
            detail: match (status.live, status.resting) {
                (true, _) => format!("{:.1} fps", status.fps),
                (false, true) => says(short, "Cooling Down", "Cooling"),
                (false, false) => "Off".into(),
            },
            state: match (problem, status.live) {
                (Some(_), _) => State::Fault,
                (None, true) => State::On,
                (None, false) => State::Off,
            },
            lit: SOURCE,
            toggle: false,
            hint: problem
                .clone()
                .unwrap_or_else(|| format!("{model}, live view")),
        },
    }
}

/// Denoise, Super Res and Smooth, which all run on the NVIDIA GPU.
fn gpu_stages(status: &Status, look: &Look, short: bool) -> [Stage; 3] {
    let gpu = status.vsr_available && status.vsr_problem.is_none();
    let denoise_levels = [(8, "Low"), (9, "Medium"), (10, "High"), (11, "Ultra")];
    let vsr_levels = [(2, "Fast"), (3, "High"), (4, "Ultra")];
    let (denoise, super_res) = if gpu {
        (
            switch(
                "Denoise",
                look.denoise > 0,
                level(look.denoise, &denoise_levels),
                "NVIDIA noise reduction, before upscaling",
            ),
            switch(
                "Super Res",
                look.vsr_quality > 0,
                level(look.vsr_quality, &vsr_levels),
                "NVIDIA super resolution",
            ),
        )
    } else {
        (
            fault("Denoise", "Unavailable", no_gpu(status), false),
            fault("Super Res", "Unavailable", no_gpu(status), false),
        )
    };
    let smooth = match (gpu, look.smooth, &status.smooth_problem) {
        (false, _, _) => fault("Smooth", "Unavailable", no_gpu(status), false),
        (true, true, _) if look.vsr_quality == 0 => fault(
            "Smooth",
            &says(short, "Needs Super Res", "Needs SR"),
            "Smooth Motion needs Super Resolution on.".into(),
            true,
        ),
        (true, true, Some(p)) => fault(
            "Smooth",
            "Failed",
            format!("Frame generation failed, so frames are paced without it. {p}"),
            true,
        ),
        (true, on, _) => switch(
            "Smooth",
            on,
            if status.smoothing {
                says(short, "Pacing 30 fps", "30 fps")
            } else {
                "Ready".into()
            },
            "Paces apps at an even 30 fps with NVIDIA frame generation. Adds about 70 ms of delay.",
        ),
    };
    [denoise, super_res, smooth]
}

/// The virtual camera at the end of the chain, lit while an app takes the picture.
fn output_stage(status: &Status, short: bool) -> Stage {
    let kind = |f: shared::Format| {
        if short {
            String::new()
        } else if f.fourcc == YUY2 {
            " YUY2".into()
        } else {
            " NV12".into()
        }
    };
    match (&status.vcam, status.consumer) {
        (Some(problem), _) => Stage {
            name: "Output",
            detail: "Not Set Up".into(),
            state: State::Fault,
            lit: SIGNAL,
            toggle: false,
            hint: problem.clone(),
        },
        (None, consumer) => Stage {
            name: "Output",
            detail: consumer
                .map_or_else(|| "No App".into(), |f| format!("{}p{}", f.height, kind(f))),
            state: if consumer.is_some() {
                State::On
            } else {
                State::Off
            },
            lit: SIGNAL,
            toggle: false,
            hint: consumer.map_or_else(
                || "No app is using the camera".into(),
                |f| format!("An app is taking {}×{}", f.width, f.height),
            ),
        },
    }
}

/// A monitor's housing: a graphite bezel with a tally lamp along its top, lighting it and
/// its surround when `on`, and dark glass set into it.
fn bezel(p: &Painter, id: Id, rect: Rect, glass: Rect, light: Color32, on: bool) {
    let t = finish::lit(p, id, on);
    if t > 0.0 {
        finish::glow(p, rect, 6.0, light.gamma_multiply(0.18 * t), 16.0);
    }
    p.add(
        egui::epaint::RectShape::filled(
            rect.translate(Vec2::new(0.0, 4.0)),
            8.0,
            Color32::from_black_alpha(150),
        )
        .with_blur_width(14.0),
    );
    finish::module(p, rect, 6.0);
    finish::well(p, glass, 2.0);
    finish::vignette(p, glass.shrink(1.0));
    let strip = Rect::from_min_max(
        Pos2::new(rect.left() + 12.0, rect.top() + 2.0),
        Pos2::new(rect.right() - 12.0, rect.top() + 4.0),
    );
    p.rect_filled(strip, 1.0, DARK_GLASS.lerp_to_gamma(light, 0.16));
    if t > 0.0 {
        p.rect_stroke(
            rect,
            6.0,
            Stroke::new(1.0, light.gamma_multiply(0.55 * t)),
            StrokeKind::Outside,
        );
        finish::glow(p, strip, 1.0, light.gamma_multiply(0.55 * t), 10.0);
        p.rect_filled(strip, 1.0, finish::bright(light).gamma_multiply(t));
    }
}

/// A round tally lamp, lit or dark.
fn tally_lamp(p: &Painter, id: Id, centre: Pos2, light: Color32, on: bool) {
    let t = finish::lit(p, id, on);
    p.circle_filled(centre, 3.5, DARK_GLASS.lerp_to_gamma(light, 0.2));
    p.circle_stroke(
        centre,
        3.5,
        Stroke::new(1.0, Color32::from_black_alpha(160)),
    );
    if t > 0.0 {
        finish::glow(
            p,
            Rect::from_center_size(centre, Vec2::splat(7.0)),
            3.5,
            light.gamma_multiply(0.5 * t),
            10.0,
        );
        p.circle_filled(centre, 3.5, light.gamma_multiply(t));
    }
}

/// Title-safe corners, as a monitor shows with nothing on it.
fn corners(p: &Painter, rect: Rect) {
    let safe = rect.shrink2(rect.size() * 0.08);
    let arm = (rect.width() * 0.04).clamp(6.0, 12.0);
    let stroke = Stroke::new(1.0, finish::FAINT.gamma_multiply(3.0));
    for (corner, dx, dy) in [
        (safe.left_top(), 1.0, 1.0),
        (safe.right_top(), -1.0, 1.0),
        (safe.left_bottom(), 1.0, -1.0),
        (safe.right_bottom(), -1.0, -1.0),
    ] {
        p.line_segment([corner, corner + Vec2::new(dx * arm, 0.0)], stroke);
        p.line_segment([corner, corner + Vec2::new(0.0, dy * arm)], stroke);
    }
}

/// An empty monitor at rest: its corners and a dim legend.
fn rest(p: &Painter, rect: Rect, title: &str) {
    corners(p, rect);
    let ink = REST;
    let galley = p.layout_job(finish::legend_job(title, 11.0, ink));
    finish::place(p, rect.center(), Align2::CENTER_CENTER, galley, ink);
}

/// A legend and a line of explanation in the middle of an empty monitor, inside its corners,
/// with a button under them if there is one. The explanation gets smaller to fit a small
/// monitor and is left out when even that won't fit, since the monitor's readout still has it.
fn notice(ui: &mut Ui, rect: Rect, title: &str, detail: &str, button: Option<&str>) -> bool {
    corners(ui.painter(), rect);
    let room = rect
        .shrink2(rect.size() * 0.08)
        .shrink2(Vec2::new(10.0, 4.0));
    let p = ui.painter();
    let mut heading = finish::legend_job(title, 12.0, TEXT);
    (heading.wrap.max_width, heading.halign) = (room.width(), Align::Center);
    let heading = p.layout_job(heading);
    let button_h = if button.is_some() { 8.0 + 30.0 } else { 0.0 };
    let lines = [13.0, 12.0, 11.0]
        .into_iter()
        .filter(|_| !detail.is_empty())
        .map(|size| {
            let mut job = egui::text::LayoutJob::simple(
                detail.to_owned(),
                FontId::proportional(size),
                MUTED,
                room.width(),
            );
            job.halign = Align::Center;
            p.layout_job(job)
        })
        .find(|g| heading.size().y + 4.0 + g.size().y + button_h <= room.height());
    let h = heading.size().y + lines.as_ref().map_or(0.0, |g| 4.0 + g.size().y) + button_h;
    let mut y = (room.center().y - h / 2.0).max(room.top()).round();
    y += heading.size().y;
    p.galley(
        Pos2::new(room.center().x, y - heading.size().y),
        heading,
        TEXT,
    );
    if let Some(g) = lines {
        y += 4.0;
        p.galley(Pos2::new(room.center().x, y), g.clone(), MUTED);
        y += g.size().y;
    }
    let Some(text) = button else { return false };
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_size(
                Pos2::new(room.left(), y + 8.0),
                Vec2::new(room.width(), 30.0),
            ))
            .layout(Layout::top_down(Align::Center)),
    );
    primary(&mut child, text).clicked()
}

/// Switches a level off, or back on at the level it had.
const fn flip(value: &mut u32, last: &mut u32, default: u32) {
    if *value > 0 {
        *last = *value;
        *value = 0;
    } else {
        *value = if *last > 0 { *last } else { default };
    }
}

fn fit(painter: &Painter, rect: Rect, t: &TextureHandle) {
    let [w, h] = t.size();
    let scale = (rect.width() / w as f32).min(rect.height() / h as f32);
    let shown = Rect::from_center_size(rect.center(), Vec2::new(w as f32, h as f32) * scale);
    painter.image(
        t.id(),
        shown,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
}

/// A stage as a backlit key. On lights the edge, the legend and the lamp bar. Off leaves the
/// cap dark with an empty lamp, and a fault rings it in amber with a warning triangle.
/// Returns how far it has lit, for the path between keys.
fn stage(ui: &Ui, rect: Rect, s: &Stage, reach: f32) -> (Response, f32) {
    let sense = if s.toggle {
        Sense::click()
    } else {
        Sense::hover()
    };
    let r = ui.interact(rect, Id::unique(("stage", s.name)), sense);
    let name = || format!("{}: {}", s.name, s.detail);
    if s.toggle {
        // A failing switch is still switched on.
        r.widget_info(|| WidgetInfo::selected(Role::CheckBox, true, s.state != State::Off, name()));
    } else {
        readout(&r, name);
    }
    let r = r.on_hover_text(&s.hint);
    let key = Key {
        fault: s.state == State::Fault,
        hot: s.toggle && r.hovered(),
        down: s.toggle && r.is_pointer_button_down_on(),
        focus: r.has_focus(),
        ..Key::new(r.id, s.lit, s.state == State::On)
    };
    let painter = ui.painter();
    let t = finish::key(painter, rect, key, reach, WARN);
    let (name_ink, detail_ink) = match s.state {
        State::Fault => (TEXT, WARN),
        _ => (
            MUTED.lerp_to_gamma(finish::bright(s.lit), t),
            MUTED.lerp_to_gamma(TEXT, t),
        ),
    };
    let compact = rect.width() < 100.0;
    let pad = if compact { 7.0 } else { 11.0 };
    // In the corner above the legend's line, so a long legend can run under it.
    let lamp_at = Pos2::new(rect.right() - 11.0, rect.top() + 6.5);
    finish::lamp(
        painter,
        lamp_at,
        s.state == State::Fault,
        if s.state == State::Fault { WARN } else { s.lit },
        t,
    );
    let size = if compact { 10.0 } else { 11.0 };
    let job = finish::legend_job(s.name, size, name_ink);
    let mut galley = painter.layout_job(job);
    if galley.size().x > rect.width() - pad * 2.0 {
        let mut tight = finish::legend_job(s.name, size, name_ink);
        if let Some(section) = tight.sections.first_mut() {
            section.format.extra_letter_spacing = 0.4;
        }
        galley = painter.layout_job(tight);
    }
    finish::place(
        painter,
        rect.left_top() + Vec2::new(pad, 12.0),
        Align2::LEFT_TOP,
        galley,
        name_ink,
    );
    let room = rect.width() - pad * 2.0;
    let fits = |size: f32| {
        ui.fonts_mut(|f| {
            f.layout_no_wrap(s.detail.clone(), FontId::proportional(size), TEXT)
                .size()
                .x
        }) <= room
    };
    let detail_font = FontId::proportional(match (compact, fits(12.0)) {
        (false, _) => 13.5,
        (true, true) => 12.0,
        (true, false) => 11.0,
    });
    let detail = clipped(ui, &s.detail, detail_font, detail_ink, room);
    painter.galley(
        Align2::LEFT_BOTTOM
            .anchor_size(rect.left_bottom() + Vec2::new(pad, -7.0), detail.size())
            .min,
        detail,
        detail_ink,
    );
    (r, t)
}

/// The path between two stages, jacked into each key's edge. `t` is how far it's lit, which
/// it is while the picture is being shaped on both sides. `cut` means the next stage failed.
#[expect(
    clippy::manual_midpoint,
    reason = "midpoint could round differently, and the drawing must stay the same"
)]
fn connector(p: &Painter, from: f32, to: f32, y: f32, cut: bool, t: f32) {
    let (left, right) = (Pos2::new(from, y), Pos2::new(to, y));
    if cut {
        let mid = (from + to) / 2.0;
        let stroke = Stroke::new(1.5, WARN);
        p.line_segment([left, Pos2::new(mid - 4.0, y)], stroke);
        p.line_segment([Pos2::new(mid + 4.0, y), right], stroke);
        for dx in [-3.0, 3.0] {
            p.line_segment(
                [
                    Pos2::new(mid + dx - 2.5, y + 6.0),
                    Pos2::new(mid + dx + 2.5, y - 6.0),
                ],
                stroke,
            );
        }
        return;
    }
    p.line_segment([left, right], Stroke::new(1.0, PATCH));
    for c in jacks(left, right) {
        p.circle_filled(c, 2.5, JACK);
        p.circle_stroke(c, 2.5, Stroke::new(1.0, PATCH.lerp_to_gamma(TEXT, 0.08)));
    }
    if t > 0.0 {
        p.line_segment(
            [left, right],
            Stroke::new(4.0, SIGNAL.gamma_multiply(0.15 * t)),
        );
        p.line_segment(
            [left, right],
            Stroke::new(1.5, finish::bright(SIGNAL).gamma_multiply(t)),
        );
        for c in jacks(left, right) {
            p.circle_filled(c, 2.5, SIGNAL.gamma_multiply(t));
        }
    }
}

/// A dot on each key's edge, or one between them when the keys stand too close for two.
fn jacks(a: Pos2, b: Pos2) -> impl Iterator<Item = Pos2> {
    let close = b.x - a.x < 12.0;
    std::iter::once(if close { a.lerp(b, 0.5) } else { a }).chain((!close).then_some(b))
}

/// A module with a legend at its head. Returns the room left under the legend.
fn card_frame(p: &Painter, rect: Rect, title: &str) -> Rect {
    finish::module(p, rect, 6.0);
    finish::legend(
        p,
        rect.left_top() + Vec2::new(14.0, 13.0),
        Align2::LEFT_TOP,
        title,
        11.0,
        PRINT,
    );
    Rect::from_min_max(
        rect.left_top() + Vec2::new(14.0, 36.0),
        rect.right_bottom() - Vec2::new(14.0, 14.0),
    )
}

/// A legend item for the budget meter: a stage's name and time, and its tint.
type Item = (Arc<Galley>, Color32);

/// The measured time per frame against what 30 fps allows: the total and what's spare once,
/// and an LED meter whose lit run is split into the stages that took it, each in its tint,
/// with a peak that holds for a moment. A stage that didn't run gets no segments.
fn budget(ui: &Ui, rect: Rect, stages: [f32; 4], total: f32, peak: f32) {
    readout(
        &ui.interact(rect, Id::unique("frame budget"), Sense::hover()),
        || budget_words(stages, total),
    );
    let p = ui.painter();
    let inner = card_frame(p, rect, "Frame Budget");
    p.text(
        Pos2::new(inner.right(), rect.top() + 12.0),
        Align2::RIGHT_TOP,
        format!("{BUDGET_MS:.1} ms at 30 fps"),
        FontId::proportional(12.0),
        MUTED,
    );

    // The legend's items, laid out first to know how many rows they take.
    let items: Vec<Item> = stages
        .iter()
        .zip(STAGES)
        .filter(|(ms, _)| **ms >= 0.05)
        .map(|(ms, (name, tint))| {
            let mut job = finish::legend_job(name, 9.0, MUTED);
            let value = TextFormat {
                font_id: FontId::proportional(11.5),
                color: TEXT,
                ..Default::default()
            };
            job.append(&format!("{ms:.1}"), 3.0, value);
            (p.layout_job(job), tint)
        })
        .collect();
    let rows = legend_rows(&items, inner.width());
    let legend_h = if items.is_empty() {
        0.0
    } else {
        rows.len() as f32 * 16.0
    };
    let reading_h = 24.0;
    let meter_h = 20.0;
    let scale_h = 13.0;
    let show_scale = inner.height() - (reading_h + meter_h + scale_h + legend_h) >= 12.0;
    // The reading at the top and the legend at the bottom, with the meter centred between.
    let middle = meter_h + if show_scale { scale_h } else { 0.0 };
    let mut y = inner.top();

    if total > 0.0 {
        reading(p, inner, y, reading_h, total);
    }
    y = (y + reading_h + (inner.bottom() - legend_h - y - reading_h - middle) / 2.0).round();

    let meter = Rect::from_min_size(
        Pos2::new(inner.left(), y),
        Vec2::new(inner.width(), meter_h),
    );
    meter_leds(p, meter, stages, total, peak);
    let mark = meter_x(meter, BUDGET_MS);
    p.line_segment(
        [
            Pos2::new(mark, meter.top() - 3.0),
            Pos2::new(mark, meter.bottom() + 3.0),
        ],
        Stroke::new(1.5, TEXT),
    );
    y += meter_h;
    if show_scale {
        scale(p, meter, y);
    }

    let mut y = inner.bottom() - legend_h;
    for row in rows {
        let mut x = inner.left();
        for (item, tint) in row {
            let size = item.size();
            let swatch =
                Rect::from_min_size(Pos2::new(x, y + size.y / 2.0 - 3.5), Vec2::new(8.0, 8.0));
            p.rect_filled(swatch, 2.0, tint);
            p.galley(Pos2::new(swatch.right() + 5.0, y), item, TEXT);
            x = swatch.right() + 5.0 + size.x + 14.0;
        }
        y += 16.0;
    }
}

/// What the frame budget reads out to a screen reader.
fn budget_words(stages: [f32; 4], total: f32) -> String {
    if total <= 0.0 {
        return "Frame Budget: no reading".into();
    }
    let (word, by) = if total > BUDGET_MS {
        ("over", total - BUDGET_MS)
    } else {
        ("spare", BUDGET_MS - total)
    };
    let split: Vec<String> = stages
        .iter()
        .zip(STAGES)
        .filter(|(ms, _)| **ms >= 0.05)
        .map(|(ms, (name, _))| format!("{name} {ms:.1}"))
        .collect();
    format!(
        "Frame Budget: {total:.1} ms per frame, {by:.1} ms {word} of {BUDGET_MS:.1}. {}",
        split.join(", ")
    )
}

/// Breaks the legend's items into rows that fit `width`, with at least one row.
fn legend_rows(items: &[Item], width: f32) -> Vec<Vec<Item>> {
    let item_w = |it: &Item| 13.0 + it.0.size().x + 14.0;
    let mut rows = vec![Vec::new()];
    let mut x = 0.0;
    for it in items {
        if x + item_w(it) > width + 12.0 && !rows.last().is_some_and(Vec::is_empty) {
            rows.push(Vec::new());
            x = 0.0;
        }
        x += item_w(it);
        if let Some(row) = rows.last_mut() {
            row.push(it.clone());
        }
    }
    rows
}

/// The total time per frame in large type, and what's spare or over on the right.
fn reading(p: &Painter, inner: Rect, y: f32, reading_h: f32, total: f32) {
    let big = p.text(
        Pos2::new(inner.left(), y + reading_h),
        Align2::LEFT_BOTTOM,
        format!("{total:.1}"),
        FontId::proportional(24.0),
        TEXT,
    );
    p.text(
        big.right_bottom() + Vec2::new(4.0, -4.0),
        Align2::LEFT_BOTTOM,
        "ms per frame",
        FontId::proportional(12.0),
        MUTED,
    );
    let (spare, ink, word) = if total > BUDGET_MS {
        (total - BUDGET_MS, WARN, "over")
    } else {
        (BUDGET_MS - total, TEXT, "spare")
    };
    let words = p.text(
        Pos2::new(inner.right(), y + reading_h - 4.0),
        Align2::RIGHT_BOTTOM,
        word,
        FontId::proportional(12.0),
        MUTED,
    );
    p.text(
        words.left_bottom() + Vec2::new(-4.0, 2.0),
        Align2::RIGHT_BOTTOM,
        format!("{spare:.1} ms"),
        FontId::proportional(15.0),
        ink,
    );
}

/// Where a time in milliseconds falls along the meter.
fn meter_x(meter: Rect, t: f32) -> f32 {
    meter.left() + 3.0 + (meter.width() - 6.0) * t / METER_MS
}

/// The meter's scale in milliseconds, every 10 ms, under it at `y`.
fn scale(p: &Painter, meter: Rect, y: f32) {
    let top = METER_MS as u32;
    for t in (0..=top).step_by(10) {
        let align = match t {
            0 => Align2::LEFT_TOP,
            _ if t == top => Align2::RIGHT_TOP,
            _ => Align2::CENTER_TOP,
        };
        let text = if t == top {
            format!("{t} ms")
        } else {
            t.to_string()
        };
        p.text(
            Pos2::new(meter_x(meter, t as f32), y + 2.0),
            align,
            text,
            FontId::proportional(10.5),
            MUTED,
        );
    }
}

/// Segments lit through each stage's share in its tint, the peak's segment left lit, and
/// the unlit ones faintly green, then amber near the budget, then red past it.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
fn meter_leds(p: &Painter, rect: Rect, stages: [f32; 4], total: f32, peak: f32) {
    finish::well(p, rect, 3.0);
    let row = rect.shrink(3.0);
    let pitch = 7.0;
    let n = ((row.width() + 2.0) / pitch).floor().max(1.0) as usize;
    let w = (row.width() + 2.0) / n as f32;
    let peak_at = ((peak / METER_MS) * n as f32).ceil() as usize;
    let sum: f32 = stages.iter().sum();
    let [.., (_, last)] = STAGES;
    let stage_at = |ms: f32| {
        let mut end = 0.0;
        for (&t, (_, tint)) in stages.iter().zip(STAGES) {
            end += t;
            if ms < end {
                return Some(tint);
            }
        }
        None
    };
    for i in 0..n {
        let start = i as f32 * METER_MS / n as f32;
        let end = (i + 1) as f32 * METER_MS / n as f32;
        let zone = if end <= NEAR_MS + 0.01 {
            CUE
        } else if start < BUDGET_MS {
            WARN
        } else {
            TALLY
        };
        let seg = Rect::from_min_size(
            Pos2::new(row.left() + i as f32 * w, row.top()),
            Vec2::new(w - 2.0, row.height()),
        );
        let held = peak > 0.0 && i + 1 == peak_at;
        let colour = if start < total {
            // Past the budget the run turns red whatever took it. Without a split only the
            // total was measured, so the run is one neutral light.
            if start >= BUDGET_MS {
                TALLY
            } else if sum > 0.0 {
                stage_at(start).unwrap_or(last)
            } else {
                UNSPLIT
            }
        } else if held {
            TEXT
        } else {
            finish::WELL.lerp_to_gamma(zone, 0.13)
        };
        p.rect_filled(seg, 1.0, colour);
        if start < total || held {
            p.hline(
                seg.x_range(),
                seg.top() + 0.5,
                Stroke::new(1.0, finish::bright(colour)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use eframe::egui::{Key, TextureHandle, Vec2};
    use shared::{Format, NV12};

    use super::super::{App, style};
    use super::Bus;
    use crate::pipeline::Look;
    use crate::worker::{Hub, lock};

    /// A ramp for the processed picture and a checkerboard for the original, so the two
    /// monitors can be told apart in a render.
    pub(in super::super) fn frame(hub: &Hub, original: bool) {
        let (width, height) = (320usize, 180usize);
        let rgba = (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i.div_euclid(width));
                let v = if original {
                    if (x.div_euclid(20) + y.div_euclid(20)) % 2 == 0 {
                        200
                    } else {
                        60
                    }
                } else {
                    (x * 255).div_euclid(width) as u8
                };
                [v, if original { v } else { 120 }, 255 - v, 255]
            })
            .collect();
        let mut p = lock(&hub.preview);
        (p.width, p.height, p.rgba, p.fresh, p.histogram, p.original) =
            (width, height, rgba, true, vec![1; 64], original);
    }

    /// Holds Compare and lets it go, checking Program keeps the processed picture while
    /// Preview gets the original.
    #[test]
    fn compare_keeps_both_pictures() {
        let hub = Hub::new(Look::default());
        {
            let mut s = lock(&hub.status);
            s.camera = Some("Canon EOS REBEL T3".into());
            (s.live, s.fps, s.frame_ms, s.vsr_available) = (true, 27.6, 14.7, true);
            s.consumer = Some(Format {
                width: 1280,
                height: 720,
                fourcc: NV12,
            });
        }
        frame(&hub, false);
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::with_context(
            &eframe::egui::Context::default(),
            hub.clone(),
            tx,
            Arc::new(AtomicBool::new(false)),
            true,
        );
        let mut harness = egui_kittest::Harness::builder()
            .with_size(Vec2::new(860.0, 520.0))
            .with_pixels_per_point(1.75)
            .with_context_setup(style)
            .build_ui_state(|ui, app: &mut App| app.draw(ui), app);
        harness.run();
        let processed = harness
            .state()
            .texture
            .as_ref()
            .map(TextureHandle::id)
            .expect("processed texture");
        assert!(
            hub.want_original.load(std::sync::atomic::Ordering::Relaxed),
            "Preview asks for a still as live view starts"
        );

        harness.key_down(Key::Backslash);
        harness.run();
        frame(&hub, true);
        harness.run();
        let held = harness.state().bus.held.as_ref().map(TextureHandle::id);
        assert_eq!(held, Some(processed), "Program holds the processed picture");
        assert!(harness.state().original);
        assert_ne!(
            harness.state().texture.as_ref().map(TextureHandle::id),
            Some(processed),
            "Preview has its own texture"
        );

        harness.key_up(Key::Backslash);
        harness.run();
        assert!(
            harness.state().bus.held.is_none() && harness.state().still.is_some(),
            "the original is kept as a still"
        );
        assert_eq!(
            harness.state().texture.as_ref().map(TextureHandle::id),
            Some(processed),
            "Program is live again"
        );
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the peak is a copy of a reading, so it matches exactly"
    )]
    fn peak_holds_then_falls() {
        let mut bus = Bus::default();
        bus.hold(30.0, 0.0);
        bus.hold(15.0, 1.0);
        assert_eq!(bus.peak, 30.0, "held within two seconds");
        bus.hold(15.0, 2.1);
        assert_eq!(bus.peak, 15.0, "falls back after two seconds");
        bus.hold(0.0, 2.2);
        assert_eq!(bus.peak, 0.0, "nothing held once live view stops");
    }
}
