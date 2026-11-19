use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use canon::{Prop, values};
use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, Context, EventFilter, FontData, FontDefinitions,
    FontFamily, FontId, Galley, Key, Layout, Pos2, Rect, Response, RichText, Role, Sense, Shape,
    Stroke, StrokeKind, TextStyle, TextureHandle, TextureOptions, Ui, UiBuilder, Vec2,
    ViewportCommand, WidgetInfo,
    epaint::text::{FontTweak, VariationCoords},
    text::{LayoutJob, TextWrapping},
};
use windows::Win32::Foundation::HWND;

use crate::exposure;
use crate::pipeline::Look;
use crate::worker::{Command, Hub, Status, lock};

/// The mode dial on M, and the lens switch on MF, as the body reports them.
const MODE_M: u32 = 3;
const FOCUS_MF: u32 = 3;

mod chrome;
pub(crate) mod demo;
mod finish;
mod switcher;

// A neutral, untinted surround so the picture's colour is judged against nothing.
const BG: Color32 = Color32::from_rgb(11, 11, 12);
const RAISED: Color32 = Color32::from_rgb(38, 38, 42);
const HOVER: Color32 = Color32::from_rgb(48, 48, 53);
const LINE: Color32 = Color32::from_rgb(46, 46, 51);
const TEXT: Color32 = Color32::from_rgb(237, 237, 239);
const MUTED: Color32 = Color32::from_rgb(158, 158, 166);
/// Section headers and other printed legends.
const PRINT: Color32 = Color32::from_rgb(182, 182, 190);
/// Lit legends and edges, and the mark.
const SIGNAL: Color32 = Color32::from_rgb(255, 214, 10);
/// Reserved for one meaning: an app is receiving the picture right now.
const TALLY: Color32 = Color32::from_rgb(255, 59, 48);
/// Faults. Always with a ring or a warning triangle too, so it never reads as lit yellow.
const WARN: Color32 = Color32::from_rgb(255, 176, 32);
/// Preview's tally, lit while it shows the camera live, as a mixer's preview bus does.
const CUE: Color32 = Color32::from_rgb(48, 209, 88);
/// Glass with no light behind it: an unlit tally and the strip a monitor's tally sits in.
const DARK_GLASS: Color32 = Color32::from_rgb(20, 20, 22);
/// A dark translucent backing for words set over a picture.
const SCRIM: Color32 = Color32::from_black_alpha(190);
/// A scope's trace, the fill under it, and how far a fader has travelled.
const TRACE: Color32 = Color32::from_gray(214);
const TRACE_FILL: Color32 = Color32::from_rgba_unmultiplied_const(160, 162, 170, 120);
const TRAVEL: Color32 = Color32::from_gray(150);
/// The smallest window the layout is made for, in points. main.rs sets the same at zoom 1.
pub(crate) const MIN_SIZE: Vec2 = Vec2::new(860.0, 520.0);

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
    original: bool,
    saved_look: Look,
    saved_exposure: exposure::Prefs,
    /// Off in the demo, so trying its controls never touches the real saved settings.
    persist: bool,
    rounded: bool,
    /// The still's pixels, kept like `image` so a fresh still is copied in place.
    still_image: Arc<ColorImage>,
    bus: switcher::Bus,
    /// When Windows' settings were last read, in egui's time.
    system_checked: f64,
    /// The Text size last applied, and the zoom the minimum window size was last set for.
    text_scale: f32,
    min_zoom: f32,
}

impl App {
    pub(crate) fn new(
        cc: &eframe::CreationContext<'_>,
        hub: Arc<Hub>,
        commands: Sender<Command>,
        quitting: Arc<AtomicBool>,
        visible: bool,
    ) -> Self {
        Self::with_context(&cc.egui_ctx, hub, commands, quitting, visible)
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
        Self {
            hub,
            commands,
            quitting,
            texture: None,
            still: None,
            still_asked: f64::NEG_INFINITY,
            image: Arc::default(),
            histogram: vec![0; 64],
            original: false,
            saved_look,
            saved_exposure,
            persist: true,
            rounded: false,
            still_image: Arc::default(),
            bus: switcher::Bus::default(),
            system_checked: f64::NEG_INFINITY,
            text_scale: 1.0,
            min_zoom: 1.0,
        }
    }

    fn send(&self, cmd: Command) {
        self.commands.send(cmd).ok();
    }

    /// Draws the whole window. Split from `eframe::App` so it can be rendered offscreen.
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
                self.original = p.original;
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
        let status = lock(&self.hub.status).clone();
        let mut look = *lock(&self.hub.look);

