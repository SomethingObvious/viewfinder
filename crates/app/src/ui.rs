use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use canon::{Prop, values};
use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, Context, EventFilter, FontData, FontDefinitions,
    FontFamily, FontId, Galley, Key, Layout, Pos2, Rect, Response, RichText, Role, Sense, Shape,
    Stroke, StrokeKind, TextStyle, TextureHandle, TextureOptions, Ui, UiBuilder, Vec2,
    ViewportCommand, WidgetInfo,
    text::{LayoutJob, TextWrapping},
};
use windows::Win32::Foundation::HWND;

use crate::adjust::{self, Adjust};
use crate::exposure;
use crate::focus;
use crate::general::{self, Close};
use crate::pipeline::Look;
use crate::worker::{Command, Hub, Status, lock};

/// The mode dial on M, and the lens switch on MF, as the body reports them.
const MODE_M: u32 = 3;
const FOCUS_MF: u32 = 3;
/// White balance set to a colour temperature in Kelvin.
const WB_KELVIN: u32 = 9;

mod card;
mod chrome;
pub(crate) mod demo;
mod finish;
mod switcher;

// Keepsake's inks, on a neutral ground so the picture's colour is judged against nothing.
const GROUND: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0a);
/// The recess behind pictures.
const MAT: Color32 = Color32::from_rgb(0x05, 0x05, 0x05);
const HOVER_SHEET: Color32 = Color32::from_rgb(0x12, 0x12, 0x12);
const PRESSED_SHEET: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
const RULE: Color32 = Color32::from_rgb(0x2f, 0x2f, 0x2e);
const ROW_RULE: Color32 = Color32::from_rgb(0x1d, 0x1d, 0x1c);
/// The outline of every control, at 3:1 against the ground.
const EDGE: Color32 = Color32::from_rgb(0x62, 0x61, 0x5d);
/// Plot Ink, and two steps down from it.
const INK: Color32 = Color32::from_rgb(0xec, 0xeb, 0xe6);
const DIM: Color32 = Color32::from_rgb(0xae, 0xad, 0xa7);
const FAINT: Color32 = Color32::from_rgb(0x8d, 0x8c, 0x86);
/// The one item in hand: on air.
const RED: Color32 = Color32::from_rgb(0xd8, 0x32, 0x2b);
/// Failure text, light enough to read at body size.
const WARN: Color32 = Color32::from_rgb(0xf2, 0x66, 0x5b);
/// The colour code of the timed stages, always beside their names.
const DECODE: Color32 = Color32::from_rgb(0x2f, 0x66, 0xd8);
const REPAIR: Color32 = Color32::from_rgb(0x3c, 0x9a, 0x5f);
const GPU: Color32 = Color32::from_rgb(0xe2, 0xb0, 0x0e);
const OUTPUT: Color32 = INK;
/// Off, or not known.
const UNKNOWN: Color32 = Color32::from_rgb(0x6b, 0x6a, 0x66);
/// Label type's size. Capitals never go smaller.
const LABEL: f32 = 11.0;
/// The smallest window the layout is made for, in points. main.rs sets the same at zoom 1.
pub(crate) const MIN_SIZE: Vec2 = Vec2::new(860.0, 520.0);
/// How long Reset All Settings waits for its second press, in seconds.
const ARMED_FOR: f64 = 3.5;

pub(crate) use card::hidden_card;

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about the window, not states of one machine"
)]
pub(crate) struct App {
    hub: Arc<Hub>,
    commands: Sender<Command>,
    quitting: Arc<AtomicBool>,
    texture: Option<TextureHandle>,
    /// The camera's own picture for Preview, from the last Compare or `Hub::want_original`.
    still: Option<TextureHandle>,
    /// When a fresh still was last asked for, in egui's time.
    still_asked: f64,
    /// The preview's pixels, shared with the painter until it has uploaded them.
    image: Arc<ColorImage>,
    histogram: Vec<u32>,
    saved_look: Look,
    saved_exposure: exposure::Prefs,
    saved_adjust: Adjust,
    /// Off in the demo, so trying its controls never touches the real saved settings.
    persist: bool,
    /// None in the demo and the tests, which show made-up cameras.
    cameras: Option<Vec<capture::Choice>>,
    /// `capture::changes` when the list was last made.
    listed: Option<u64>,
    /// A list being made off the UI thread, since Windows can be slow over it.
    listing: Option<Receiver<Vec<capture::Choice>>>,
    rounded: bool,
    /// The still's pixels, kept like `image` so a fresh still is copied in place.
    still_image: Arc<ColorImage>,
    bus: switcher::Bus,
    /// When Windows' settings were last read, in egui's time.
    system_checked: f64,
    /// The Text size last applied, and the zoom the minimum window size was last set for.
    text_scale: f32,
    min_zoom: f32,
    /// The tray's Hide Camera item and icon, from main. None in the demo and the tests.
    pub(crate) tray: Option<crate::Tray>,
    /// Whether the tray's item says Show Camera.
    tray_says_show: bool,
    saved_general: general::Prefs,
    /// Whether the Run value is there.
    starts_with_windows: bool,
    /// When Reset All Settings was pressed once, in egui's time.
    reset_armed: Option<f64>,
    /// The battery warning last given: 0 for none, 1 at 25% and 2 when almost empty.
    battery_warned: u8,
    /// Keep on Top as the window last heard it.
    on_top: Option<bool>,
}

impl App {
    pub(crate) fn new(
        cc: &eframe::CreationContext<'_>,
        hub: Arc<Hub>,
        commands: Sender<Command>,
        quitting: Arc<AtomicBool>,
        visible: bool,
    ) -> Self {
        let mut app = Self::with_context(&cc.egui_ctx, hub, commands, quitting, visible);
        app.cameras = Some(Vec::new());
        app.persist = true;
        app.starts_with_windows = general::starts_with_windows(general::RUN_KEY);
        app.on_top = Some(app.saved_general.on_top);
        app
    }

    pub(crate) fn with_context(
        ctx: &Context,
        hub: Arc<Hub>,
        commands: Sender<Command>,
        quitting: Arc<AtomicBool>,
        visible: bool,
    ) -> Self {
        style(ctx);
        let c = ctx.clone();
        hub.on_repaint(move || c.request_repaint());
        hub.preview_wanted.store(visible, Ordering::Relaxed);
        let saved_look = *lock(&hub.look);
        let saved_exposure = *lock(&hub.exposure);
        let saved_adjust = *lock(&hub.adjust);
        let saved_general = *lock(&hub.general);
        Self {
            hub,
            commands,
            quitting,
            texture: None,
            still: None,
            still_asked: f64::NEG_INFINITY,
            image: Arc::default(),
            histogram: vec![0; 64],
            saved_look,
            saved_exposure,
            saved_adjust,
            persist: false,
            cameras: None,
            listed: None,
            listing: None,
            rounded: false,
            still_image: Arc::default(),
            bus: switcher::Bus::default(),
            system_checked: f64::NEG_INFINITY,
            text_scale: 1.0,
            min_zoom: 1.0,
            tray: None,
            tray_says_show: false,
            saved_general,
            starts_with_windows: false,
            reset_armed: None,
            battery_warned: 0,
            on_top: None,
        }
    }

    fn send(&self, cmd: Command) {
        self.commands.send(cmd).ok();
    }

    /// Split from `eframe::App` so it can be rendered offscreen.
    pub(crate) fn draw(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        {
            let mut p = lock(&self.hub.preview);
            if p.fresh && p.width > 0 {
                p.fresh = false;
                upload(
                    &ctx,
                    "preview",
                    &mut self.texture,
                    &mut self.image,
                    [p.width, p.height],
                    &p.rgba,
                );
                self.histogram.clone_from(&p.histogram);
            }
            if p.still_fresh {
                p.still_fresh = false;
                upload(
                    &ctx,
                    "still",
                    &mut self.still,
                    &mut self.still_image,
                    [p.still_size.0, p.still_size.1],
                    &p.still,
                );
            }
        }
        self.list_cameras(&ctx);
        let status = lock(&self.hub.status).clone();
        let mut look = *lock(&self.hub.look);

        self.switcher(ui, &status, &mut look);

        *lock(&self.hub.look) = look;
        if !self.persist || ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        if look != self.saved_look {
            crate::save_look(look);
            self.saved_look = look;
        }
        let prefs = *lock(&self.hub.exposure);
        if prefs != self.saved_exposure {
            exposure::save(prefs);
            self.saved_exposure = prefs;
        }
        let adjust = *lock(&self.hub.adjust);
        if adjust != self.saved_adjust {
            adjust::save(adjust);
            self.saved_adjust = adjust;
        }
        let general = *lock(&self.hub.general);
        if general != self.saved_general {
            general::save(general);
            self.saved_general = general;
        }
    }

