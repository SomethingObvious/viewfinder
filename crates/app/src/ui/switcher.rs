use std::sync::Arc;
use std::sync::atomic::Ordering;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Galley, Id, Layout, Painter, Pos2, Rect, Response, Role,
    Sense, Stroke, StrokeKind, TextFormat, TextureHandle, Ui, UiBuilder, Vec2, WidgetInfo,
};
use shared::YUY2;

use super::finish;
use super::{
    App, DECODE, DIM, EDGE, FAINT, GPU, GROUND, HOVER_SHEET, INK, LABEL, MAT, OUTPUT,
    PRESSED_SHEET, RED, REPAIR, ROW_RULE, RULE, UNKNOWN, WARN, chrome, clipped, diagnostics,
    histogram, primary, readout,
};
use crate::pipeline::Look;
use crate::usage;
use crate::worker::{Command, Status, lock};

const BUDGET_MS: f32 = 1000.0 / 30.0;
const METER_MS: f32 = 40.0;
const PEAK_HOLD: f64 = 2.0;
/// Seconds. The still is a CPU copy of the decoded frame, so it costs the GPU nothing.
const STILL_EVERY: f64 = 2.0;
pub(super) const BAR: f32 = 52.0;
/// Partly in the margin, so the frame budget keeps room for its legend in the smallest
/// window.
const STRIP_H: f32 = 22.0;
/// In `Status::stage_ms` order.
const STAGES: [(&str, Color32); 4] = [
    ("Decode", DECODE),
    ("Repair", REPAIR),
    ("GPU", GPU),
    ("Output", OUTPUT),
];

#[derive(Default)]
pub(super) struct Bus {
    denoise: u32,
    vsr: u32,
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
    code: Color32,
    toggle: bool,
    hint: String,
}