        self.switcher(ui, &status, &mut look);

        *lock(&self.hub.look) = look;
        if self.persist && look != self.saved_look && !ctx.input(|i| i.pointer.any_down()) {
            crate::save_look(look);
            self.saved_look = look;
        }
        let prefs = *lock(&self.hub.exposure);
        if self.persist && prefs != self.saved_exposure && !ctx.input(|i| i.pointer.any_down()) {
            exposure::save(prefs);
            self.saved_exposure = prefs;
        }
    }

    fn top_bar(ui: &mut Ui, status: &Status) {
        ui.horizontal_centered(|ui| {
            mark(ui);
            ui.add_space(2.0);
            ui.label(finish::legend_job("Viewfinder", 13.0, TEXT));
            ui.add_space(12.0);
            // A readout with nothing to read is left out, not shown empty.
            let camera = status
                .camera
                .as_deref()
                .map(|m| m.trim_start_matches("Canon "));
            let battery = status.battery.map(values::battery_label);
            let room =
                ui.available_width() - chrome::KEYS_WIDTH - 14.0 - tally_width(ui, status) - 12.0;
            // Battery goes last, since it's the one worth knowing before a call.
            let battery_w = if battery.is_some() { 78.0 } else { 0.0 };
            if let Some(camera) = camera
                && room >= battery_w + 158.0
            {
                stat(ui, "Camera", camera, 158.0);
            }
            if let Some(battery) = &battery
                && room >= battery_w
            {
                stat(ui, "Battery", battery, battery_w);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(chrome::KEYS_WIDTH + 14.0);
                tally(ui, status);
            });
        });
    }

    /// Hold to see the camera's own picture, as Lightroom's backslash does. Holding Space while
    /// the key has focus does the same. Returns how wide the key came out.
    fn compare_button(&self, ui: &mut Ui, live: bool) -> f32 {
        if !live {
            ui.disable();
        }
        let key = live && ui.ctx().input(|i| i.key_down(Key::Backslash));
        let was = self.hub.compare.load(Ordering::Relaxed);
        // The shortcut's keycap only where there's room for it beside the monitor's name.
        let shortcut = (ui.available_width() >= 300.0).then_some("\\");
        let r = push_key(ui, "Hold to Compare", 0.0, was, (CUE, TEXT), shortcut).on_hover_text(
            "Shows the camera's picture before any processing. Backslash does the same.",
        );
        let held = r.is_pointer_button_down_on()
            || (r.has_focus() && ui.input(|i| i.key_down(Key::Space)));
        self.hub
            .compare
            .store(live && (held || key), Ordering::Relaxed);
        r.rect.width()
    }

    /// The live picture, fitted into `rect`, with its kind and any problem written over it.
    fn picture(&self, ui: &Ui, rect: Rect, status: &Status) {
        let Some(t) = &self.texture else { return };
        let painter = ui.painter_at(rect);
        let [w, h] = t.size();
        let scale = (rect.width() / w as f32).min(rect.height() / h as f32);
        let shown = Rect::from_center_size(rect.center(), Vec2::new(w as f32, h as f32) * scale);
        painter.image(
            t.id(),
            shown,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        if self.original {
            plate(&painter, shown, "Camera Original", Some(SIGNAL));
        }
        if let Some(problem) = &status.problem {
            let strip =
                Rect::from_min_max(Pos2::new(shown.left(), shown.bottom() - 30.0), shown.max);
            painter.rect_filled(strip, 0, Color32::from_black_alpha(200));
            finish::lamp(
                &painter,
                strip.left_center() + Vec2::new(16.0, 0.0),
                true,
                WARN,
                1.0,
            );
            let text = clipped(
                ui,
                problem,
                FontId::proportional(13.0),
                WARN,
                strip.width() - 44.0,
            );
            painter.galley(
                Align2::LEFT_CENTER
                    .anchor_size(strip.left_center() + Vec2::new(30.0, 0.0), text.size())
                    .min,
                text,
                WARN,
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
                    MUTED,
                ),
                Some(m) if values::is_creative_mode(m) => (
                    "Turn the mode dial to M to hold the exposure steady. On P, Tv and Av the camera meters again every moment.",
                    WARN,
                ),
                _ => ("", MUTED),
            };
            if !text.is_empty() {
                ui.label(RichText::new(text).small().color(colour));
            }
        }
        *lock(&self.hub.exposure) = prefs;
        for prop in [Prop::Iso, Prop::Shutter, Prop::Aperture, Prop::ExposureComp] {
            ui.add_enabled_ui(!(held && matches!(prop, Prop::Iso | Prop::Shutter)), |ui| {
                self.setting_row(ui, status, prop);
            });
        }
    }

    fn colour_and_focus(&self, ui: &mut Ui, status: &Status) {
        section(ui, "Colour");
        for prop in [Prop::WhiteBalance, Prop::PictureStyle] {
            self.setting_row(ui, status, prop);
        }
        section(ui, "Focus");
        if status.focus_mode == Some(FOCUS_MF) {
            ui.label(
                RichText::new("The lens switch is on MF, so focus stays where the ring puts it. Switch it to AF to focus from here.")
                    .small()
                    .color(MUTED),
            );
        }
        self.setting_row(ui, status, Prop::AfMethod);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if primary(ui, "Autofocus").clicked() {
                self.send(Command::Autofocus);
            }
            ui.add_space(4.0);
            self.nudges(ui, 30.0);
        });
    }

    /// Six lens nudges, three sizes each way.
    fn nudges(&self, ui: &mut Ui, size: f32) {
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
            if chevrons(ui, step, size, hint)
                .on_hover_ui(|ui| drop(ui.label(hint)))
                .clicked()
            {
                self.send(Command::Focus(step));
            }
        }
    }

    /// Framing, and the levels behind the chain's keys. The keys switch the stages on and off.
    fn picture_section(ui: &mut Ui, status: &Status, look: &mut Look) {
        section(ui, "Processing");
        row(ui, "Frame", |ui| {
            segmented(
                ui,
                &mut look.fit,
                &[(true, "Whole Picture"), (false, "Fill 16:9")],
            )
        });
        if !look.fit {
            row(ui, "Crop Position", |ui| {
                fader(ui, &mut look.framing, 0.0..=1.0)
                    .on_hover_text("Moves the crop between the top and bottom of the frame")
            });
        }
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
                .unwrap_or("NVIDIA's libraries aren't installed, so noise reduction and super resolution are off. Run install.ps1 to fetch them.");
            ui.label(RichText::new(why).small().color(WARN));
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
            ui.label(RichText::new(text).small().color(MUTED));
        }
    }

    fn setting_row(&self, ui: &mut Ui, status: &Status, prop: Prop) {
        let setting = status.settings.iter().find(|s| s.prop == Some(prop));
        let (value, allowed) = setting.map_or((None, &[][..]), |s| (s.value, &s.allowed[..]));
        row(ui, prop_name(prop), |ui| {
            let shown = value.map_or_else(|| "Unavailable".into(), |v| label(prop, v));
            let r = ui
                .add_enabled_ui(allowed.len() > 1, |ui| {
                    egui::ComboBox::from_id_salt(prop_name(prop))
                        .width(150.0)
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
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
        }
        self.hub
            .preview_wanted
            .store(on_screen(frame), Ordering::Relaxed);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting.load(Ordering::Relaxed) {
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
    /// Follows Windows' Text size and Show animations settings, read every couple of seconds.
    /// Text size zooms the whole window, since every key and readout is sized around its words,
    /// and the smallest window grows with it so the layout still fits.
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

/// What Smooth Motion is doing, or why it can't.
fn smooth_note(ui: &mut Ui, status: &Status, look: &Look) {
    let (text, colour) = match (&status.smooth_problem, look.smooth, look.vsr_quality > 0) {
        (Some(p), true, true) => (format!("Smooth Motion failed, so frames are paced without frame generation. {p}"), WARN),
        (_, true, false) => ("Smooth Motion needs Super Resolution on.".to_owned(), WARN),
        (_, true, true) => (
            "Smooth Motion is on. Apps get an even 30 fps, with NVIDIA frame generation drawing the frames in between. It adds about 70 ms of delay."
                .to_owned(),
            MUTED,
        ),
        (_, false, _) => (
            "Smooth Motion is off. Apps get each camera frame as it's ready, which comes out at about 22 different frames a second."
                .to_owned(),
            MUTED,
        ),
    };
    ui.add_space(4.0);
    ui.label(RichText::new(text).small().color(colour));
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

/// Whether the window is showing and not minimized. egui only knows about minimized, so
/// while the window sat hidden in the tray it kept asking for a preview, and the GPU ran
/// super resolution on every camera frame for nobody.
#[expect(unsafe_code, reason = "asking about a window is a Win32 call")]
fn on_screen(frame: &eframe::Frame) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
    hwnd(frame).is_none_or(|hwnd| {
        // SAFETY: both only read the state of a window handle, live or not.
        unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() }
    })
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

fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// The tray's ring, so the window and the tray read as one thing.
fn mark(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
    let c = rect.center();
    finish::glow(
        ui.painter(),
        Rect::from_center_size(c, Vec2::splat(12.0)),
        6.0,
        SIGNAL.gamma_multiply(0.18),
        10.0,
    );
    ui.painter().circle_stroke(c, 8.0, Stroke::new(2.4, SIGNAL));
    ui.painter().circle_filled(c, 3.2, SIGNAL);
}

/// A legend over a readout, behind a machined groove. Fixed width, so the bar stays put as
/// the numbers change.
fn stat(ui: &mut Ui, name: &str, value: &str, width: f32) {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::hover());
    readout(&r, || format!("{name}: {value}"));
    let p = ui.painter();
    groove(p, rect.left(), rect.y_range().shrink(2.0));
    let x = rect.left() + 12.0;
    finish::legend(
        p,
        Pos2::new(x, rect.top() + 2.0),
        Align2::LEFT_TOP,
        name,
        9.5,
        MUTED,
    );
    let value = clipped(ui, value, FontId::proportional(15.0), TEXT, width - 18.0);
    p.galley(
        Align2::LEFT_BOTTOM
            .anchor_size(Pos2::new(x, rect.bottom() + 1.0), value.size())
            .min,
        value,
        TEXT,
    );
}

/// Names something painted for screen readers, which can't read the painter.
fn readout(r: &Response, text: impl Fn() -> String) {
    r.widget_info(|| WidgetInfo::labeled(Role::Label, true, text()));
}

/// A vertical cut in the panel: a dark line with a lit edge beside it.
fn groove(p: &egui::Painter, x: f32, ys: egui::Rangef) {
    let h = 1.0 / p.pixels_per_point();
    p.vline(x, ys, Stroke::new(h, Color32::from_black_alpha(220)));
    p.vline(x + h, ys, Stroke::new(h, Color32::from_white_alpha(14)));
}

/// `text` on one line, cut short with an ellipsis where it would run past `width`.
fn clipped(ui: &Ui, text: &str, font: FontId, colour: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, colour);
    job.wrap = TextWrapping::truncate_at_width(width);
    ui.fonts_mut(|f| f.layout_job(job))
}

const fn tally_text(status: &Status) -> &'static str {
    if status.consumer.is_some() {
        "In Use"
    } else {
        "Off Air"
    }
}

fn tally_width(ui: &Ui, status: &Status) -> f32 {
    30.0 + ui
        .fonts_mut(|f| f.layout_job(finish::legend_job(tally_text(status), 10.5, TEXT)))
        .size()
        .x
        + 10.0
}

/// A lamp that lights red only while an app is taking the picture, like a studio camera's
/// tally light.
fn tally(ui: &mut Ui, status: &Status) {
    let (rect, r) =
        ui.allocate_exact_size(Vec2::new(tally_width(ui, status), 30.0), Sense::hover());
    readout(&r, || format!("Tally: {}", tally_text(status)));
    let p = ui.painter();
    finish::well(p, rect, 5.0);
    let t = finish::lit(p, r.id, status.consumer.is_some());
    let dot = Pos2::new(rect.left() + 15.0, rect.center().y);
    p.circle_filled(dot, 4.0, DARK_GLASS.lerp_to_gamma(TALLY, 0.2));
    p.circle_stroke(dot, 4.0, Stroke::new(1.0, Color32::from_black_alpha(160)));
    if t > 0.0 {
        p.rect_stroke(
            rect,
            5.0,
            Stroke::new(1.0, TALLY.gamma_multiply(0.45 * t)),
            StrokeKind::Inside,
        );
        finish::glow(
            p,
            Rect::from_center_size(dot, Vec2::splat(8.0)),
            4.0,
            TALLY.gamma_multiply(0.55 * t),
            12.0,
        );
        p.circle_filled(dot, 4.0, TALLY.gamma_multiply(t));
        p.circle_filled(
            dot - Vec2::new(1.0, 1.0),
            1.4,
            Color32::from_rgb(255, 190, 180).gamma_multiply(t),
        );
    }
    let ink = MUTED.lerp_to_gamma(finish::bright(TALLY), t);
    finish::legend(
        p,
        Pos2::new(rect.left() + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        tally_text(status),
        10.5,
        ink,
    );
}

/// A small dark label over a picture's corner, saying what kind of picture it is, with an
/// optional lamp.
fn plate(painter: &egui::Painter, rect: Rect, text: &str, lamp: Option<Color32>) {
    let galley = painter.layout_job(finish::legend_job(text, 10.0, TEXT));
    let lead = if lamp.is_some() { 16.0 } else { 0.0 };
    let tag = Rect::from_min_size(
        rect.min + Vec2::splat(10.0),
        Vec2::new(galley.size().x + 16.0 + lead, 22.0),
    );
    painter.rect_filled(tag, 4.0, SCRIM);
    painter.rect_stroke(
        tag,
        4.0,
        Stroke::new(1.0, Color32::from_white_alpha(22)),
        StrokeKind::Inside,
    );
    if let Some(c) = lamp {
        let dot = Pos2::new(tag.left() + 12.0, tag.center().y);
        finish::glow(
            painter,
            Rect::from_center_size(dot, Vec2::splat(6.0)),
            3.0,
            c.gamma_multiply(0.5),
            8.0,
        );
        painter.circle_filled(dot, 3.0, c);
    }
    finish::place(
        painter,
        Pos2::new(tag.left() + 8.0 + lead, tag.center().y),
        Align2::LEFT_CENTER,
        galley,
        TEXT,
    );
}

/// A legend with an engraved rule running out to the right of it.
fn section(ui: &mut Ui, title: &str) {
    ui.add_space(20.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        ui.label(finish::legend_job(title, 11.0, PRINT));
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), Sense::hover());
        let p = ui.painter();
        let h = 1.0 / p.pixels_per_point();
        let y = p.round_to_pixel_center(rect.center().y);
        p.hline(
            rect.x_range(),
            y,
            Stroke::new(h, Color32::from_black_alpha(220)),
        );
        p.hline(
            rect.x_range(),
            y + h,
            Stroke::new(h, Color32::from_white_alpha(14)),
        );
    });
    ui.add_space(4.0);
}