    /// Lists the cameras when the window opens, and again whenever a device comes or goes.
    fn list_cameras(&mut self, ctx: &Context) {
        if let Some(rx) = &self.listing {
            match rx.try_recv() {
                Ok(found) => {
                    self.cameras = Some(found);
                    self.listing = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.listing = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
            }
        }
        let changes = capture::changes();
        if self.cameras.is_none() || self.listed == Some(changes) {
            return;
        }
        self.listed = Some(changes);
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            tx.send(capture::list()).ok();
            ctx.request_repaint();
        });
        self.listing = Some(rx);
    }

    /// Which camera to use. The chosen one stays listed while it's unplugged, and Viewfinder
    /// waits for it rather than using another.
    fn camera_row(&self, ui: &mut Ui, status: &Status) {
        let chosen = lock(&self.hub.chosen).clone();
        let cameras = self.cameras.as_deref().unwrap_or_default();
        let at = chosen.find(cameras);
        let shown = at
            .and_then(|i| cameras.get(i))
            .map(|c| c.label().to_owned())
            .or_else(|| status.camera.clone())
            .unwrap_or_else(|| format!("{} (Not Connected)", chosen.label()));
        let mut picked = None;
        row(ui, "Camera", |ui| {
            // Wider than the other fields, since camera names run long.
            let r = egui::ComboBox::from_id_salt("Camera")
                .width(210.0)
                .truncate()
                .icon(chevron)
                .selected_text(&shown)
                .show_ui(ui, |ui| {
                    menu(ui);
                    if at.is_none() {
                        ui.add(egui::Button::selectable(true, &shown));
                    }
                    for (i, c) in cameras.iter().enumerate() {
                        if ui.selectable_label(at == Some(i), c.label()).clicked() && at != Some(i)
                        {
                            picked = Some(c.clone());
                        }
                    }
                })
                .response;
            edge(ui, &r);
            r
        });
        if let Some(c) = picked {
            if self.persist {
                crate::selection::save(&c);
            }
            *lock(&self.hub.chosen) = c;
            self.send(Command::Retry);
        }
    }

    /// Hide Camera, which isn't saved, so Viewfinder always starts with the camera shown.
    fn hide_row(&self, ui: &mut Ui, status: &Status) {
        let mut hidden = status.hidden;
        row(ui, "Hide Camera", |ui| {
            segmented(ui, &mut hidden, &[(false, "Off"), (true, "On")])
        });
        if hidden != status.hidden {
            self.hub.set_hidden(hidden);
        }
        if hidden {
            note(
                ui,
                "Apps get a dark card saying the camera is hidden, and live view is off so the camera cools. Viewfinder always starts with the camera shown.",
                FAINT,
            );
        }
    }

    /// The mark and name, the camera and battery up to `column`, where the settings start,
    /// and the tally beside the window's buttons.
    fn top_bar(ui: &Ui, bar: Rect, column: f32, status: &Status) {
        // The middle of the pixels above the rule, which lands on the pixel row below the bar.
        let ppp = ui.ctx().pixels_per_point();
        let mid = f32::midpoint((bar.top() * ppp).round(), (bar.bottom() * ppp).floor()) / ppp;
        let x = mark(ui.painter(), bar.left() + 16.0, mid) + 12.0;
        let mut x = name(ui, x, mid) + 24.0;
        // A readout with nothing to read is left out, not shown empty.
        let camera = status
            .camera
            .as_deref()
            .map(|m| m.trim_start_matches("Canon "));
        let battery = status.battery.map(values::battery_label);
        let room = column - 12.0 - x;
        // Battery goes last, since it's the one worth knowing before a call.
        let battery_w = if battery.is_some() { 92.0 } else { 0.0 };
        let block =
            |x: f32, w: f32| Rect::from_min_size(Pos2::new(x, mid - 16.0), Vec2::new(w, 32.0));
        if let Some(camera) = camera
            && room >= battery_w + 170.0
        {
            stat(ui, block(x, 170.0), "Camera", camera);
            x += 170.0;
        }
        if let Some(battery) = &battery
            && room >= battery_w
        {
            stat(ui, block(x, battery_w), "Battery", battery);
        }
        let w = tally_width(ui, status);
        tally(
            ui,
            block(bar.right() - chrome::KEYS_WIDTH - 16.0 - w, w),
            status,
        );
    }

    /// Hold to see the camera's own picture, as Lightroom's backslash does. Holding Space while
    /// the key has focus does the same. Returns how wide the key came out.
    fn compare_button(&self, ui: &mut Ui, live: bool) -> f32 {
        if !live {
            ui.disable();
        }
        let key = live && ui.ctx().input(|i| i.key_down(Key::Backslash));
        let was = self.hub.compare.load(Ordering::Relaxed);
        // The whole name and its shortcut where there's room beside the monitor's name, then
        // the name alone, then just Compare.
        let room = ui.available_width() - 84.0;
        let (shown, shortcut) = if room >= 205.0 {
            ("Hold to Compare", Some("\\"))
        } else if room >= 177.0 {
            ("Hold to Compare", None)
        } else {
            ("Compare", None)
        };
        let r = push_key(ui, "Hold to Compare", shown, was, false, shortcut).on_hover_text(
            "Shows the camera's picture before any processing. Backslash does the same.",
        );
        let held = r.is_pointer_button_down_on()
            || (r.has_focus() && ui.input(|i| i.key_down(Key::Space)));
        self.hub
            .compare
            .store(live && (held || key), Ordering::Relaxed);
        r.rect.width()
    }

    /// The live picture fitted into `rect`, with any problem written over it.
    fn picture(&self, ui: &Ui, rect: Rect, status: &Status) {
        let Some(t) = &self.texture else { return };
        let painter = ui.painter_at(rect);
        let shown = switcher::fit(&painter, rect, t);
        if let Some(problem) = &status.problem {
            let strip =
                Rect::from_min_max(Pos2::new(shown.left(), shown.bottom() - 30.0), shown.max);
            painter.rect_filled(strip, 0, GROUND);
            finish::warning(&painter, strip.left_center() + Vec2::new(16.0, 0.0), WARN);
            let text = clipped(
                ui,
                problem,
                FontId::proportional(13.0),
                INK,
                strip.width() - 44.0,
            );
            painter.galley(
                Align2::LEFT_CENTER
                    .anchor_size(strip.left_center() + Vec2::new(30.0, 0.0), text.size())
                    .min,
                text,
                INK,
            );
        }
    }

    fn exposure_section(&self, ui: &mut Ui, status: &Status) {
        if let Some(mode) = status.mode
            && !values::is_creative_mode(mode)
        {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "The mode dial is on {}. Turn it to P, Tv, Av or M to set exposure from here.",
                    values::ae_mode_label(mode)
                ))
                .color(WARN),
            );
        }
        section(ui, "Exposure");
        let mut prefs = *lock(&self.hub.exposure);
        row(ui, "Steady Exposure", |ui| {
            segmented(ui, &mut prefs.steady, &[(false, "Off"), (true, "On")])
        });
        let held = prefs.steady && status.mode == Some(MODE_M);
        if held {
            row(ui, "Brightness", |ui| {
                fader(ui, &mut prefs.brightness, -2.0..=2.0)
                    .on_hover_text("Up to 2 stops brighter or darker")
            });
        }
        if prefs.steady {
            let (text, colour) = match status.mode {
                Some(MODE_M) => (
                    "ISO and shutter are set for you and stay put until the light changes by about a stop.",
                    FAINT,
                ),
                Some(m) if values::is_creative_mode(m) => (
                    "Turn the mode dial to M to hold the exposure steady. On P, Tv and Av the camera meters again every moment.",
                    WARN,
                ),
                _ => ("", FAINT),
            };
            if !text.is_empty() {
                note(ui, text, colour);
            }
        }
        *lock(&self.hub.exposure) = prefs;
        for prop in [Prop::Iso, Prop::Shutter, Prop::Aperture, Prop::ExposureComp] {
            ui.add_enabled_ui(!(held && matches!(prop, Prop::Iso | Prop::Shutter)), |ui| {
                self.setting_row(ui, status, prop);
            });
        }
    }

    fn adjust_section(&self, ui: &mut Ui, status: &Status, look: &Look) {
        section(ui, "Adjust");
        let before = *lock(&self.hub.adjust);
        let mut adjust = before;
        for (&(name, hint, lo, hi), value) in adjust::SLIDERS.iter().zip(adjust.sliders()) {
            row(ui, name, |ui| fader(ui, value, lo..=hi).on_hover_text(hint));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let auto = ui
                .add_enabled_ui(status.live, |ui| primary(ui, "Auto"))
                .inner;
            if auto
                .on_hover_text("Sets the sliders once from the picture in view")
                .clicked()
            {
                self.hub.want_auto.store(true, Ordering::Relaxed);
            }
            if push_key(ui, "Reset", "Reset", false, false, None).clicked() {
                adjust = Adjust::default();
            }
        });
        if adjust != before {
            *lock(&self.hub.adjust) = adjust;
        }
        let gpu = status.vsr_available && status.vsr_problem.is_none() && look.vsr_quality > 0;
        let text = if gpu {
            "Auto looks at one frame, favouring your face, and leaves the sliders for you to fine tune. Vignette, Clarity and Fade stay as you set them."
        } else {
            "The Canon's picture is only adjusted with Super Resolution on."
        };
        ui.add_space(2.0);
        note(ui, text, if gpu { FAINT } else { WARN });
    }

    fn colour_and_focus(&self, ui: &mut Ui, status: &Status) {
        section(ui, "Colour");
        let mut prefs = *lock(&self.hub.exposure);
        row(ui, "Steady White Balance", |ui| {
            segmented(ui, &mut prefs.white, &[(false, "Off"), (true, "On")])
        });
        let creative = status.mode.is_some_and(values::is_creative_mode);
        if prefs.white {
            white_note(ui, status, creative);
            row(ui, "Warmth", |ui| {
                fader(ui, &mut prefs.warmth, -exposure::WARMTH..=exposure::WARMTH)
                    .on_hover_text("Leaves the picture cooler or warmer than neutral")
            });
        }
        ui.add_enabled_ui(!(prefs.white && creative), |ui| {
            self.setting_row(ui, status, Prop::WhiteBalance);
        });
        self.setting_row(ui, status, Prop::PictureStyle);
        *lock(&self.hub.exposure) = prefs;
        self.focus_section(ui, status);
    }

    fn focus_section(&self, ui: &mut Ui, status: &Status) {
        section(ui, "Focus");
        let mut prefs = *lock(&self.hub.exposure);
        let manual = status.focus_mode == Some(FOCUS_MF);
        if manual {
            note(
                ui,
                "The lens switch is on MF, so focus stays where the ring puts it. Switch it to AF to focus from here.",
                FAINT,
            );
        }
        row(ui, "Autofocus", |ui| {
            segmented(ui, &mut prefs.autofocus, &[(false, "Off"), (true, "On")])
        });
        if prefs.autofocus {
            row(ui, "Focus By", |ui| {
                segmented(
                    ui,
                    &mut prefs.camera_af,
                    &[(false, "Viewfinder"), (true, "Camera")],
                )
            });
            if prefs.camera_af {
                self.setting_row(ui, status, Prop::AfMethod);
            } else {
                row(ui, "Always Focus", |ui| {
                    segmented(ui, &mut prefs.hold_focus, &[(false, "Off"), (true, "On")])
                });
            }
        }
        row(ui, "Remember Focus", |ui| {
            segmented(
                ui,
                &mut prefs.remember_focus,
                &[(false, "Off"), (true, "On")],
            )
        });
        *lock(&self.hub.exposure) = prefs;
        if !manual {
            note(ui, focus_note(&prefs), FAINT);
            if prefs.remember_focus {
                note(
                    ui,
                    "When the camera comes back on, the lens goes back to where it last focused. It takes a few seconds.",
                    FAINT,
                );
            }
        }
        ui.add_space(6.0);
        // Autofocus in expanded capitals and six nudges don't fit the column on one line.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            if prefs.autofocus && primary(ui, "Autofocus").clicked() {
                self.send(Command::Autofocus);
            }
            let progress = match status.focus {
                _ if manual || prefs.camera_af => "",
                focus::Report::Idle => "",
                focus::Report::Searching => "Focusing",
                focus::Report::Focused => "In focus",
                focus::Report::Flat => "Nothing sharp to focus on",
                focus::Report::NoFace => "No face to focus on",
            };
            if !progress.is_empty() {
                ui.add(egui::Label::new(RichText::new(progress).small().color(FAINT)).truncate());
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| self.nudges(ui));
        // The column's end, so the last row clears the window's bottom edge when scrolled to.
        ui.add_space(18.0);
    }

    fn nudges(&self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing.x = 4.0;
        for step in [-3i8, -2, -1, 1, 2, 3] {
            let hint = match step {
                -3 => "Nearer, big step",
                -1 => "Nearer, fine step",
                -2 => "Nearer",
                1 => "Farther, fine step",
                2 => "Farther",
                _ => "Farther, big step",
            };
            if chevrons(ui, step, 30.0, hint)
                .on_hover_ui(|ui| drop(ui.label(hint)))
                .clicked()
            {
                self.send(Command::Focus(step));
            }
        }
    }

    /// The levels behind the chain's keys. The keys switch the stages on and off.
    fn processing_section(ui: &mut Ui, status: &Status, look: &mut Look) {
        section(ui, "Processing");
        let gpu = status.vsr_available && status.vsr_problem.is_none();
        if gpu {
            row(ui, "Noise Reduction", |ui| {
                choice(
                    ui,
                    "denoise",
                    &mut look.denoise,
                    &[
                        (0, "Off"),
                        (8, "Low"),
                        (9, "Medium"),
                        (10, "High"),
                        (11, "Ultra"),
                    ],
                )
            });
            row(ui, "Super Resolution", |ui| {
                choice(
                    ui,
                    "vsr",
                    &mut look.vsr_quality,
                    &[(0, "Off"), (2, "Fast"), (3, "High"), (4, "Ultra")],
                )
            });
            if look.vsr_quality > 0 {
                row(ui, "Detail", |ui| {
                    fader(ui, &mut look.vsr_strength, 0.0..=1.0)
                });
            }
        } else {
            let why = status
                .vsr_problem
                .as_deref()
                .unwrap_or("NVIDIA's libraries aren't installed, so noise reduction and super resolution are off. Run scripts\\install.ps1 to fetch them.");
            note(ui, why, WARN);
        }
        if !gpu || look.vsr_quality == 0 {
            row(ui, "Sharpening", |ui| {
                fader(ui, &mut look.sharpen, 0.0..=1.5)
            });
        }
        if gpu {
            smooth_note(ui, status, look);
        }
        if look.fix_pixels {
            let text = match status.stuck {
                0 => "Hot Pixel Repair hasn't found any stuck pixels yet. It needs a few seconds of live view.".to_owned(),
                1 => "Hot Pixel Repair is filling 1 stuck pixel.".to_owned(),
                n => format!("Hot Pixel Repair is filling {n} stuck pixels."),
            };
            ui.add_space(2.0);
            note(ui, text, FAINT);
        }
    }

    fn picture_section(&self, ui: &mut Ui, look: &mut Look) {
        section(ui, "Picture");
        row(ui, "Frame", |ui| {
            segmented(
                ui,
                &mut look.fit,
                &[(true, "Whole Picture"), (false, "Fill 16:9")],
            )
        });
        row(ui, "Zoom", |ui| {
            choice(
                ui,
                "zoom",
                &mut look.zoom,
                &[
                    (100, "1\u{d7}"),
                    (125, "1.25\u{d7}"),
                    (150, "1.5\u{d7}"),
                    (175, "1.75\u{d7}"),
                    (200, "2\u{d7}"),
                ],
            )
        });
        if look.zoom > 100 {
            zoom_rows(ui, look);
        } else if !look.fit {
            row(ui, "Crop Position", |ui| {
                fader(ui, &mut look.framing, 0.0..=1.0)
                    .on_hover_text("Moves the crop between the top and bottom of the frame")
            });
        }
        row(ui, "Auto Frame", |ui| {
            segmented(ui, &mut look.auto_frame, &[(false, "Off"), (true, "On")])
        });
        if look.auto_frame {
            note(
                ui,
                "The crop follows your face, keeping it centred and a third of the way down. It zooms to at least 1.25\u{d7} so there's room to move.",
                FAINT,
            );
        }
        row(ui, "Mirror", |ui| {
            segmented(ui, &mut look.mirror, &[(false, "Off"), (true, "On")])
        });
        row(ui, "Upside Down", |ui| {
            segmented(ui, &mut look.upside_down, &[(false, "Off"), (true, "On")])
        });
        note(
            ui,
            "Mirror flips what apps get left to right, for apps that show you un-mirrored. Upside Down turns it over for a camera clamped upside down. Preview stays as the camera sees it.",
            FAINT,
        );
        let mut prefs = *lock(&self.hub.general);
        row(ui, "Framing Guide", |ui| {
            segmented(ui, &mut prefs.guide, &[(false, "Off"), (true, "On")])
        });
        note(ui, "Thirds lines over Preview. Apps never see them.", FAINT);
        *lock(&self.hub.general) = prefs;
    }

    fn general_section(&mut self, ui: &mut Ui, look: &mut Look) {
        section(ui, "General");
        let mut prefs = *lock(&self.hub.general);
        row(ui, "Pause After", |ui| {
            choice(
                ui,
                "pause",
                &mut prefs.pause_after,
                &[
                    (1, "1 Minute"),
                    (3, "3 Minutes"),
                    (10, "10 Minutes"),
                    (0, "Never"),
                ],
            )
        });
        note(
            ui,
            "How long live view runs for this window alone, to save the battery and keep the camera cool. It never stops while an app uses the camera.",
            FAINT,
        );
        let mut start = self.starts_with_windows;
        row(ui, "Start with Windows", |ui| {
            segmented(ui, &mut start, &[(false, "Off"), (true, "On")])
        });
        if start != self.starts_with_windows {
            self.starts_with_windows = if self.persist {
                general::set_starts_with_windows(general::RUN_KEY, start);
                general::starts_with_windows(general::RUN_KEY)
            } else {
                start
            };
        }
        row(ui, "Close Button", |ui| {
            segmented(
                ui,
                &mut prefs.close,
                &[(Close::Hide, "Hide to Tray"), (Close::Quit, "Quit")],
            )
        });
        row(ui, "Keep on Top", |ui| {
            segmented(ui, &mut prefs.on_top, &[(false, "Off"), (true, "On")])
        });
        row(ui, "Low Battery Warning", |ui| {
            segmented(
                ui,
                &mut prefs.battery_warning,
                &[(false, "Off"), (true, "On")],
            )
        });
        note(
            ui,
            "A tray notification when the camera's battery is down to 25%, and again when it's almost empty.",
            FAINT,
        );
        note(
            ui,
            "Ctrl+Alt+H hides or shows the camera and Ctrl+Alt+F runs Autofocus, even while another app is in front.",
            FAINT,
        );
        *lock(&self.hub.general) = prefs;
        self.reset_all(ui, look);
    }

    /// Reset All Settings, which asks for a second press within `ARMED_FOR` seconds.
    fn reset_all(&mut self, ui: &mut Ui, look: &mut Look) {
        ui.add_space(12.0);
        let now = ui.input(|i| i.time);
        let armed = self.reset_armed.is_some_and(|t| now - t < ARMED_FOR);
        let shown = if armed {
            "Press Again to Reset"
        } else {
            "Reset All Settings"
        };
        note(
            ui,
            "Puts the picture, camera and general settings back as they started. The camera choice, Start with Windows and the window's place stay as they are.",
            FAINT,
        );
        ui.add_space(6.0);
        let r = push_key(ui, shown, shown, false, false, None);
        if armed {
            let p = ui.painter();
            p.rect_filled(r.rect, 0, RED);
            // White rather than Plot Ink, which is 4.0:1 on the red and white 4.8:1.
            finish::label(
                p,
                Pos2::new(r.rect.left() + 13.0, r.rect.center().y),
                Align2::LEFT_CENTER,
                shown,
                LABEL,
                Color32::WHITE,
            );
        }
        if r.clicked() && armed {
            self.reset_armed = None;
            *look = Look::default();
            *lock(&self.hub.exposure) = exposure::Prefs::default();
            *lock(&self.hub.adjust) = Adjust::default();
            let mut prefs = lock(&self.hub.general);
            *prefs = general::Prefs {
                window: prefs.window,
                ..general::Prefs::default()
            };
        } else if r.clicked() {
            self.reset_armed = Some(now);
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(ARMED_FOR));
        } else if !armed {
            self.reset_armed = None;
        }
    }

    fn setting_row(&self, ui: &mut Ui, status: &Status, prop: Prop) {
        let setting = status.settings.iter().find(|s| s.prop == Some(prop));
        let (value, allowed) = setting.map_or((None, &[][..]), |s| (s.value, &s.allowed[..]));
        let kelvin = status
            .settings
            .iter()
            .find(|s| s.prop == Some(Prop::ColorTemp))
            .and_then(|s| s.value);
        row(ui, prop_name(prop), |ui| {
            let shown = match (prop, value, kelvin) {
                (Prop::WhiteBalance, Some(WB_KELVIN), Some(k)) => format!("{k} K"),
                _ => value.map_or_else(|| "Unavailable".into(), |v| label(prop, v)),
            };
            let r = ui
                .add_enabled_ui(allowed.len() > 1, |ui| {
                    egui::ComboBox::from_id_salt(prop_name(prop))
                        .width(150.0)
                        .icon(chevron)
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            menu(ui);
                            for &v in allowed {
                                if ui
                                    .selectable_label(Some(v) == value, label(prop, v))
                                    .clicked()
                                {
                                    self.send(Command::Set(prop, v));
                                }
                            }
                        })
                })
                .inner
                .response;
            edge(ui, &r);
            r
        });
    }
}