impl Bus {
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
        let bar = Rect::from_min_size(whole.min, Vec2::new(whole.width(), BAR));
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
            p.rect_filled(whole, 0, GROUND);
            let rule = Stroke::new(finish::hair(p), RULE);
            p.hline(whole.x_range(), p.round_to_pixel_center(bar.bottom()), rule);
            if !self.setup.open {
                p.vline(
                    p.round_to_pixel_center(palette.left()),
                    whole.y_range(),
                    rule,
                );
            }
        }
        // Made in reading order, which is the order Tab takes too.
        let region = |ui: &mut Ui, name: &str, rect: Rect| {
            ui.new_child(
                UiBuilder::new()
                    .id_salt(name)
                    .max_rect(rect)
                    .accessibility_label(name),
            )
        };
        Self::top_bar(&region(ui, "Title Bar", bar), bar, palette.left(), status);
        if self.setup.open {
            let below = whole.with_min_y(bar.bottom() + 1.0);
            self.setup_sheet(&mut region(ui, "Setup", below), below, status);
            chrome::window_buttons(ui, whole);
            return;
        }
        let inner = mixer.shrink(16.0);
        let strip = inner
            .with_max_y(mixer.bottom() - 8.0)
            .with_min_y(mixer.bottom() - 8.0 - STRIP_H);
        self.mixer(
            &mut region(ui, "Monitors", inner.with_max_y(strip.top() - 12.0)),
            status,
            look,
        );
        usage_strip(&region(ui, "Usage", strip), strip);
        let mut settings = region(
            ui,
            "Settings",
            palette
                .with_min_x(palette.left() + 1.0)
                .shrink2(Vec2::new(18.0, 4.0)),
        );
        settings.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        egui::ScrollArea::vertical().show(&mut settings, |ui| {
            ui.add_space(8.0);
            self.camera_row(ui, status);
            self.hide_row(ui, status);
            self.processing_section(ui, status, look);
            self.picture_section(ui, look);
            self.exposure_section(ui, status);
            self.adjust_section(ui, status, look);
            self.colour_and_focus(ui, status);
            diagnostics(ui, status);
            self.general_section(ui, look);
            ui.add_space(14.0);
            // egui leaves a control Tab moved to wherever it is, so the panel follows it.
            if let Some(r) = ui
                .memory(egui::Memory::focused)
                .and_then(|id| ui.ctx().read_response(id))
                && r.gained_focus()
                && ui.min_rect().contains_rect(r.rect)
            {
                ui.scroll_to_rect(r.rect.expand2(Vec2::new(0.0, 26.0)), None);
            }
        });
        chrome::window_buttons(ui, whole);
    }

    fn mixer(&mut self, ui: &mut Ui, status: &Status, look: &mut Look) {
        let area = ui.available_rect_before_wrap();
        let (gap, bar_h, scopes_min, scopes_max) = (14.0, 48.0, 128.0, 180.0);
        let fixed = bar_h + gap + 58.0 + gap;
        let mon_w = ((area.width() - gap) / 2.0)
            .min((area.height() - fixed - scopes_min) * 16.0 / 9.0)
            .floor();
        let mon_h = (mon_w * 9.0 / 16.0).round();
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

        let on_air = status.consumer.is_some() && !status.hidden;
        for mat in [preview, program] {
            ui.painter().rect_filled(mat, 0, MAT);
        }

        let (comparing, now) = self.preview_strip(ui, preview_bar, status);
        self.program_strip(ui, program_bar, status);
        self.preview_glass(ui, preview, status, comparing && status.live);
        self.program_glass(ui, program, status, on_air);
        let p = ui.painter();
        let t = finish::lit(p, Id::unique("program on air"), on_air);
        for (mat, colour) in [(preview, RULE), (program, RULE.lerp_to_gamma(RED, t))] {
            p.rect_stroke(
                mat,
                0,
                Stroke::new(finish::hair(p), colour),
                StrokeKind::Outside,
            );
        }

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

    fn preview_strip(&mut self, ui: &mut Ui, bar: Rect, status: &Status) -> (bool, f64) {
        self.monitor_bar(ui, bar, "Preview", |app, ui, text_room| {
            let mut child = ui.new_child(
                UiBuilder::new()
                    .max_rect(bar.with_min_y(bar.top() + 8.0))
                    .layout(Layout::right_to_left(Align::Min)),
            );
            let used = app.compare_button(&mut child, status.live);
            (["Camera Original", "Original"], text_room - used - 12.0)
        });
        let comparing = self.hub.compare.load(Ordering::Relaxed);
        let now = ui.input(|i| i.time);
        if !status.live {
            self.still_asked = f64::NEG_INFINITY;
        } else if !comparing && (self.still.is_none() || now - self.still_asked > STILL_EVERY) {
            self.hub.want_original.store(true, Ordering::Relaxed);
            self.still_asked = now;
        }
        (comparing, now)
    }

    fn program_strip(&mut self, ui: &mut Ui, bar: Rect, status: &Status) {
        let on_air = status.consumer.is_some() && !status.hidden;
        self.monitor_bar(ui, bar, "Program", |_, ui, text_room| {
            let p = ui.painter();
            let at = Pos2::new(bar.right(), bar.top() + 10.0);
            let (state, ink) = match (status.hidden, on_air) {
                (true, _) => ("Hidden", INK),
                (false, true) => ("On Air", INK),
                (false, false) => ("Off Air", FAINT),
            };
            let used = finish::label(p, at, Align2::RIGHT_TOP, state, LABEL, ink);
            let block = Rect::from_center_size(
                Pos2::new(used.left() - 13.0, used.center().y),
                Vec2::splat(10.0),
            );
            finish::code(p, block, if on_air { RED } else { EDGE }, on_air);
            (
                ["What Apps Get", "To Apps"],
                text_room - used.width() - 30.0,
            )
        });
    }

    fn preview_glass(&self, ui: &Ui, preview_glass: Rect, status: &Status, live_original: bool) {
        let painter = ui.painter_at(preview_glass);
        let shown = self.still.as_ref().filter(|_| status.live);
        let state = shown.map_or_else(
            || {
                let state = if status.live {
                    "Starting"
                } else {
                    "No Picture"
                };
                corners(&painter, preview_glass);
                let title = format!("Preview · {state}");
                let at = preview_glass.center();
                finish::label(&painter, at, Align2::CENTER_CENTER, &title, LABEL, FAINT);
                state
            },
            |t| {
                let shown = fit(&painter, preview_glass, t);
                if lock(&self.hub.general).guide {
                    thirds(&painter, shown);
                }
                let plate = if live_original { "Live" } else { "Still" };
                super::plate(&painter, preview_glass, plate, live_original);
                plate
            },
        );
        readout(
            &ui.interact(preview_glass, Id::unique("preview glass"), Sense::hover()),
            || format!("Preview: {state}, the camera's own picture"),
        );
    }

    fn program_glass(&self, ui: &mut Ui, program_glass: Rect, status: &Status, on_air: bool) {
        let on_air = if on_air { "On Air" } else { "Off Air" };
        let program = ui.interact(program_glass, Id::unique("program glass"), Sense::hover());
        if status.hidden {
            let detail = "Apps get a dark card saying the camera is hidden. Live view is off so the camera cools.";
            readout(&program, || {
                format!("Program: {on_air}, Camera Hidden. {detail}")
            });
            if notice(
                ui,
                program_glass,
                "Camera Hidden",
                detail,
                Some("Show Camera"),
            ) {
                self.hub.set_hidden(false);
            }
            return;
        }

        if status.live && self.texture.is_some() {
            readout(&program, || format!("Program: {on_air}, what apps get"));
            self.picture(ui, program_glass, status);
        } else {
            let problem = status.problem.clone().unwrap_or_default();
            let (title, detail) = match (&status.camera, status.live, status.resting) {
                (_, _, true) => ("The Camera Is Cooling Down", problem),
                (Some(_), false, _) if status.paused => (
                    "Live View Paused",
                    format!(
                        "No app used the camera for {}, so live view stopped to save the T3's battery and keep it cool. It starts again when an app opens it.",
                        minutes(lock(&self.hub.general).pause_after)
                    ),
                ),
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
                if status.paused {
                    Some("Resume")
                } else {
                    status.resting.then_some("Try Again Now")
                },
            ) {
                self.send(Command::Retry);
            }
        }
    }

    fn scopes(&mut self, ui: &mut Ui, status: &Status, meter: Rect, card: Rect, now: f64) {
        // Readings left in the status after live view stops are stale.
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

    /// `extra` draws the right side and returns the line under the name, long and short,
    /// and the width left for it.
    fn monitor_bar(
        &mut self,
        ui: &mut Ui,
        bar: Rect,
        name: &str,
        extra: impl FnOnce(&mut Self, &mut Ui, f32) -> ([&'static str; 2], f32),
    ) {
        let (what, room) = extra(self, ui, bar.width());
        let p = ui.painter();
        finish::label(
            p,
            bar.left_top() + Vec2::new(0.0, 10.0),
            Align2::LEFT_TOP,
            name,
            LABEL,
            INK,
        );
        let font = FontId::proportional(12.0);
        let fits = |t: &str| {
            ui.fonts_mut(|f| f.layout_no_wrap(t.to_owned(), font.clone(), FAINT).size().x) <= room
        };
        // A cut-off word reads worse than none.
        let what = [what[0], what[1]]
            .into_iter()
            .find(|t| fits(t))
            .unwrap_or_default();
        ui.painter().text(
            bar.left_top() + Vec2::new(0.0, 25.0),
            Align2::LEFT_TOP,
            what,
            font,
            FAINT,
        );
    }

    fn chain(&mut self, ui: &Ui, rect: Rect, status: &Status, look: &mut Look) {
        let link = if rect.width() < 640.0 { 6.0 } else { 12.0 };
        let w = ((rect.width() - link * 5.0) / 6.0).floor();
        let short = w < 100.0;
        let face = Face::fit(ui, w);
        let camera = camera_stage(status, short);
        let repair = switch(
            "Repair",
            REPAIR,
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
        let mut lit = [0.0; 6];
        for (i, ((s, r), slot)) in stages.iter().zip(&boxes).zip(&mut lit).enumerate() {
            let (response, t) = stage(ui, *r, s, face);
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

fn switch(name: &'static str, code: Color32, on: bool, detail: String, hint: &str) -> Stage {
    Stage {
        name,
        detail: if on { detail } else { "Off".into() },
        state: if on { State::On } else { State::Off },
        code,
        toggle: true,
        hint: hint.into(),
    }
}

fn fault(name: &'static str, detail: &str, hint: String, toggle: bool) -> Stage {
    Stage {
        name,
        detail: detail.into(),
        state: State::Fault,
        code: GPU,
        toggle,
        hint,
    }
}

fn says(short: bool, long: &str, brief: &str) -> String {
    if short {
        brief.to_owned()
    } else {
        long.to_owned()
    }
}

fn level(v: u32, names: &[(u32, &str)]) -> String {
    names
        .iter()
        .find(|n| n.0 == v)
        .map_or("Custom", |n| n.1)
        .to_owned()
}

fn camera_stage(status: &Status, short: bool) -> Stage {
    match (&status.camera, &status.problem) {
        (None, problem) => Stage {
            name: "Camera",
            detail: "Not Found".into(),
            state: State::Fault,
            code: DECODE,
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
                (false, false) if status.hidden => "Hidden".into(),
                (false, false) if status.paused => "Paused".into(),
                (false, false) => "Off".into(),
            },
            state: match (problem, status.live) {
                (Some(_), _) => State::Fault,
                (None, true) => State::On,
                (None, false) => State::Off,
            },
            code: DECODE,
            toggle: false,
            hint: problem
                .clone()
                .unwrap_or_else(|| format!("{model}, live view")),
        },
    }
}

fn gpu_stages(status: &Status, look: &Look, short: bool) -> [Stage; 3] {
    let gpu = status.vsr_available && status.vsr_problem.is_none();
    let denoise_levels = [(8, "Low"), (9, "Medium"), (10, "High"), (11, "Ultra")];
    let vsr_levels = [(2, "Fast"), (3, "High"), (4, "Ultra")];
    let unavailable = |name| {
        let why = status.vsr_problem.clone().unwrap_or_else(|| {
            "NVIDIA's Video Effects aren't running. Setup in General says why.".into()
        });
        fault(name, &says(short, "Unavailable", "No GPU"), why, false)
    };
    let (denoise, super_res) = if gpu {
        (
            switch(
                "Denoise",
                GPU,
                look.denoise > 0,
                level(look.denoise, &denoise_levels),
                "NVIDIA noise reduction, before upscaling",
            ),
            switch(
                "Super Res",
                GPU,
                look.vsr_quality > 0,
                level(look.vsr_quality, &vsr_levels),
                "NVIDIA super resolution",
            ),
        )
    } else {
        (unavailable("Denoise"), unavailable("Super Res"))
    };
    let smooth = match (gpu, look.smooth) {
        (false, _) => unavailable("Smooth"),
        (true, true) if look.vsr_quality == 0 => fault(
            "Smooth",
            &says(short, "Needs Super Res", "Needs SR"),
            "Smooth Motion needs Super Resolution on.".into(),
            true,
        ),
        (true, on) => switch(
            "Smooth",
            GPU,
            on,
            if status.smoothing {
                says(short, "Pacing 30 fps", "30 fps")
            } else {
                "Ready".into()
            },
            "Paces apps at an even 30 fps, showing the last frame again when the camera misses one. Adds about 70 ms of delay.",
        ),
    };
    [denoise, super_res, smooth]
}

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
            code: OUTPUT,
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
            code: OUTPUT,
            toggle: false,
            hint: consumer.map_or_else(
                || "No app is using the camera".into(),
                |f| format!("An app is taking {}×{}", f.width, f.height),
            ),
        },
    }
}

fn corners(p: &Painter, rect: Rect) {
    let safe = rect.shrink2(rect.size() * 0.08);
    let arm = (rect.width() * 0.04).clamp(6.0, 12.0);
    let stroke = Stroke::new(finish::hair(p), EDGE);
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

/// The detail shrinks or goes to fit, since the readout still carries it.
fn notice(ui: &mut Ui, rect: Rect, title: &str, detail: &str, button: Option<&str>) -> bool {
    corners(ui.painter(), rect);
    let room = rect
        .shrink2(rect.size() * 0.08)
        .shrink2(Vec2::new(10.0, 4.0));
    let p = ui.painter();
    let mut heading = finish::label_job(title, LABEL, INK);
    (heading.wrap.max_width, heading.halign) = (room.width(), Align::Center);
    let heading = p.layout_job(heading);
    let button_h = if button.is_some() { 16.0 + 32.0 } else { 0.0 };
    let lines = [13.0, 12.0]
        .into_iter()
        .filter(|_| !detail.is_empty())
        .map(|size| {
            let mut job = egui::text::LayoutJob::single_section(
                detail.to_owned(),
                TextFormat {
                    font_id: FontId::proportional(size),
                    color: DIM,
                    line_height: Some((size * 1.4).round()),
                    ..Default::default()
                },
            );
            (job.wrap.max_width, job.halign) = (room.width(), Align::Center);
            p.layout_job(job)
        })
        .find(|g| heading.size().y + 10.0 + g.size().y + button_h <= room.height());
    let h = heading.size().y + lines.as_ref().map_or(0.0, |g| 10.0 + g.size().y) + button_h;
    let mut y = (room.center().y - h / 2.0).max(room.top()).round();
    y += heading.size().y;
    p.galley(
        Pos2::new(room.center().x, y - heading.size().y),
        heading,
        INK,
    );
    if let Some(g) = lines {
        y += 10.0;
        p.galley(Pos2::new(room.center().x, y), g.clone(), DIM);
        y += g.size().y;
    }
    let Some(text) = button else { return false };
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_size(
                Pos2::new(room.left(), y + 16.0),
                Vec2::new(room.width(), 32.0),
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

pub(super) fn fit(painter: &Painter, rect: Rect, t: &TextureHandle) -> Rect {
    let [w, h] = t.size();
    let scale = (rect.width() / w as f32).min(rect.height() / h as f32);
    let shown = Rect::from_center_size(rect.center(), Vec2::new(w as f32, h as f32) * scale);
    painter.image(
        t.id(),
        shown,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
    shown
}

fn thirds(p: &Painter, rect: Rect) {
    let stroke = Stroke::new(finish::hair(p), FAINT);
    for third in [1.0, 2.0] {
        let x = p.round_to_pixel_center(rect.left() + rect.width() * third / 3.0);
        let y = p.round_to_pixel_center(rect.top() + rect.height() * third / 3.0);
        p.vline(x, rect.y_range(), stroke);
        p.hline(rect.x_range(), y, stroke);
    }
}

fn minutes(n: u32) -> String {
    if n == 1 {
        "a minute".into()
    } else {
        format!("{n} minutes")
    }
}

#[derive(Clone, Copy)]
struct Face {
    beside: bool,
    tracking: f32,
    short: bool,
}

impl Face {
    const BLOCK: f32 = 16.0;

    const fn pad(self) -> f32 {
        if self.beside { 10.0 } else { 8.0 }
    }

    fn name(self, name: &'static str) -> &'static str {
        if self.short && name == "Super Res" {
            "SR"
        } else {
            name
        }
    }

    fn job(self, name: &'static str, ink: Color32) -> egui::text::LayoutJob {
        let mut job = finish::label_job(self.name(name), LABEL, ink);
        if let Some(section) = job.sections.first_mut() {
            section.format.extra_letter_spacing = LABEL * self.tracking;
        }
        job
    }

    fn fit(ui: &Ui, w: f32) -> Self {
        let names = [
            "Camera",
            "Repair",
            "Denoise",
            "Super Res",
            "Smooth",
            "Output",
        ];
        let mut tries = [(true, false), (false, false), (false, true)]
            .into_iter()
            .flat_map(|(beside, short)| {
                [0.12, 0.06, 0.0].map(|tracking| Self {
                    beside,
                    tracking,
                    short,
                })
            });
        let fits = |face: &Self| {
            let room = face.pad().mul_add(-2.0, w) - if face.beside { Self::BLOCK } else { 0.0 };
            names.iter().all(|n| {
                let g = ui.fonts_mut(|f| f.layout_job(face.job(n, INK)));
                g.size().x <= room
            })
        };
        tries.find(fits).unwrap_or(Self {
            beside: false,
            tracking: 0.0,
            short: true,
        })
    }
}

fn stage(ui: &Ui, rect: Rect, s: &Stage, face: Face) -> (Response, f32) {
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
    let painter = ui.painter();
    let t = finish::lit(painter, r.id, s.state == State::On);
    let hot = s.toggle && (r.hovered() || r.has_focus());
    if s.toggle && r.is_pointer_button_down_on() {
        painter.rect_filled(rect, 0, PRESSED_SHEET);
    } else if hot {
        painter.rect_filled(rect, 0, HOVER_SHEET);
    }
    let (edge, ink) = match s.state {
        State::Fault => (WARN, WARN),
        _ => (
            if hot { INK } else { EDGE.lerp_to_gamma(INK, t) },
            FAINT.lerp_to_gamma(INK, t),
        ),
    };
    finish::outline(painter, rect, edge);
    if r.has_focus() {
        finish::focus_ring(painter, rect);
    }
    stage_face(ui, rect, s, face, ink);
    (r, t)
}

fn stage_face(ui: &Ui, rect: Rect, s: &Stage, face: Face, ink: Color32) {
    let painter = ui.painter();
    let pad = face.pad();
    let inset = if face.beside { Face::BLOCK } else { 0.0 };
    let head = rect.left_top() + Vec2::new(pad, if face.beside { 12.0 } else { 19.0 });
    let block = if face.beside {
        Rect::from_min_size(head + Vec2::new(0.0, 1.0), Vec2::splat(10.0))
    } else {
        Rect::from_min_size(
            Pos2::new(rect.right() - pad - 10.0, rect.top() + 7.0),
            Vec2::splat(10.0),
        )
    };
    match s.state {
        State::Fault => finish::warning(painter, block.center(), WARN),
        State::On => finish::code(painter, block, s.code, true),
        State::Off => finish::code(painter, block, UNKNOWN, false),
    }
    let galley = painter.layout_job(face.job(s.name, ink));
    finish::place(
        painter,
        head + Vec2::new(inset, 0.0),
        Align2::LEFT_TOP,
        galley,
        ink,
    );
    let room = pad.mul_add(-2.0, rect.width());
    let fits = |size: f32| {
        ui.fonts_mut(|f| {
            f.layout_no_wrap(s.detail.clone(), FontId::proportional(size), INK)
                .size()
                .x
        }) <= room
    };
    let size = if fits(13.0) { 13.0 } else { 12.0 };
    let detail = clipped(ui, &s.detail, FontId::proportional(size), ink, room);
    painter.galley(
        Align2::LEFT_BOTTOM
            .anchor_size(rect.left_bottom() + Vec2::new(pad, -9.0), detail.size())
            .min,
        detail,
        ink,
    );
}

#[expect(
    clippy::manual_midpoint,
    reason = "midpoint could round differently, and the drawing must stay the same"
)]
fn connector(p: &Painter, from: f32, to: f32, y: f32, cut: bool, t: f32) {
    let y = p.round_to_pixel_center(y);
    let hair = finish::hair(p);
    let (left, right) = (Pos2::new(from, y), Pos2::new(to, y));
    if cut {
        let mid = (from + to) / 2.0;
        let stroke = Stroke::new(1.4, WARN);
        p.line_segment([left, Pos2::new(mid - 3.0, y)], Stroke::new(hair, WARN));
        p.line_segment([Pos2::new(mid + 3.0, y), right], Stroke::new(hair, WARN));
        p.line_segment(
            [Pos2::new(mid - 2.0, y + 5.0), Pos2::new(mid + 2.0, y - 5.0)],
            stroke,
        );
        return;
    }
    p.line_segment([left, right], Stroke::new(hair, EDGE.lerp_to_gamma(INK, t)));
}

fn card_frame(p: &Painter, rect: Rect, title: &str) -> Rect {
    finish::outline(p, rect, EDGE);
    finish::label(
        p,
        rect.left_top() + Vec2::new(16.0, 14.0),
        Align2::LEFT_TOP,
        title,
        LABEL,
        INK,
    );
    let y = p.round_to_pixel_center(rect.top() + 36.0);
    p.hline(
        rect.x_range().shrink(16.0),
        y,
        Stroke::new(finish::hair(p), RULE),
    );
    Rect::from_min_max(
        rect.left_top() + Vec2::new(16.0, 48.0),
        rect.right_bottom() - Vec2::new(16.0, 16.0),
    )
}

type Item = (Arc<Galley>, Color32);

fn budget(ui: &Ui, rect: Rect, stages: [f32; 4], total: f32, peak: f32) {
    readout(
        &ui.interact(rect, Id::unique("frame budget"), Sense::hover()),
        || budget_words(stages, total),
    );
    let p = ui.painter();
    let inner = card_frame(p, rect, "Frame Budget");
    let heading = p.layout_job(finish::label_job("Frame Budget", LABEL, INK));
    let room = inner.width() - heading.size().x - 12.0;
    if let Some(g) = [
        format!("{BUDGET_MS:.1} ms at 30 fps"),
        format!("{BUDGET_MS:.1} ms"),
    ]
    .into_iter()
    .map(|t| p.layout_no_wrap(t, FontId::proportional(12.0), FAINT))
    .find(|g| g.size().x <= room)
    {
        p.galley(
            Pos2::new(inner.right() - g.size().x, rect.top() + 13.0),
            g,
            FAINT,
        );
    }

    let items: Vec<Item> = stages
        .iter()
        .zip(STAGES)
        .filter(|(ms, _)| **ms >= 0.05)
        .map(|(ms, (name, code))| {
            let mut job = finish::label_job(name, LABEL, DIM);
            let value = TextFormat {
                font_id: FontId::proportional(12.0),
                color: INK,
                ..Default::default()
            };
            job.append(&format!("{ms:.1}"), 5.0, value);
            (p.layout_job(job), code)
        })
        .collect();
    let rows = legend_rows(&items, inner.width());
    let legend_h = if items.is_empty() {
        0.0
    } else {
        rows.len() as f32 * 17.0
    };
    let reading_h = 26.0;
    let meter_h = 12.0;
    let scale_h = 16.0;
    let show_scale = inner.height() - (reading_h + meter_h + scale_h + legend_h) >= 10.0;
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
    meter_run(p, meter, stages, total, peak);
    y += meter_h;
    if show_scale {
        scale(p, meter, y);
    }

    let mut y = inner.bottom() - legend_h;
    for row in rows {
        let mut x = inner.left();
        for (item, code) in row {
            let size = item.size();
            let block = Rect::from_min_size(Pos2::new(x, y + size.y / 2.0 - 4.5), Vec2::splat(9.0));
            p.rect_filled(block, 0, code);
            p.galley(Pos2::new(block.right() + 6.0, y), item, INK);
            x = block.right() + 6.0 + size.x + 12.0;
        }
        y += 17.0;
    }
}

fn usage_strip(ui: &Ui, rect: Rect) {
    let Some(meters) = lock(&usage::LATEST).as_ref().map(usage::Usage::meters) else {
        return;
    };
    let gap = 16.0;
    let w = ((rect.width() - gap * 2.0) / 3.0).floor();
    let p = ui.painter();
    let font = FontId::proportional(12.0);
    for (i, m) in meters.iter().enumerate() {
        let slot = Rect::from_min_size(
            rect.min + Vec2::new(i as f32 * (w + gap), 0.0),
            Vec2::new(w, rect.height()),
        );
        readout(
            &ui.interact(slot, Id::unique(("usage", i)), Sense::hover()),
            || format!("{}, {}", m.total, m.own),
        );
        let words = |own: &str| {
            let mut job = egui::text::LayoutJob::default();
            let ink = |color| TextFormat {
                font_id: font.clone(),
                color,
                ..Default::default()
            };
            job.append(&m.total, 0.0, ink(DIM));
            if !own.is_empty() {
                job.append(&format!(", {own}"), 0.0, ink(INK));
            }
            p.layout_job(job)
        };
        let fits = [&m.own, m.own.trim_start_matches("Viewfinder "), ""]
            .into_iter()
            .map(words)
            .find(|g| g.size().x <= w)
            .unwrap_or_else(|| words(""));
        p.galley(slot.min, fits, INK);
        let track = slot.with_min_y(slot.bottom() - 4.0);
        p.rect_filled(track, 0, ROW_RULE);
        for (k, (fill, colour)) in [(m.fill, EDGE), (m.own_fill, INK)].into_iter().enumerate() {
            let fill = glide(
                ui.ctx(),
                Id::unique(("usage fill", i, k)),
                fill.clamp(0.0, 1.0),
            );
            let run = track.with_max_x(egui::lerp(track.left()..=track.right(), fill));
            p.rect_filled(run, 0, colour);
        }
    }
}

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

/// Glides over one reading, so a usage bar climbs and falls instead of stepping. egui's own
/// animation would redraw at the monitor's rate, and with a reading every 125 ms it would never
/// stop, so this asks for 30 fps. With Windows animations off it jumps.
fn glide(ctx: &egui::Context, id: Id, target: f32) -> f32 {
    const SECONDS: f64 = 0.125;
    if ctx.global_style().animation_time <= 0.0 {
        return target;
    }
    let now = ctx.input(|i| i.time);
    let at = |(from, to, start): (f32, f32, f64)| {
        egui::lerp(from..=to, ((now - start) / SECONDS).clamp(0.0, 1.0) as f32)
    };
    let run = ctx.data_mut(|d| {
        let run = d.get_temp_mut_or_insert_with(id, || (target, target, now));
        if (run.1 - target).abs() > f32::EPSILON {
            *run = (at(*run), target, now);
        }
        *run
    });
    if now - run.2 < SECONDS {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(1.0 / 30.0));
    }
    at(run)
}

/// As few rows as fit, then the items shared out evenly, so the last row isn't one orphan.
fn legend_rows(items: &[Item], width: f32) -> Vec<Vec<Item>> {
    let item_w = |it: &Item| 15.0 + it.0.size().x + 12.0;
    let greedy = greedy_rows(items, width, item_w);
    let per = items.len().div_ceil(greedy.len().max(1)).max(1);
    let even: Vec<Vec<Item>> = items.chunks(per).map(<[Item]>::to_vec).collect();
    let fits = |row: &Vec<Item>| row.iter().map(item_w).sum::<f32>() <= width + 12.0;
    if even.len() == greedy.len() && even.iter().all(fits) {
        even
    } else {
        greedy
    }
}

fn greedy_rows(items: &[Item], width: f32, item_w: impl Fn(&Item) -> f32) -> Vec<Vec<Item>> {
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

fn reading(p: &Painter, inner: Rect, y: f32, reading_h: f32, total: f32) {
    let small = |t: &str| p.layout_no_wrap(t.to_owned(), FontId::proportional(12.0), FAINT);
    let big = p.layout_no_wrap(
        format!("{total:.1}"),
        FontId::new(26.0, finish::number_family()),
        INK,
    );
    let (spare, ink, word) = if total > BUDGET_MS {
        (total - BUDGET_MS, WARN, "over")
    } else {
        (BUDGET_MS - total, INK, "spare")
    };
    let figure = p.layout_no_wrap(
        format!("{spare:.1} ms"),
        FontId::new(17.0, finish::number_family()),
        ink,
    );
    let word = small(word);
    let right = figure.size().x + 6.0 + word.size().x;
    let unit = ["ms per frame", "ms"]
        .map(small)
        .into_iter()
        .find(|g| big.size().x + 6.0 + g.size().x + 16.0 + right <= inner.width())
        .unwrap_or_else(|| small("ms"));
    let base = y + reading_h;
    let at = |g: &Galley, x: f32, lift: f32| Pos2::new(x, base - lift - g.size().y);
    p.galley(at(&big, inner.left(), 0.0), big.clone(), INK);
    p.galley(
        at(&unit, inner.left() + big.size().x + 6.0, 4.0),
        unit,
        FAINT,
    );
    let words_x = inner.right() - word.size().x;
    p.galley(at(&word, words_x, 4.0), word, FAINT);
    p.galley(
        at(&figure, words_x - 6.0 - figure.size().x, 1.0),
        figure,
        ink,
    );
}

fn meter_x(meter: Rect, t: f32) -> f32 {
    meter.left() + meter.width() * t.clamp(0.0, METER_MS) / METER_MS
}

fn scale(p: &Painter, meter: Rect, y: f32) {
    let top = METER_MS as u32;
    for t in (0..=top).step_by(10) {
        let align = match t {
            0 => Align2::LEFT_TOP,
            _ if t == top => Align2::RIGHT_TOP,
            _ => Align2::CENTER_TOP,
        };
        let text = if t == top && meter.width() >= 280.0 {
            format!("{t} ms")
        } else {
            t.to_string()
        };
        finish::label(
            p,
            Pos2::new(meter_x(meter, t as f32), y + 4.0),
            align,
            &text,
            LABEL,
            FAINT,
        );
    }
}

/// Without a split only the total was measured, so the run is one grey.
fn meter_run(p: &Painter, rect: Rect, stages: [f32; 4], total: f32, peak: f32) {
    p.rect_filled(rect, 0, MAT);
    finish::outline(p, rect, RULE);
    let run = rect.shrink(2.0);
    let gap = finish::hair(p);
    let segment = |from: f32, to: f32, colour: Color32| {
        let (a, b) = (meter_x(run, from), meter_x(run, to.min(BUDGET_MS)));
        if b > a {
            let r = Rect::from_min_max(Pos2::new(a, run.top()), Pos2::new(b, run.bottom()));
            p.rect_filled(r, 0, colour);
        }
    };
    if stages.iter().sum::<f32>() > 0.0 {
        let mut start = 0.0;
        for (&ms, (_, code)) in stages.iter().zip(STAGES) {
            if ms >= 0.05 {
                segment(start, start + ms - gap * METER_MS / run.width(), code);
            }
            start += ms;
        }
    } else if total > 0.0 {
        segment(0.0, total, UNKNOWN);
    }
    if total > BUDGET_MS {
        let r = Rect::from_min_max(
            Pos2::new(meter_x(run, BUDGET_MS), run.top()),
            Pos2::new(meter_x(run, total), run.bottom()),
        );
        p.rect_filled(r, 0, WARN);
    }
    if peak > total {
        let x = p.round_to_pixel_center(meter_x(run, peak));
        p.vline(x, run.y_range(), Stroke::new(2.0, INK));
    }
    let x = p.round_to_pixel_center(meter_x(run, BUDGET_MS));
    p.vline(
        x,
        rect.y_range().expand(3.0),
        Stroke::new(gap.max(1.0), INK),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use std::sync::atomic::Ordering;

    use eframe::egui::{Event, Key, Modifiers, PointerButton, TextureHandle, Vec2};
    use egui_kittest::kittest::Queryable;
    use shared::{Format, NV12};

    use super::super::{App, style};
    use crate::pipeline::Look;
    use crate::worker::{Hub, lock};

    fn frame(hub: &Hub, original: bool) {
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
        if original {
            (p.still_size, p.still, p.still_fresh) = ((width, height), rgba, true);
        } else {
            (p.width, p.height, p.rgba, p.fresh, p.histogram) =
                (width, height, rgba, true, vec![1; 64]);
        }
    }

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
            hub.want_original.load(Ordering::Relaxed),
            "Preview asks for a still as live view starts"
        );

        harness.key_down(Key::Backslash);
        harness.run();
        frame(&hub, true);
        harness.run();
        assert_eq!(
            harness.state().texture.as_ref().map(TextureHandle::id),
            Some(processed),
            "Program stays on the processed picture"
        );
        assert!(harness.state().still.is_some(), "Preview has the original");

        harness.key_up(Key::Backslash);
        harness.run();
        assert!(
            harness.state().still.is_some(),
            "the original is kept as a still"
        );
    }

    #[test]
    fn compare_stays_held_past_a_click() {
        let hub = Hub::new(Look::default());
        lock(&hub.status).live = true;
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
            .with_step_dt(0.1)
            .with_context_setup(style)
            .build_ui_state(|ui, app: &mut App| app.draw(ui), app);
        harness.run();
        let at = harness.get_by_label("Hold to Compare").rect().center();
        harness.hover_at(at);
        harness.event(Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        });
        for _ in 0..30 {
            harness.step();
        }
        assert!(hub.compare.load(Ordering::Relaxed), "still held after 3 s");
    }

    #[test]
    fn usage_bars_glide_unless_animations_are_off() {
        let ctx = eframe::egui::Context::default();
        let id = eframe::egui::Id::unique("bar");
        let mut seen = Vec::new();
        for (time, target) in [(0.0, 0.2), (1.0, 0.8), (1.0625, 0.8), (1.5, 0.8)] {
            let input = eframe::egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            drop(ctx.run_ui(input, |ui| seen.push(super::glide(ui.ctx(), id, target))));
        }
        assert_eq!(seen, [0.2, 0.2, 0.5, 0.8]);
        ctx.all_styles_mut(|s| s.animation_time = 0.0);
        drop(ctx.run_ui(eframe::egui::RawInput::default(), |ui| {
            assert!((super::glide(ui.ctx(), id, 0.1) - 0.1).abs() < f32::EPSILON);
        }));
    }
}