/// A name on the left and its control on the right, joined for screen readers.
fn row(ui: &mut Ui, name: &str, control: impl FnOnce(&mut Ui) -> Response) {
    ui.horizontal(|ui| {
        ui.set_min_height(34.0);
        let label = ui.label(RichText::new(name).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            control(ui).labelled_by(label.id)
        });
    });
}

/// A momentary key with its words on the cap, lit in `light` while held down or while `on`,
/// with an optional keyboard shortcut shown as a small keycap at its right end.
fn push_key(
    ui: &mut Ui,
    text: &str,
    min_width: f32,
    on: bool,
    (light, ink): (Color32, Color32),
    shortcut: Option<&str>,
) -> Response {
    let font = FontId::new(13.5, bold());
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font, ink));
    let cap = if shortcut.is_some() { 24.0 } else { 0.0 };
    let size = Vec2::new((galley.size().x + 28.0 + cap).max(min_width), 30.0);
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    r.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), text));
    let down = r.is_pointer_button_down_on();
    let k = finish::Key {
        hot: r.hovered(),
        down,
        focus: r.has_focus(),
        ..finish::Key::new(r.id, light, on || down)
    };
    let p = ui.painter();
    let t = finish::key(p, rect, k, 8.0, WARN);
    let nudge = Vec2::new(0.0, if down { 0.5 } else { 0.0 });
    let words = Rect::from_min_max(rect.min, rect.max - Vec2::new(cap, 0.0));
    p.galley(
        words.center() - galley.size() / 2.0 + nudge,
        galley,
        ink.lerp_to_gamma(finish::bright(light), t),
    );
    if let Some(keys) = shortcut {
        let hint = Rect::from_center_size(
            Pos2::new(rect.right() - 17.0, rect.center().y),
            Vec2::new(17.0, 17.0),
        );
        finish::well(p, hint, 3.0);
        p.text(
            hint.center() + Vec2::new(0.0, 0.5),
            Align2::CENTER_CENTER,
            keys,
            FontId::proportional(11.5),
            MUTED,
        );
    }
    r
}