impl eframe::App for App {
    /// eframe skips `ui` while the window is minimized or hidden but still calls this on every
    /// repaint request, and the camera thread asks for one with each frame.
    fn logic(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        if !self.rounded
            && let Some(hwnd) = hwnd(frame)
        {
            self.rounded = true;
            round_corners(hwnd);
            chrome::install(hwnd, ctx);
            let place = lock(&self.hub.general).window;
            if let Some(place) = place {
                restore_place(hwnd, place);
            }
        }
        self.hub
            .preview_wanted
            .store(on_screen(frame), Ordering::Relaxed);
        self.follow_prefs(ctx, frame);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let quits = lock(&self.hub.general).close == Close::Quit;
        if ctx.input(|i| i.viewport().close_requested())
            && !quits
            && !self.quitting.load(Ordering::Relaxed)
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.hub.preview_wanted.store(false, Ordering::Relaxed);
            self.texture = None;
            self.still = None;
            self.image = Arc::default();
            return;
        }
        self.hub
            .preview_wanted
            .store(on_screen(frame), Ordering::Relaxed);
        self.follow_system(&ctx);
        self.draw(ui);
    }
}

impl App {
    /// Keeps the window, the tray and the battery warning in step with the settings, even
    /// while the window is hidden.
    fn follow_prefs(&mut self, ctx: &Context, frame: &eframe::Frame) {
        let prefs = *lock(&self.hub.general);
        if self.on_top != Some(prefs.on_top) {
            self.on_top = Some(prefs.on_top);
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if prefs.on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }));
        }
        let (hidden, battery) = {
            let s = lock(&self.hub.status);
            (s.hidden, s.battery)
        };
        if let Some(tray) = &self.tray
            && self.tray_says_show != hidden
        {
            self.tray_says_show = hidden;
            tray.hide
                .set_text(if hidden { "Show Camera" } else { "Hide Camera" });
        }
        self.warn_battery(battery, prefs.battery_warning);
        if self.persist
            && ctx.input(|i| i.viewport().close_requested())
            && let Some(place) = hwnd(frame).and_then(window_place)
            && prefs.window != Some(place)
        {
            let prefs = general::Prefs {
                window: Some(place),
                ..prefs
            };
            *lock(&self.hub.general) = prefs;
            general::save(prefs);
            self.saved_general = prefs;
        }
    }

    /// A tray notification once as the battery reaches 25%, and again as it's almost empty.
    /// The levels are the T3's own: 0 is Low, 5 is 25%, and the rest are higher.
    fn warn_battery(&mut self, battery: Option<u32>, on: bool) {
        let level = match battery {
            Some(0) => 2,
            Some(5) => 1,
            Some(_) => 0,
            None => return,
        };
        if level > self.battery_warned
            && on
            && let Some(tray) = &self.tray
        {
            let text = if level == 2 {
                "The camera's battery is almost empty. Plug in the mains adapter or change the battery soon."
            } else {
                "The camera's battery is down to 25%. Live view runs it down in an hour or two."
            };
            tray.notify("Camera Battery Low", text);
        }
        self.battery_warned = if level == 0 {
            0
        } else {
            level.max(self.battery_warned)
        };
    }

    /// Follows Windows' Text size and Show animations settings, read every couple of seconds.
    /// Text size zooms the whole window, and the smallest window grows with it.
    #[expect(
        clippy::float_cmp,
        reason = "any change to the scale or zoom at all has to be acted on"
    )]
    fn follow_system(&mut self, ctx: &Context) {
        let now = ctx.input(|i| i.time);
        if now - self.system_checked >= 2.0 {
            self.system_checked = now;
            let scale = text_scale();
            if scale != self.text_scale {
                self.text_scale = scale;
                // No bigger than lets the smallest layout fit on this screen.
                let fits = ctx.input(|i| i.viewport().monitor_size).map_or(scale, |m| {
                    let m = m * ctx.zoom_factor();
                    (m.x / MIN_SIZE.x).min(m.y * 0.9 / MIN_SIZE.y)
                });
                ctx.set_zoom_factor(scale.min(fits).max(1.0));
            }
            let still = !animations_on();
            ctx.all_styles_mut(|s| {
                let normal = egui::Style::default();
                s.animation_time = if still { 0.0 } else { normal.animation_time };
                s.scroll_animation = if still {
                    egui::style::ScrollAnimation::none()
                } else {
                    normal.scroll_animation
                };
            });
        }
        // Once the zoom has taken effect, so the size is in the new points.
        let zoom = ctx.zoom_factor();
        if zoom != self.min_zoom {
            self.min_zoom = zoom;
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(MIN_SIZE));
        }
    }
}