/// The one action in its group, its legend printed in the accent colour.
fn primary(ui: &mut Ui, text: &str) -> Response {
    push_key(ui, text, 0.0, false, (SIGNAL, SIGNAL), None)
}

/// Keys set in one recessed strip, the chosen one lit. As with Windows' radio buttons, Tab
/// stops only on the chosen key and the arrow keys move the choice.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> Response {
    let font = FontId::proportional(13.5);
    let widths: Vec<f32> = options
        .iter()
        .map(|o| {
            ui.fonts_mut(|f| f.layout_no_wrap(o.1.to_owned(), font.clone(), TEXT))
                .size()
                .x
                + 26.0
        })
        .collect();
    let (pad, gap, height) = (3.0, 3.0, 26.0);
    let total = Vec2::new(
        widths.iter().sum::<f32>() + gap * (widths.len() - 1) as f32 + pad * 2.0,
        height + pad * 2.0,
    );
    let (rect, group) = ui.allocate_exact_size(total, Sense::hover());
    group.widget_info(|| WidgetInfo::labeled(Role::RadioGroup, ui.is_enabled(), ""));
    finish::well(ui.painter(), rect, 6.0);
    let keys = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .accessibility_parent(group.id),
    );
    let mut x = rect.left() + pad;
    let mut moved = None;
    for (i, (&(option, text), width)) in options.iter().zip(widths).enumerate() {
        let key = Rect::from_min_size(Pos2::new(x, rect.top() + pad), Vec2::new(width, height));
        x += width + gap;
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
        // A choice rather than a switch, so it lights white, not in the accent.
        let cap = finish::Key {
            hot: r.hovered(),
            down: r.is_pointer_button_down_on(),
            focus: r.has_focus(),
            ..finish::Key::new(r.id, TEXT, chosen)
        };
        let glow = finish::key(keys.painter(), key, cap, 0.0, WARN);
        let ink = MUTED.lerp_to_gamma(TEXT, glow);
        keys.painter()
            .text(key.center(), Align2::CENTER_CENTER, text, font.clone(), ink);
    }
    if let Some((j, &(option, _))) = moved.and_then(|j| Some((j, options.get(j)?))) {
        *value = option;
        keys.memory_mut(|m| m.request_focus(group.id.with(j)));
        keys.ctx().request_repaint();
    }
    group
}

fn choice(ui: &mut Ui, id: &str, value: &mut u32, options: &[(u32, &str)]) -> Response {
    let shown = options
        .iter()
        .find(|o| o.0 == *value)
        .map_or("Custom", |o| o.1);
    let r = egui::ComboBox::from_id_salt(id)
        .width(150.0)
        .selected_text(shown)
        .show_ui(ui, |ui| {
            for &(v, text) in options {
                ui.selectable_value(value, v, text);
            }
        })
        .response;
    edge(ui, &r);
    r
}

/// The recess egui's combo box sits in, and its focus ring.
fn edge(ui: &Ui, r: &Response) {
    finish::lips(ui.painter(), r.rect, 5.0);
    if r.has_focus() {
        finish::focus_ring(ui.painter(), r.rect, 5.0);
    }
}

/// egui's slider for the dragging, keys and screen readers, drawn as a fader: a slot with
/// its travel lit up to a graphite cap.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
fn fader(ui: &mut Ui, value: &mut f32, range: RangeInclusive<f32>) -> Response {
    let slot = ui.painter().add(Shape::Noop);
    let r = ui.add(egui::Slider::new(value, range.clone()).show_value(false));
    let rect = r.rect;
    // Where egui puts the handle: a rect 0.6 by 0.8 of the height, its centre kept inside.
    let half = rect.height() / 2.5 * 0.75;
    let t = ((*value - range.start()) / (range.end() - range.start())).clamp(0.0, 1.0);
    let x = egui::lerp(rect.left() + half..=rect.right() - half, t);
    let groove = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), 6.0));
    let lit = Rect::from_min_max(
        Pos2::new(groove.left() + 2.0, groove.center().y - 1.0),
        Pos2::new(x, groove.center().y + 1.0),
    );
    ui.painter().set(
        slot,
        vec![
            Shape::rect_filled(groove, 3.0, finish::WELL),
            Shape::rect_stroke(
                groove,
                3.0,
                Stroke::new(1.0, Color32::from_black_alpha(200)),
                StrokeKind::Inside,
            ),
            Shape::hline(
                groove.x_range().shrink(3.0),
                groove.bottom() + 0.5,
                Stroke::new(1.0, Color32::from_white_alpha(12)),
            ),
            Shape::rect_filled(lit, 1.0, TRAVEL),
        ],
    );
    let cap = Rect::from_center_size(
        Pos2::new(x, rect.center().y),
        Vec2::new(half * 2.0 + 1.0, rect.height() * 0.8 + 1.0),
    );
    let k = finish::Key {
        hot: r.hovered(),
        down: r.dragged(),
        focus: r.has_focus(),
        ..finish::Key::new(r.id, TEXT, false)
    };
    finish::key(ui.painter(), cap, k, 0.0, WARN);
    let index = if r.dragged() { SIGNAL } else { TEXT };
    ui.painter().vline(
        cap.center().x,
        cap.y_range().shrink(5.0),
        Stroke::new(1.5, index),
    );
    r
}