fn zoom_rows(ui: &mut Ui, look: &mut Look) {
    row(ui, "Left to Right", |ui| {
        fader(ui, &mut look.pan, 0.0..=1.0)
            .on_hover_text("Moves the zoomed crop between the left and right of the frame")
    });
    row(ui, "Top to Bottom", |ui| {
        fader(ui, &mut look.framing, 0.0..=1.0)
            .on_hover_text("Moves the zoomed crop between the top and bottom of the frame")
    });
    note(
        ui,
        "Zoom crops in before super resolution, so detail holds. A new zoom takes about a second to set up.",
        FAINT,
    );
    ui.add_space(6.0);
    if push_key(ui, "Reset Zoom", "Reset Zoom", false, false, None).clicked() {
        (look.zoom, look.pan, look.framing) = (100, 0.5, 0.5);
    }
}

fn smooth_note(ui: &mut Ui, status: &Status, look: &Look) {
    let (text, colour) = match (&status.smooth_problem, look.smooth, look.vsr_quality > 0) {
        (Some(p), true, true) => (format!("Smooth Motion failed, so frames are paced without frame generation. {p}"), WARN),
        (_, true, false) => ("Smooth Motion needs Super Resolution on.".to_owned(), WARN),
        (_, true, true) => (
            "Smooth Motion is on. Apps get an even 30 fps, and when the camera misses a frame, NVIDIA frame generation draws one in its place. It adds about 70 ms of delay."
                .to_owned(),
            FAINT,
        ),
        (_, false, _) => (
            "Smooth Motion is off. Apps get each camera frame as it's ready, which comes out at about 22 different frames a second."
                .to_owned(),
            FAINT,
        ),
    };
    ui.add_space(4.0);
    note(ui, text, colour);
}

const fn focus_note(prefs: &exposure::Prefs) -> &'static str {
    match (prefs.camera_af, prefs.hold_focus) {
        _ if !prefs.autofocus => {
            "Viewfinder never focuses by itself. The arrows below still move the lens."
        }
        (true, _) => {
            "The camera focuses with its own live view AF, which is slower and can hunt or pause the picture."
        }
        (false, false) => {
            "The computer steps the lens until the biggest face is sharpest, then stops. With no face, the lens stays put."
        }
        (false, true) => {
            "The computer keeps the biggest face sharp, nudging the lens whenever it stays soft for a second or two. With no face, the lens stays put."
        }
    }
}

fn white_note(ui: &mut Ui, status: &Status, creative: bool) {
    let (text, colour) = match (creative, status.white_unseen, status.white_held) {
        (false, ..) => (
            "Turn the mode dial to P, Tv, Av or M to hold white balance steady.".to_owned(),
            WARN,
        ),
        (true, true, _) => (
            "Live view didn't change when white balance did, so it's held where it is.".to_owned(),
            WARN,
        ),
        (true, false, Some(k)) => (
            format!("White balance is held at {k} K and stays put until the light changes colour."),
            FAINT,
        ),
        (true, false, None) => (
            "White balance is set for you from the grey things in view and stays put until the light changes colour.".to_owned(),
            FAINT,
        ),
    };
    note(ui, text, colour);
}

/// Small text's line height, Keepsake's 1.45.
const NOTE_LINE: f32 = 17.5;

fn note(ui: &mut Ui, text: impl Into<String>, colour: Color32) {
    ui.label(
        RichText::new(text)
            .small()
            .line_height(Some(NOTE_LINE))
            .color(colour),
    );
}

/// Windows' Text size, from Settings, Accessibility, Text size, as a factor.
#[expect(unsafe_code, reason = "reading the registry is a Win32 call")]
fn text_scale() -> f32 {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let (mut percent, mut size) = (100u32, size_of::<u32>() as u32);
    // SAFETY: the names are static wide strings, and `size` says the out slot holds one DWORD.
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Accessibility"),
            windows::core::w!("TextScaleFactor"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut percent).cast()),
            Some(&raw mut size),
        )
    };
    if read.is_ok() {
        percent.clamp(100, 225) as f32 / 100.0
    } else {
        1.0
    }
}

/// Windows' "Show animations" setting, under Accessibility, Visual effects.
#[expect(unsafe_code, reason = "reading a system setting is a Win32 call")]
fn animations_on() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut on = windows_core::BOOL(1);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL, which `on` is.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_err() || on.as_bool()
}

/// Copies an RGBA frame into `image` and its texture. A new 3.7 MB image and texture every
/// frame came to 25,000 page faults a second, so the image is refilled once the painter has
/// let go of it and the texture is updated where it is.
fn upload(
    ctx: &Context,
    name: &str,
    texture: &mut Option<TextureHandle>,
    image: &mut Arc<ColorImage>,
    size: [usize; 2],
    rgba: &[u8],
) {
    if image.size != size || Arc::get_mut(image).is_none() {
        *image = Arc::new(ColorImage::filled(size, Color32::BLACK));
    }
    if let Some(fresh) = Arc::get_mut(image) {
        for (px, &[r, g, b, _]) in fresh.pixels.iter_mut().zip(rgba.as_chunks::<4>().0) {
            *px = Color32::from_rgb(r, g, b);
        }
    }
    match texture {
        Some(t) if t.size() == size => t.set_partial([0, 0], image.clone(), TextureOptions::LINEAR),
        Some(t) => t.set(image.clone(), TextureOptions::LINEAR),
        None => *texture = Some(ctx.load_texture(name, image.clone(), TextureOptions::LINEAR)),
    }
}

fn hwnd(frame: &eframe::Frame) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = frame.window_handle().ok()?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return None;
    };
    Some(HWND(h.hwnd.get() as *mut std::ffi::c_void))
}

/// Whether the window is showing and not minimized. egui only knows about minimized, and a
/// window hidden in the tray would otherwise keep live view and super resolution running for
/// nobody. A window started in the tray has no handle yet, and that counts as hidden.
#[expect(unsafe_code, reason = "asking about a window is a Win32 call")]
fn on_screen(frame: &eframe::Frame) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
    hwnd(frame).is_some_and(|hwnd| {
        // SAFETY: both only read the state of a window handle, live or not.
        unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() }
    })
}

/// The window's restored place on the desktop, in pixels.
#[expect(unsafe_code, reason = "reading a window's placement is a Win32 call")]
fn window_place(hwnd: HWND) -> Option<[i32; 4]> {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, WINDOWPLACEMENT};
    let mut place = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: `place` is a WINDOWPLACEMENT with its length filled in.
    unsafe { GetWindowPlacement(hwnd, &raw mut place) }.ok()?;
    let r = place.rcNormalPosition;
    Some([r.left, r.top, r.right, r.bottom])
}