/// One to three chevrons pointing toward near (negative) or far, named `hint`.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
fn chevrons(ui: &mut Ui, step: i8, size: f32, hint: &str) -> Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(size, size), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), hint));
    let down = r.is_pointer_button_down_on();
    let cap = finish::Key {
        hot: r.hovered(),
        down,
        focus: r.has_focus(),
        ..finish::Key::new(r.id, SIGNAL, down)
    };
    let glow = finish::key(ui.painter(), rect, cap, 6.0, WARN);
    let ink = TEXT.lerp_to_gamma(finish::bright(SIGNAL), glow);
    let painter = ui.painter();
    let count = step.unsigned_abs() as usize;
    let dir = if step < 0 { -1.0 } else { 1.0 };
    let (arm, rise, gap) = (size * 0.13, size * 0.18, size * 0.15);
    let span = arm + gap * (count as f32 - 1.0);
    for i in 0..count {
        let x = rect.center().x - dir * span / 2.0 + dir * (arm + gap * i as f32);
        let tip = Pos2::new(x, rect.center().y);
        let back = Pos2::new(x - dir * arm, rect.center().y);
        let stroke = Stroke::new(1.6, ink);
        painter.line_segment([Pos2::new(back.x, back.y - rise), tip], stroke);
        painter.line_segment([Pos2::new(back.x, back.y + rise), tip], stroke);
    }
    r
}