/// Puts the window back where it was. Windows moves it onto a screen if that place is now off
/// every one, as when a monitor has been unplugged. It stays hidden if it was.
#[expect(unsafe_code, reason = "placing a window is a Win32 call")]
fn restore_place(hwnd: HWND, [left, top, right, bottom]: [i32; 4]) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SW_HIDE, SW_SHOWNORMAL, SetWindowPlacement, WINDOWPLACEMENT,
    };
    // SAFETY: only reads the window's state.
    let shown = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let place = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        showCmd: (if shown { SW_SHOWNORMAL } else { SW_HIDE }).0 as u32,
        rcNormalPosition: RECT {
            left,
            top,
            right,
            bottom,
        },
        ..Default::default()
    };
    // SAFETY: `place` is a filled-in WINDOWPLACEMENT that outlives the call.
    unsafe { SetWindowPlacement(hwnd, &raw const place) }.ok();
}

/// Windows 11 rounds a window's corners only when it has the system frame, so ask for it.
#[expect(unsafe_code, reason = "setting a window attribute is a DWM call")]
fn round_corners(hwnd: HWND) {
    use windows::Win32::Graphics::Dwm::{
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
    };
    let round = DWMWCP_ROUND;
    // SAFETY: the pointer and size describe `round`, which outlives the call.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::from_ref(&round).cast(),
            size_of_val(&round) as u32,
        )
    }
    .ok();
}

fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

/// Archivo's capital height, as a share of its size.
const CAP_HEIGHT: f32 = 0.686;
const NAME_SIZE: f32 = 17.0;

/// The mark, 30 points square and centred on `mid`, laid out in whole physical pixels so the
/// brackets stay sharp at any scale. Returns its right edge.
fn mark(p: &egui::Painter, left: f32, mid: f32) -> f32 {
    let ppp = p.pixels_per_point();
    let centre = mid * ppp;
    let mut side = (30.0 * ppp).round();
    let mut top = (centre - side / 2.0).round();
    if (top + side / 2.0 - centre).abs() > 0.25 {
        side += 1.0;
        top = (centre - side / 2.0).round();
    }
    let left = (left * ppp).round();
    let at = |[x, y, width, height]: [u32; 4]| {
        Rect::from_min_size(
            Pos2::new(left + x as f32, top + y as f32) / ppp,
            Vec2::new(width as f32, height as f32) / ppp,
        )
    };
    let pixels = side as u32;
    finish::outline(p, at([0, 0, pixels, pixels]), EDGE);
    let brackets = crate::Brackets::new(pixels);
    for bar in brackets.bars() {
        p.rect_filled(at(bar), 0, INK);
    }
    let (x, y, tally) = brackets.tally;
    p.rect_filled(at([x, y, tally, tally]), 0, RED);
    (left + side) / ppp
}

/// The name with its capitals, not its line box, centred on `mid`. The size is nudged from 17
/// so the capitals land on whole pixels, crisp at the top and the baseline. Returns its right
/// edge.
fn name(ui: &Ui, left: f32, mid: f32) -> f32 {
    let p = ui.painter();
    let ppp = p.pixels_per_point();
    let centre = mid * ppp;
    let ideal = CAP_HEIGHT * NAME_SIZE * ppp;
    let miss = |cap: f32| {
        let edge = centre + cap / 2.0;
        (edge - edge.round())
            .abs()
            .mul_add(4.0, (cap - ideal).abs())
    };
    let cap = if miss(ideal.floor()) <= miss(ideal.ceil()) {
        ideal.floor()
    } else {
        ideal.ceil()
    };
    let galley = p.layout_job(finish::name_job(
        "Viewfinder",
        cap / (CAP_HEIGHT * ppp),
        INK,
    ));
    let baseline = galley
        .rows
        .first()
        .and_then(|r| r.glyphs.first().map(|g| r.pos.y + g.pos.y))
        .unwrap_or_default();
    let at = (centre + cap / 2.0).round() / ppp;
    let left = (left * ppp).round() / ppp;
    let width = galley.size().x - finish::trail(&galley);
    let rect = Rect::from_min_size(
        Pos2::new(left, at - baseline),
        Vec2::new(width, galley.size().y),
    );
    readout(
        &ui.interact(rect, egui::Id::unique("name"), Sense::hover()),
        || "Viewfinder".into(),
    );
    p.galley(rect.min, galley, INK);
    rect.right()
}

/// A Label head over its value, behind a Rule. Fixed width, so the bar stays put as the
/// numbers change.
fn stat(ui: &Ui, rect: Rect, name: &str, value: &str) {
    readout(
        &ui.interact(rect, egui::Id::unique(("stat", name)), Sense::hover()),
        || format!("{name}: {value}"),
    );
    let p = ui.painter();
    let x = p.round_to_pixel_center(rect.left());
    p.vline(x, rect.y_range(), Stroke::new(finish::hair(p), RULE));
    let x = rect.left() + 14.0;
    finish::label(
        p,
        Pos2::new(x, rect.top()),
        Align2::LEFT_TOP,
        name,
        LABEL,
        FAINT,
    );
    let value = clipped(
        ui,
        value,
        FontId::proportional(13.0),
        INK,
        rect.width() - 20.0,
    );
    p.galley(
        Align2::LEFT_BOTTOM
            .anchor_size(Pos2::new(x, rect.bottom() + 1.0), value.size())
            .min,
        value,
        INK,
    );
}

/// Names something painted for screen readers, which can't read the painter.
fn readout(r: &Response, text: impl Fn() -> String) {
    r.widget_info(|| WidgetInfo::labeled(Role::Label, true, text()));
}

fn clipped(ui: &Ui, text: &str, font: FontId, colour: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, colour);
    job.wrap = TextWrapping::truncate_at_width(width);
    ui.fonts_mut(|f| f.layout_job(job))
}

const fn tally_text(status: &Status) -> &'static str {
    match (status.hidden, status.consumer.is_some()) {
        (true, _) => "Hidden",
        (false, true) => "In Use",
        (false, false) => "Off Air",
    }
}

/// The red block at the right end of a lit tally, as on Keepsake's primary button.
const BLOCK: f32 = 9.0;

fn tally_width(ui: &Ui, status: &Status) -> f32 {
    let words = ui.fonts_mut(|f| f.layout_job(finish::label_job(tally_text(status), LABEL, INK)));
    13.0 + words.size().x - finish::trail(&words) + 13.0 + BLOCK
}

/// Whether an app is taking the picture, like a studio camera's tally light.
fn tally(ui: &Ui, rect: Rect, status: &Status) {
    let r = ui.interact(rect, egui::Id::unique("tally"), Sense::hover());
    readout(&r, || format!("Tally: {}", tally_text(status)));
    let p = ui.painter();
    // Hidden keeps the red off, since no picture is on air, and lights the words and outline.
    let t = finish::lit(p, r.id, status.consumer.is_some() && !status.hidden);
    if t > 0.0 {
        let block = Rect::from_min_max(Pos2::new(rect.right() - BLOCK, rect.top()), rect.max);
        p.rect_filled(block, 0, RED.gamma_multiply(t));
    }
    let t = if status.hidden { 1.0 } else { t };
    finish::outline(p, rect, EDGE.lerp_to_gamma(INK, t));
    finish::label(
        p,
        Pos2::new(rect.left() + 13.0, rect.center().y),
        Align2::LEFT_CENTER,
        tally_text(status),
        LABEL,
        FAINT.lerp_to_gamma(INK, t),
    );
}

/// A Label block over a picture's corner. `lit` inverts it, for a picture that is live.
fn plate(painter: &egui::Painter, rect: Rect, text: &str, lit: bool) {
    let (fill, ink) = if lit { (INK, GROUND) } else { (GROUND, INK) };
    let galley = painter.layout_job(finish::label_job(text, LABEL, ink));
    let tag = Rect::from_min_size(
        rect.min + Vec2::splat(10.0),
        Vec2::new(galley.size().x - finish::trail(&galley) + 16.0, 22.0),
    );
    painter.rect_filled(tag, 0, fill);
    finish::place(
        painter,
        Pos2::new(tag.left() + 8.0, tag.center().y),
        Align2::LEFT_CENTER,
        galley,
        ink,
    );
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(20.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        ui.label(finish::label_job(title, LABEL, INK));
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), Sense::hover());
        let p = ui.painter();
        let y = p.round_to_pixel_center(rect.center().y);
        p.hline(rect.x_range(), y, Stroke::new(finish::hair(p), RULE));
    });
    ui.add_space(4.0);
}

/// A name and its control, joined for screen readers.
fn row(ui: &mut Ui, name: &str, control: impl FnOnce(&mut Ui) -> Response) {
    ui.horizontal(|ui| {
        ui.set_min_height(38.0);
        let label = ui.label(RichText::new(name).color(DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            control(ui).labelled_by(label.id)
        });
    });
}

/// A button showing `shown`, which may be shorter than its name `text`, inverted while `on`.
/// A `strong` one keeps a Plot Ink outline and inverts under the pointer.
fn push_key(
    ui: &mut Ui,
    text: &str,
    shown: &str,
    on: bool,
    strong: bool,
    shortcut: Option<&str>,
) -> Response {
    let words = ui.fonts_mut(|f| f.layout_job(finish::label_job(shown, LABEL, INK)));
    let cap = if shortcut.is_some() { 28.0 } else { 0.0 };
    let size = Vec2::new(words.size().x - finish::trail(&words) + 26.0 + cap, 32.0);
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    r.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), text));
    let hot = r.hovered() || r.has_focus();
    let p = ui.painter();
    let t = finish::lit(p, r.id, on || (strong && r.hovered()));
    if r.is_pointer_button_down_on() && t < 1.0 {
        p.rect_filled(rect, 0, PRESSED_SHEET);
    }
    if t > 0.0 {
        p.rect_filled(rect, 0, INK.gamma_multiply(t));
    }
    finish::outline(p, rect, if hot || strong { INK } else { EDGE });
    if r.has_focus() {
        finish::focus_ring(p, rect);
    }
    let ink = INK.lerp_to_gamma(GROUND, t);
    finish::label(
        p,
        Pos2::new(rect.left() + 13.0, rect.center().y),
        Align2::LEFT_CENTER,
        shown,
        LABEL,
        ink,
    );
    if let Some(keys) = shortcut {
        let x = p.round_to_pixel_center(rect.right() - cap);
        p.vline(
            x,
            rect.y_range().shrink(8.0),
            Stroke::new(finish::hair(p), EDGE.lerp_to_gamma(GROUND, t)),
        );
        p.text(
            Pos2::new(rect.right() - cap / 2.0, rect.center().y),
            Align2::CENTER_CENTER,
            keys,
            FontId::proportional(13.0),
            FAINT.lerp_to_gamma(GROUND, t),
        );
    }
    r
}

/// The one action in its group, outlined in Plot Ink.
fn primary(ui: &mut Ui, text: &str) -> Response {
    push_key(ui, text, text, false, true, None)
}

/// A joined row of keys. As with Windows' radio buttons, Tab stops only on the chosen key and
/// the arrow keys move the choice.
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> Response {
    let height = 28.0;
    let widths: Vec<f32> = options
        .iter()
        .map(|o| {
            let g = ui.fonts_mut(|f| f.layout_job(finish::label_job(o.1, LABEL, INK)));
            (g.size().x - finish::trail(&g) + 22.0).round()
        })
        .collect();
    let total = Vec2::new(widths.iter().sum::<f32>(), height);
    let (rect, group) = ui.allocate_exact_size(total, Sense::hover());
    group.widget_info(|| WidgetInfo::labeled(Role::RadioGroup, ui.is_enabled(), ""));
    let keys = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .accessibility_parent(group.id),
    );
    let p = keys.painter();
    let mut x = rect.left();
    let mut moved = None;
    let mut focus = None;
    for (i, (&(option, text), width)) in options.iter().zip(widths).enumerate() {
        let key = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(width, height));
        x += width;
        let r = keys.interact(
            key,
            group.id.with(i),
            if *value == option {
                Sense::click()
            } else {
                Sense::CLICK
            },
        );
        if r.clicked() {
            *value = option;
        }
        if r.has_focus() {
            focus = Some(key);
            keys.memory_mut(|m| {
                m.set_focus_lock_filter(
                    r.id,
                    EventFilter {
                        horizontal_arrows: true,
                        ..Default::default()
                    },
                );
            });
            let step = keys.input(|i| {
                i32::from(i.key_pressed(Key::ArrowRight)) - i32::from(i.key_pressed(Key::ArrowLeft))
            });
            moved = i
                .checked_add_signed(step as isize)
                .filter(|&j| step != 0 && j < options.len());
        }
        let chosen = *value == option;
        r.widget_info(|| WidgetInfo::selected(Role::RadioButton, keys.is_enabled(), chosen, text));
        segment(p, &r, key, text, chosen, i == 0);
    }
    finish::outline(p, rect, EDGE);
    if let Some(key) = focus {
        finish::focus_ring(p, key);
    }
    if let Some((j, &(option, _))) = moved.and_then(|j| Some((j, options.get(j)?))) {
        *value = option;
        keys.memory_mut(|m| m.request_focus(group.id.with(j)));
        keys.ctx().request_repaint();
    }
    group
}

fn segment(p: &egui::Painter, r: &Response, key: Rect, text: &str, chosen: bool, first: bool) {
    let t = finish::lit(p, r.id, chosen);
    if t > 0.0 {
        p.rect_filled(key, 0, INK.gamma_multiply(t));
    } else if r.is_pointer_button_down_on() {
        p.rect_filled(key, 0, PRESSED_SHEET);
    }
    if !first {
        let x = p.round_to_pixel_center(key.left());
        p.vline(x, key.y_range(), Stroke::new(finish::hair(p), EDGE));
    }
    let idle = if r.hovered() { INK } else { DIM };
    finish::label(
        p,
        key.center(),
        Align2::CENTER_CENTER,
        text,
        LABEL,
        idle.lerp_to_gamma(GROUND, t),
    );
}

fn choice(ui: &mut Ui, id: &str, value: &mut u32, options: &[(u32, &str)]) -> Response {
    let shown = options
        .iter()
        .find(|o| o.0 == *value)
        .map_or("Custom", |o| o.1);
    let r = egui::ComboBox::from_id_salt(id)
        .width(150.0)
        .icon(chevron)
        .selected_text(shown)
        .show_ui(ui, |ui| {
            menu(ui);
            for &(v, text) in options {
                ui.selectable_value(value, v, text);
            }
        })
        .response;
    edge(ui, &r);
    r
}

fn chevron(ui: &Ui, rect: Rect, _: &egui::style::WidgetVisuals, _: bool) {
    let c = rect.center();
    let stroke = Stroke::new(1.4, DIM);
    let (w, h) = (4.0, 2.0);
    ui.painter()
        .line_segment([c + Vec2::new(-w, -h), c + Vec2::new(0.0, h)], stroke);
    ui.painter()
        .line_segment([c + Vec2::new(0.0, h), c + Vec2::new(w, -h)], stroke);
}

/// An open menu's items invert under the pointer, as the chosen one does.
fn menu(ui: &mut Ui) {
    let hovered = &mut ui.visuals_mut().widgets.hovered;
    hovered.weak_bg_fill = INK;
    hovered.bg_fill = INK;
    hovered.bg_stroke = Stroke::NONE;
    hovered.fg_stroke = Stroke::new(1.0, GROUND);
}

fn edge(ui: &Ui, r: &Response) {
    if r.has_focus() {
        finish::focus_ring(ui.painter(), r.rect);
    }
}

/// egui's slider for the dragging, keys and screen readers, drawn as Keepsake's volume.
fn fader(ui: &mut Ui, value: &mut f32, range: RangeInclusive<f32>) -> Response {
    // How far egui keeps the handle's centre from the ends, as a share of its height.
    const INSET: f32 = 0.12;
    let r = ui
        .scope(|ui| {
            let widgets = &mut ui.visuals_mut().widgets;
            for look in [
                &mut widgets.inactive,
                &mut widgets.hovered,
                &mut widgets.active,
            ] {
                (look.bg_fill, look.weak_bg_fill) = (Color32::TRANSPARENT, Color32::TRANSPARENT);
                (look.bg_stroke, look.fg_stroke) = (Stroke::NONE, Stroke::NONE);
            }
            ui.add(
                egui::Slider::new(value, range.clone())
                    .show_value(false)
                    .handle_shape(egui::style::HandleShape::Rect {
                        aspect_ratio: INSET,
                    }),
            )
        })
        .inner;
    let area = r.rect;
    let half = area.height() / 2.5 * INSET;
    let along = ((*value - range.start()) / (range.end() - range.start())).clamp(0.0, 1.0);
    let painter = ui.painter();
    let handle_x =
        painter.round_to_pixel_center(egui::lerp(area.left() + half..=area.right() - half, along));
    let mid = painter.round_to_pixel_center(area.center().y);
    let hair = finish::hair(painter);
    painter.hline(area.x_range(), mid, Stroke::new(hair, EDGE));
    painter.hline(area.left()..=handle_x, mid, Stroke::new(hair, INK));
    let handle = Rect::from_center_size(Pos2::new(handle_x, mid), Vec2::new(3.0, 14.0));
    painter.rect_filled(handle, 0, if r.dragged() { RED } else { INK });
    if r.has_focus() {
        finish::focus_ring(painter, handle);
    }
    r
}

/// One to three chevrons toward near (negative `step`) or far.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
fn chevrons(ui: &mut Ui, step: i8, size: f32, hint: &str) -> Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(size, size), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), hint));
    let painter = ui.painter();
    if r.is_pointer_button_down_on() {
        painter.rect_filled(rect, 0, PRESSED_SHEET);
    }
    let hot = r.hovered() || r.has_focus();
    finish::outline(painter, rect, if hot { INK } else { EDGE });
    if r.has_focus() {
        finish::focus_ring(painter, rect);
    }
    let ink = if hot { INK } else { DIM };
    let count = step.unsigned_abs() as usize;
    let dir = if step < 0 { -1.0 } else { 1.0 };
    let (arm, rise, gap) = (size * 0.12, size * 0.16, size * 0.14);
    let span = arm + gap * (count as f32 - 1.0);
    for i in 0..count {
        let x = rect.center().x - dir * span / 2.0 + dir * (arm + gap * i as f32);
        let tip = Pos2::new(x, rect.center().y);
        let back = Pos2::new(x - dir * arm, rect.center().y);
        let stroke = Stroke::new(1.4, ink);
        painter.line_segment([Pos2::new(back.x, back.y - rise), tip], stroke);
        painter.line_segment([Pos2::new(back.x, back.y + rise), tip], stroke);
    }
    r
}

/// The level spread as Keepsake's signal line, with a clipped end outlined in Warn.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
fn histogram(ui: &mut Ui, bins: &[u32], height: f32) {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let peak = bins.iter().copied().max().unwrap_or(0).max(1) as f32;
    let clipped_at = |i: Option<usize>| {
        i.and_then(|i| bins.get(i))
            .is_some_and(|&n| n as f32 > peak * 0.02)
    };
    let (shadows, highlights) = (clipped_at(Some(0)), clipped_at(bins.len().checked_sub(1)));
    r.widget_info(|| {
        let what = match (bins.is_empty(), shadows, highlights) {
            (true, ..) => "no picture",
            (_, true, true) => "shadows and highlights clipped",
            (_, true, false) => "shadows clipped",
            (_, false, true) => "highlights clipped",
            _ => "nothing clipped",
        };
        WidgetInfo::labeled(Role::Image, true, format!("Histogram: {what}"))
    });
    let painter = ui.painter();
    painter.rect_filled(rect, 0, MAT);
    let plot = rect.shrink(6.0);
    let hair = finish::hair(painter);
    for quarter in 1..4 {
        let x = painter.round_to_pixel_center(plot.left() + plot.width() * quarter as f32 / 4.0);
        painter.vline(x, rect.y_range(), Stroke::new(hair, ROW_RULE));
    }
    if bins.is_empty() {
        return;
    }
    let bar_w = plot.width() / bins.len() as f32;
    // Square root keeps a big flat wall from flattening everything else.
    let y = |n: u32| plot.bottom() - (n as f32 / peak).sqrt() * plot.height();
    let mut line = Vec::with_capacity(bins.len() * 2);
    for (i, &n) in bins.iter().enumerate() {
        let x = plot.left() + i as f32 * bar_w;
        line.extend([Pos2::new(x, y(n)), Pos2::new(x + bar_w, y(n))]);
    }
    painter.add(Shape::line(line, Stroke::new(1.0, INK)));
    for (clip, i) in [(shadows, 0), (highlights, bins.len() - 1)] {
        if let Some(&n) = bins.get(i).filter(|_| clip) {
            let x = plot.left() + i as f32 * bar_w;
            let bar = Rect::from_min_max(Pos2::new(x, y(n)), Pos2::new(x + bar_w, plot.bottom()));
            painter.rect_stroke(bar, 0, Stroke::new(1.0, WARN), StrokeKind::Middle);
        }
    }
}