/// The level spread as a scope trace in a recessed well, clipped ends in the warning colour.
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
    finish::well(painter, rect, 4.0);
    let plot = rect.shrink(4.0);
    for quarter in 1..4 {
        let x = painter.round_to_pixel_center(plot.left() + plot.width() * quarter as f32 / 4.0);
        painter.vline(x, plot.y_range(), Stroke::new(1.0, finish::FAINT));
    }
    let bar_w = plot.width() / bins.len().max(1) as f32;
    let mut top = Vec::with_capacity(bins.len() * 2);
    for (i, &n) in bins.iter().enumerate() {
        // Square root keeps a big flat wall from flattening everything else.
        let bar_h = (n as f32 / peak).sqrt() * (plot.height() - 2.0);
        let x = plot.left() + i as f32 * bar_w;
        let bar = Rect::from_min_max(
            Pos2::new(x, plot.bottom() - bar_h),
            Pos2::new(x + bar_w, plot.bottom()),
        );
        let clip = (i == 0 && shadows) || (i == bins.len() - 1 && highlights);
        painter.rect_filled(
            bar,
            0,
            if clip {
                WARN.gamma_multiply(0.75)
            } else {
                TRACE_FILL
            },
        );
        top.extend([bar.left_top(), bar.right_top()]);
    }
    painter.add(Shape::line(top, Stroke::new(1.0, TRACE)));
}

fn diagnostics(ui: &mut Ui, status: &Status) {
    section(ui, "Diagnostics");
    let facts = [
        (
            "GPU",
            match (status.vsr_available, &status.vsr_problem) {
                (_, Some(_)) => "Failed, CPU in use".into(),
                (true, None) => "NVIDIA RTX".into(),
                (false, None) => "Not installed".into(),
            },
        ),
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
    for (name, value) in facts {
        row(ui, name, |ui| ui.label(RichText::new(value).color(TEXT)));
    }
    if let Some(problem) = &status.vcam {
        ui.label(RichText::new(problem).small().color(WARN));
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

/// Segoe UI from Windows itself, so the window matches the rest of the system. Its regular
/// weight has tabular figures, so readouts use it and hold still as they change. Legends
/// are Bahnschrift, Windows' DIN, as printed on panels. egui's own font stays as the
/// fallback for anything these lack.
fn fonts(ctx: &Context) {
    let mut defs = FontDefinitions::default();
    let dir = std::env::var_os("WINDIR")
        .map_or_else(|| "C:\\Windows".into(), std::path::PathBuf::from)
        .join("Fonts");
    let fallback = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut bold_list = fallback.clone();
    let mut legend_list = fallback;
    let semibold = FontTweak {
        coords: VariationCoords::new([(b"wght", 600.0)]),
        ..Default::default()
    };
    for (name, file, tweak) in [
        ("segoe", "segoeui.ttf", None),
        ("segoe-semibold", "seguisb.ttf", None),
        ("bahnschrift", "bahnschrift.ttf", Some(semibold)),
    ] {
        let Ok(bytes) = std::fs::read(dir.join(file)) else {
            continue;
        };
        let data = FontData::from_owned(bytes);
        defs.font_data.insert(
            name.into(),
            Arc::new(if let Some(t) = tweak {
                data.tweak(t)
            } else {
                data
            }),
        );
        match name {
            "segoe" => defs
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, name.into()),
            "segoe-semibold" => {
                bold_list.insert(0, name.into());
                legend_list.insert(0, name.into());
            }
            _ => legend_list.insert(0, name.into()),
        }
    }
    defs.families.insert(bold(), bold_list);
    defs.families.insert(finish::legend_family(), legend_list);
    ctx.set_fonts(defs);
}

fn style(ctx: &Context) {
    fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = Color32::from_rgb(30, 30, 33);
    v.window_stroke = Stroke::new(1.0, Color32::from_black_alpha(230));
    v.extreme_bg_color = finish::WELL;
    v.faint_bg_color = RAISED;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = SIGNAL.linear_multiply(0.3);
    v.selection.stroke = Stroke::new(1.0, SIGNAL);
    v.text_cursor.stroke = Stroke::new(2.0, SIGNAL);
    let edge = Color32::from_black_alpha(210);
    for (w, weak, fill, stroke) in [
        (&mut v.widgets.noninteractive, finish::WELL, RAISED, LINE),
        (&mut v.widgets.inactive, finish::WELL, RAISED, edge),
        (
            &mut v.widgets.hovered,
            Color32::from_rgb(22, 22, 25),
            HOVER,
            Color32::from_gray(96),
        ),
        (
            &mut v.widgets.active,
            Color32::from_rgb(22, 22, 25),
            HOVER,
            Color32::from_gray(96),
        ),
        (
            &mut v.widgets.open,
            Color32::from_rgb(22, 22, 25),
            HOVER,
            SIGNAL,
        ),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = weak;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.corner_radius = egui::CornerRadius::same(5);
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
    v.widgets.inactive.fg_stroke = Stroke::new(1.5, TEXT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, TEXT);
    v.widgets.active.fg_stroke = Stroke::new(1.5, SIGNAL);
    ctx.set_visuals(v);
    // Windows' Text size zooms the window instead, so Ctrl and plus can't push it past its layout.
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(12.0, 5.0);
        s.spacing.slider_width = 150.0;
        s.spacing.slider_rail_height = 0.0;
        s.spacing.interact_size.y = 28.0;
        s.text_styles
            .insert(TextStyle::Body, FontId::proportional(14.0));
        s.text_styles
            .insert(TextStyle::Button, FontId::proportional(14.0));
        s.text_styles
            .insert(TextStyle::Small, FontId::proportional(12.0));
        s.text_styles
            .insert(TextStyle::Heading, FontId::new(18.0, bold()));
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
        let builder = if gpu { builder.wgpu() } else { builder };
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

    /// Puts the demo's made-up camera into one of the states a render shows.
    fn set_state(name: &str, hub: &Hub, harness: &mut Harness<'_, App>) {
        match name {
            "compare" => {
                harness.key_down(Key::Backslash);
                harness.run();
                demo::show(hub, true);
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
            "cooling" => {
                let mut s = lock(&hub.status);
                (s.live, s.resting, s.consumer) = (false, true, None);
                s.problem = Some("The camera stopped sending live view, probably because it's warm. It gets a minute to cool down.".into());
            }
            _ => {}
        }
        harness.run();
    }

    /// Renders the window offscreen to PNGs in `VIEWFINDER_SHOTS`: five states at the two
    /// usual sizes, then the live state at the smallest window, at each display scale, at
    /// Windows' 125% and 150% Text size, half a 1080p screen and maximized. Skipped when that
    /// isn't set, since it needs a GPU, and done one after another so the GPU has one job.
    #[test]
    fn renders_window() {
        let Some(dir) = std::env::var_os("VIEWFINDER_SHOTS").map(std::path::PathBuf::from) else {
            return;
        };
        let mut shots: Vec<(String, &str, Vec2, f32)> = Vec::new();
        for state in ["live", "compare", "faults", "no-camera", "cooling"] {
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
        ] {
            shots.push((name.into(), "live", size, ppp));
        }
        for (name, state, size, ppp) in shots {
            let hub = demo::hub();
            let mut harness = harness(hub.clone(), size, ppp, true);
            harness.run();
            set_state(state, &hub, &mut harness);
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
        for _ in 0..60 {
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
        let expected = [
            "Hold to Compare",
            "Repair: 3 Pixels",
            "Denoise: High",
            "Super Res: High",
            "Smooth: Ready",
            "Whole Picture",
            "Noise Reduction",
            "Super Resolution",
            "Detail",
            "On",
            "Brightness",
            "Aperture",
            "Exposure",
            "White Balance",
            "Picture Style",
            "Focus Method",
            "Autofocus",
            "Nearer, big step",
            "Nearer",
            "Nearer, fine step",
            "Farther, fine step",
            "Farther",
            "Farther, big step",
            "Minimize",
            "Maximize",
            "Close",
        ];
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