fn diagnostics(ui: &mut Ui, status: &Status) {
    section(ui, "Diagnostics");
    let state = match (status.vsr_available, &status.vsr_problem) {
        (_, Some(_)) => Some(("Failed, CPU in use", WARN)),
        (true, None) => None,
        (false, None) => Some(("Effects not installed, CPU in use", FAINT)),
    };
    let card = crate::usage::nvidia().map(|c| c.name.as_str());
    let gpu = match (card, state) {
        (Some(name), _) => name,
        (None, Some((words, _))) => words,
        (None, None) => "NVIDIA RTX",
    };
    row(ui, "GPU", |ui| ui.label(RichText::new(gpu).color(INK)));
    // Under the name, since the name alone nearly fills the row.
    if let (Some(_), Some((words, colour))) = (card, state) {
        note(ui, words, colour);
    }
    let facts = [
        (
            "Exposure",
            status.off_target.map_or_else(
                || "No reading".into(),
                |d| format!("{d:+.1} stops from target"),
            ),
        ),
        (
            "Virtual Camera",
            if status.vcam.is_some() {
                "Not registered".into()
            } else {
                "Registered".into()
            },
        ),
    ];
    let light = status.light.map(|k| ("Light", format!("About {k} K")));
    for (name, value) in facts.into_iter().chain(light) {
        row(ui, name, |ui| ui.label(RichText::new(value).color(INK)));
    }
    if let Some(problem) = &status.vcam {
        note(ui, problem, WARN);
    }
}

const fn prop_name(prop: Prop) -> &'static str {
    match prop {
        Prop::Iso => "ISO",
        Prop::Shutter => "Shutter",
        Prop::Aperture => "Aperture",
        Prop::ExposureComp => "Exposure",
        Prop::WhiteBalance => "White Balance",
        Prop::PictureStyle => "Picture Style",
        Prop::AfMethod => "Focus Method",
        _ => "Setting",
    }
}

fn label(prop: Prop, v: u32) -> String {
    match prop {
        Prop::Iso => values::iso_label(v),
        Prop::Shutter => values::shutter_label(v),
        Prop::Aperture => values::aperture_label(v),
        Prop::ExposureComp => values::exposure_comp_label(v),
        Prop::WhiteBalance => values::white_balance_label(v),
        Prop::PictureStyle => values::picture_style_label(v),
        Prop::AfMethod => values::af_method_label(v),
        _ => format!("{v:#x}"),
    }
}

/// Archivo, Keepsake's face, in five static cuts with tabular figures. egui's own font stays
/// behind each as the fallback for anything Archivo lacks.
fn font_definitions() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let fallback = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let cuts: [(&str, &'static [u8], FontFamily); 5] = [
        (
            "archivo",
            include_bytes!("../assets/fonts/archivo-regular.ttf"),
            FontFamily::Proportional,
        ),
        (
            "archivo-medium",
            include_bytes!("../assets/fonts/archivo-medium.ttf"),
            medium(),
        ),
        (
            "archivo-wide-medium",
            include_bytes!("../assets/fonts/archivo-wide-medium.ttf"),
            finish::label_family(),
        ),
        (
            "archivo-expanded-light",
            include_bytes!("../assets/fonts/archivo-expanded-light.ttf"),
            finish::number_family(),
        ),
        (
            "archivo-expanded-semibold",
            include_bytes!("../assets/fonts/archivo-expanded-semibold.ttf"),
            finish::name_family(),
        ),
    ];
    for (name, bytes, family) in cuts {
        defs.font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
        defs.families
            .entry(family)
            .or_insert_with(|| fallback.clone())
            .insert(0, name.into());
    }
    defs
}

fn style(ctx: &Context) {
    ctx.set_fonts(font_definitions());
    let mut v = egui::Visuals::dark();
    v.panel_fill = GROUND;
    v.window_fill = GROUND;
    v.window_stroke = Stroke::new(1.0, INK);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.window_corner_radius = egui::CornerRadius::ZERO;
    v.menu_corner_radius = egui::CornerRadius::ZERO;
    v.extreme_bg_color = GROUND;
    v.faint_bg_color = HOVER_SHEET;
    v.selection.bg_fill = INK;
    v.selection.stroke = Stroke::new(1.0, GROUND);
    v.text_cursor.stroke = Stroke::new(2.0, RED);
    v.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.12 };
    for (w, fill, stroke, ink) in [
        (
            &mut v.widgets.noninteractive,
            Color32::TRANSPARENT,
            RULE,
            INK,
        ),
        (&mut v.widgets.inactive, Color32::TRANSPARENT, EDGE, INK),
        (&mut v.widgets.hovered, Color32::TRANSPARENT, FAINT, INK),
        (&mut v.widgets.active, PRESSED_SHEET, INK, INK),
        (&mut v.widgets.open, Color32::TRANSPARENT, INK, INK),
    ] {
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.fg_stroke = Stroke::new(1.0, ink);
        w.corner_radius = egui::CornerRadius::ZERO;
        w.expansion = 0.0;
    }
    // The scrollbar's thumb.
    v.widgets.inactive.bg_fill = EDGE;
    v.widgets.hovered.bg_fill = FAINT;
    v.widgets.active.bg_fill = INK;
    ctx.set_visuals(v);
    // Windows' Text size zooms the window instead, so Ctrl and plus can't push it past its layout.
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
        s.spacing.slider_width = 150.0;
        s.spacing.slider_rail_height = 0.0;
        s.spacing.interact_size.y = 32.0;
        s.text_styles
            .insert(TextStyle::Body, FontId::proportional(13.0));
        s.text_styles
            .insert(TextStyle::Button, FontId::proportional(13.0));
        s.text_styles
            .insert(TextStyle::Small, FontId::proportional(12.0));
        s.text_styles
            .insert(TextStyle::Heading, FontId::new(18.0, medium()));
    });
}

#[cfg(test)]
mod tests {
    use egui_kittest::Harness;
    use egui_kittest::kittest::NodeT as _;

    use super::*;
    use crate::worker::Hub;

    /// The window as eframe shows it, filling the viewport. kittest's own frame adds a margin
    /// the real window doesn't have.
    fn harness(hub: Arc<Hub>, size: Vec2, ppp: f32, gpu: bool) -> Harness<'static, App> {
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = App::with_context(
            &Context::default(),
            hub,
            tx,
            Arc::new(AtomicBool::new(false)),
            true,
        );
        let builder = Harness::builder()
            .with_size(size)
            .with_pixels_per_point(ppp)
            .with_context_setup(style);
        let builder = if gpu {
            builder.wgpu_setup(integrated())
        } else {
            builder
        };
        builder.build_ui_state(
            |ui, app: &mut App| {
                let screen = ui.ctx().content_rect();
                let mut window = ui.new_child(UiBuilder::new().max_rect(screen));
                window.set_clip_rect(screen);
                app.draw(&mut window);
            },
            app,
        )
    }

    /// kittest's own wgpu setup on the integrated GPU, or failing that a software one. Never the
    /// discrete GPU, which the app's own NVIDIA effects need.
    fn integrated() -> egui_wgpu::WgpuSetup {
        use egui_wgpu::wgpu::DeviceType;
        let mut setup = egui_kittest::wgpu::default_wgpu_setup();
        if let egui_wgpu::WgpuSetup::CreateNew(new) = &mut setup {
            new.native_adapter_selector = Some(Arc::new(|adapters, _| {
                [DeviceType::IntegratedGpu, DeviceType::Cpu]
                    .iter()
                    .find_map(|&kind| adapters.iter().find(|a| a.get_info().device_type == kind))
                    .cloned()
                    .ok_or_else(|| "no integrated or software adapter".to_owned())
            }));
        }
        setup
    }

    /// Puts the demo's made-up camera into one of the states a render shows.
    fn set_state(name: &str, hub: &Hub, harness: &mut Harness<'_, App>) {
        match name {
            "compare" => {
                harness.key_down(Key::Backslash);
                harness.run();
                demo::still(hub);
            }
            "faults" => {
                let mut s = lock(&hub.status);
                (
                    s.consumer,
                    s.battery,
                    s.vsr_available,
                    s.frame_ms,
                    s.stage_ms,
                ) = (None, None, false, 41.2, [6.3, 0.0, 0.0, 34.9]);
                drop(s);
                lock(&hub.look).fix_pixels = false;
            }
            "no-camera" => {
                let mut s = lock(&hub.status);
                (s.camera, s.live, s.battery, s.consumer, s.vsr_available) =
                    (None, false, None, None, false);
            }
            "holding" => {
                let mut prefs = lock(&hub.exposure);
                (prefs.white, prefs.hold_focus) = (true, true);
                drop(prefs);
                let mut s = lock(&hub.status);
                (s.white_held, s.light, s.focus) = (Some(3400), Some(3350), focus::Report::Flat);
                for setting in &mut s.settings {
                    match setting.prop {
                        Some(Prop::WhiteBalance) => setting.value = Some(WB_KELVIN),
                        Some(Prop::ColorTemp) => setting.value = Some(3400),
                        _ => {}
                    }
                }
            }
            "hidden" => {
                hub.set_hidden(true);
                lock(&hub.status).live = false;
            }
            "zoomed" => {
                let mut look = lock(&hub.look);
                (look.zoom, look.pan, look.mirror) = (150, 0.3, true);
                drop(look);
                lock(&hub.general).guide = true;
            }
            "cooling" => {
                let mut s = lock(&hub.status);
                (s.live, s.resting, s.consumer) = (false, true, None);
                s.problem = Some("The camera stopped sending live view, probably from heat. Trying again in a minute.".into());
            }
            _ => {}
        }
        harness.run();
    }

    /// Renders the window offscreen to PNGs in `VIEWFINDER_SHOTS`, in each state at the sizes
    /// and scales that matter. Skipped when that isn't set, since it needs a GPU.
    #[test]
    fn renders_window() {
        let Some(dir) = std::env::var_os("VIEWFINDER_SHOTS").map(std::path::PathBuf::from) else {
            return;
        };
        let mut shots: Vec<(String, &str, Vec2, f32)> = Vec::new();
        for state in [
            "live",
            "compare",
            "faults",
            "no-camera",
            "cooling",
            "hidden",
        ] {
            for (size, suffix) in [(Vec2::new(1180.0, 660.0), ""), (MIN_SIZE, "-small")] {
                shots.push((format!("{state}{suffix}"), state, size, 1.75));
            }
        }
        let laptop = Vec2::new(1180.0, 660.0);
        for (name, size, ppp) in [
            ("scale-125", laptop, 1.25),
            ("scale-150", laptop, 1.5),
            ("scale-200", laptop, 2.0),
            // Text size zooms the window, so the same pixels hold fewer points, down to the
            // smallest window, which Windows then grows to.
            ("text-125", (laptop / 1.25).max(MIN_SIZE), 1.75 * 1.25),
            ("text-150", (laptop / 1.5).max(MIN_SIZE), 1.75 * 1.5),
            ("half-1080p", Vec2::new(960.0, 1032.0), 1.0),
            ("maximized-1080p", Vec2::new(1920.0, 1032.0), 1.0),
            ("maximized-laptop", Vec2::new(1463.0, 886.0), 1.75),
            // The settings scrolled to their end, where the focus controls are.
            ("focus", laptop, 1.75),
            ("focus-small", MIN_SIZE, 1.75),
        ] {
            shots.push((name.into(), "live", size, ppp));
        }
        // The same with steady white balance holding and Always Focus on.
        for (name, size) in [("colour", laptop), ("colour-small", MIN_SIZE)] {
            shots.push((name.into(), "holding", size, 1.75));
        }
        // Zoomed in with the guide and Mirror on, scrolled to the Picture section.
        for (name, size) in [("picture", laptop), ("picture-small", MIN_SIZE)] {
            shots.push((name.into(), "zoomed", size, 1.75));
        }
        // The General section at the end, and Reset All Settings waiting for its second press.
        for (name, size) in [
            ("general", laptop),
            ("general-small", MIN_SIZE),
            ("reset-armed", laptop),
            ("lower-picture", laptop),
        ] {
            shots.push((name.into(), "live", size, 1.75));
        }
        for (name, state, size, ppp) in shots {
            let hub = demo::hub();
            let mut harness = harness(hub.clone(), size, ppp, true);
            harness.run();
            set_state(state, &hub, &mut harness);
            // Tabbing onto a control scrolls the settings to it, as it would for a person.
            let target = match name.split('-').next() {
                Some("focus" | "colour") => Some("Farther, big step"),
                Some("picture") => Some("Reset Zoom"),
                Some("general" | "reset") => Some("Reset All Settings"),
                Some("lower") => Some("Brightness"),
                _ => None,
            };
            if let Some(target) = target {
                for _ in 0..80 {
                    harness.key_press(Key::Tab);
                    harness.run();
                    if focused(&harness).as_deref() == Some(target) {
                        break;
                    }
                }
            }
            if name == "reset-armed" {
                harness.key_press(Key::Enter);
                harness.run();
            }
            harness
                .render()
                .expect("render")
                .save(dir.join(format!("{name}.png")))
                .expect("save");
        }
    }

    /// Every name in the accessibility tree, a label's text included.
    fn names(harness: &Harness<'_, App>) -> Vec<String> {
        harness
            .root()
            .children_recursive()
            .filter_map(|n| {
                n.accesskit_node()
                    .label()
                    .or_else(|| n.accesskit_node().value())
            })
            .collect()
    }

    fn focused(harness: &Harness<'_, App>) -> Option<String> {
        harness
            .root()
            .children_recursive()
            .find(egui_kittest::Node::is_focused)
            .and_then(|n| n.accesskit_node().label())
    }

    /// Tab reaches every control once, in reading order, and each says what it is.
    #[test]
    fn keyboard_reaches_everything_in_order() {
        let mut harness = harness(demo::hub(), Vec2::new(1180.0, 660.0), 1.75, false);
        harness.run();
        let mut order: Vec<String> = Vec::new();
        for _ in 0..80 {
            harness.key_press(Key::Tab);
            harness.run();
            let name = focused(&harness).expect("every Tab stop has a name");
            let at = harness
                .root()
                .children_recursive()
                .find(egui_kittest::Node::is_focused)
                .map(|n| n.rect())
                .expect("focused");
            assert!(
                at.top() >= 0.0 && at.bottom() <= 660.0 - 22.0,
                "{name} at {at:?} is in view and clear of the fade"
            );
            if order.first() == Some(&name) {
                break;
            }
            order.push(name);
        }
        let mut expected = vec![
            "Hold to Compare",
            "Repair: 3 Pixels",
            "Denoise: High",
            "Super Res: High",
            "Smooth: Ready",
            "Camera",
            "Off",
            "Noise Reduction",
            "Super Resolution",
            "Detail",
            "Whole Picture",
            "Zoom",
            "Off",
            "Off",
            "Off",
            "Off",
            "On",
            "Brightness",
            "Aperture",
            "Exposure",
        ];
        expected.extend(adjust::SLIDERS.map(|(name, ..)| name));
        expected.extend([
            "Auto",
            "Reset",
            "Off",
            "White Balance",
            "Picture Style",
            "On",
            "Viewfinder",
            "Off",
            "On",
            "Autofocus",
            "Nearer, big step",
            "Nearer",
            "Nearer, fine step",
            "Farther, fine step",
            "Farther",
            "Farther, big step",
            "Pause After",
            "Off",
            "Hide to Tray",
            "Off",
            "On",
            "Reset All Settings",
            "Minimize",
            "Maximize",
            "Close",
        ]);
        assert_eq!(order, expected);
    }

    /// The arrow keys move a segmented choice, and focus goes with it.
    #[test]
    fn arrows_move_a_choice() {
        let hub = demo::hub();
        let mut harness = harness(hub.clone(), Vec2::new(1180.0, 660.0), 1.75, false);
        harness.run();
        while focused(&harness).as_deref() != Some("Whole Picture") {
            harness.key_press(Key::Tab);
            harness.run();
        }
        harness.key_press(Key::ArrowRight);
        harness.run();
        assert!(!lock(&hub.look).fit, "Fill 16:9 chosen");
        assert_eq!(focused(&harness).as_deref(), Some("Fill 16:9"));
        harness.key_press(Key::ArrowLeft);
        harness.run();
        assert!(lock(&hub.look).fit, "back to Whole Picture");
    }

    /// What's painted rather than laid out is still there for a screen reader, and a readout
    /// with nothing in it isn't shown at all.
    #[test]
    fn painted_readouts_have_names() {
        let hub = demo::hub();
        let mut harness = harness(hub.clone(), Vec2::new(1180.0, 660.0), 1.75, false);
        harness.run();
        let all = names(&harness);
        for want in [
            "Camera: EOS REBEL T3",
            "Battery: 100%",
            "Tally: In Use",
            "Preview: Still, the camera's own picture",
            "Program: On Air, what apps get",
            "Camera: 27.6 fps",
            "Output: 1080p NV12",
            "Frame Budget: 14.7 ms per frame, 18.6 ms spare of 33.3. Decode 5.1, Repair 0.6, GPU 7.4, Output 1.6",
            "Histogram: shadows and highlights clipped",
        ] {
            assert!(
                all.iter().any(|n| n == want),
                "{want:?} missing from {all:#?}"
            );
        }
        set_state("no-camera", &hub, &mut harness);
        let all = names(&harness);
        assert!(
            !all.iter()
                .any(|n| n.starts_with("Battery") || n.starts_with("Camera: EOS")),
            "empty readouts are left out"
        );
        assert!(all.iter().any(|n| n == "Camera: Not Found"));
        set_state("hidden", &hub, &mut harness);
        let all = names(&harness);
        for want in [
            "Tally: Hidden",
            "Program: Off Air, Camera Hidden. Apps get a dark card saying the camera is hidden. Live view is off so the camera cools.",
        ] {
            assert!(
                all.iter().any(|n| n == want),
                "{want:?} missing from {all:#?}"
            );
        }
    }

    /// Reset All Settings takes a second press, and keeps the window's place.
    #[test]
    fn reset_needs_a_second_press() {
        let hub = demo::hub();
        lock(&hub.look).mirror = true;
        lock(&hub.general).window = Some([1, 2, 3, 4]);
        lock(&hub.general).on_top = true;
        let mut harness = harness(hub.clone(), Vec2::new(1180.0, 660.0), 1.75, false);
        harness.run();
        while focused(&harness).as_deref() != Some("Reset All Settings") {
            harness.key_press(Key::Tab);
            harness.run();
        }
        harness.key_press(Key::Enter);
        harness.run();
        assert!(lock(&hub.look).mirror, "one press changes nothing");
        assert_eq!(focused(&harness).as_deref(), Some("Press Again to Reset"));
        harness.key_press(Key::Enter);
        harness.run();
        assert_eq!(*lock(&hub.look), Look::default());
        let general = *lock(&hub.general);
        assert!(!general.on_top);
        assert_eq!(general.window, Some([1, 2, 3, 4]));
    }

    /// With Windows' animations off, a lamp lights in the same frame instead of fading in.
    #[test]
    fn lamps_switch_at_once_with_animations_off() {
        for animate in [true, false] {
            // kittest turns animations off after its setup hook, so this sets them each frame.
            let mut harness = Harness::builder().with_step_dt(1.0 / 60.0).build_ui_state(
                move |ui, (on, t): &mut (bool, f32)| {
                    ui.ctx().all_styles_mut(|s| {
                        s.animation_time = if animate {
                            egui::Style::default().animation_time
                        } else {
                            0.0
                        }
                    });
                    *t = finish::lit(ui.painter(), egui::Id::unique("lamp"), *on);
                },
                (false, 0.0),
            );
            harness.run();
            harness.state_mut().0 = true;
            harness.step();
            let t = harness.state().1;
            assert!(
                if animate {
                    t > 0.0 && t < 1.0
                } else {
                    t.to_bits() == 1.0f32.to_bits()
                },
                "animate {animate}, lit {t}"
            );
        }
    }

    /// A fresh frame or still reuses its pixels and texture once the painter has let go.
    #[test]
    fn pictures_are_copied_in_place() {
        let ctx = Context::default();
        let (mut texture, mut image) = (None, Arc::default());
        let rgba = vec![128u8; 64 * 36 * 4];
        upload(&ctx, "still", &mut texture, &mut image, [64, 36], &rgba);
        let (pixels, id) = (Arc::as_ptr(&image), texture.as_ref().map(TextureHandle::id));
        // What the painter does with each frame's upload.
        drop(ctx.tex_manager().write().take_delta());
        upload(&ctx, "still", &mut texture, &mut image, [64, 36], &rgba);
        assert_eq!(Arc::as_ptr(&image), pixels, "same pixels");
        assert_eq!(texture.as_ref().map(TextureHandle::id), id, "same texture");
    }
}
