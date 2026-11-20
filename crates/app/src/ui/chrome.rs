//! The window's own title bar. Windows' frame is off, so the buttons are drawn here, and a
//! hook tells Windows where the caption, edges and Maximize are, so it still moves, resizes and
//! snaps the window, and opens Snap Layouts over Maximize.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use eframe::egui::{
    self, Align, Color32, Context, Layout, Pos2, Rect, Response, Role, Sense, Stroke, Ui,
    UiBuilder, Vec2, ViewportCommand, WidgetInfo,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWLP_WNDPROC, GetClientRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION,
    HTCLIENT, HTLEFT, HTMAXBUTTON, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, PostMessageW,
    SC_MAXIMIZE, SC_RESTORE, SetWindowLongPtrW, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN,
    WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SYSCOMMAND, WNDPROC,
};

use super::{DIM, INK, finish};

/// How far down from the top of the window a press drags it.
const CAPTION: f32 = super::switcher::BAR;
/// How close to an edge a press resizes instead.
const EDGE: f32 = 6.0;
/// Windows 11's own caption buttons: their size, and Close's red.
const KEY: Vec2 = Vec2::new(46.0, 32.0);
const CLOSE_HOVER: Color32 = Color32::from_rgb(196, 43, 28);
/// How much of the top bar the three buttons take.
pub(super) const KEYS_WIDTH: f32 = KEY.x * 3.0;

/// The window's procedure before the hook, which gets every message the hook leaves alone.
static ORIGINAL: AtomicIsize = AtomicIsize::new(0);
/// egui's points per physical pixel, as f32 bits, so the hook can work in points.
static PPP: AtomicU32 = AtomicU32::new(0);
/// Maximize belongs to Windows while the pointer is over it, so its hover and press come from
/// the hook rather than from egui.
static MAX_HOT: AtomicBool = AtomicBool::new(false);
static MAX_DOWN: AtomicBool = AtomicBool::new(false);
static REPAINT: OnceLock<Context> = OnceLock::new();

/// Hooks the window once, so Windows asks `hit` what each point is.
#[expect(unsafe_code, reason = "replacing a window procedure is a Win32 call")]
pub(super) fn install(hwnd: HWND, ctx: &Context) {
    if ORIGINAL.load(Ordering::Relaxed) != 0 {
        return;
    }
    REPAINT.set(ctx.clone()).ok();
    // SAFETY: `procedure` has the signature a window procedure needs and lives for the program.
    let old = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, procedure as *const () as isize) };
    ORIGINAL.store(old, Ordering::Relaxed);
}

#[expect(
    unsafe_code,
    reason = "a window procedure is called by Windows through a raw pointer"
)]
unsafe extern "system" fn procedure(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: ORIGINAL holds the procedure SetWindowLongPtrW returned, set before the hook.
    let original: WNDPROC =
        unsafe { std::mem::transmute::<isize, WNDPROC>(ORIGINAL.load(Ordering::Relaxed)) };
    let on_max = wparam.0 == HTMAXBUTTON as usize;
    match msg {
        WM_NCHITTEST => {
            let ppp = f32::from_bits(PPP.load(Ordering::Relaxed));
            let mut point = POINT {
                x: i32::from((lparam.0 as u16).cast_signed()),
                y: i32::from(((lparam.0 >> 16) as u16).cast_signed()),
            };
            let mut client = RECT::default();
            if ppp > 0.0
                // SAFETY: Windows passed `hwnd`, and `point` is a live local.
                && unsafe { ScreenToClient(hwnd, &raw mut point) }.as_bool()
                // SAFETY: Windows passed `hwnd`, and `client` is a live local.
                && unsafe { GetClientRect(hwnd, &raw mut client) }.is_ok()
            {
                let pos = Pos2::new(point.x as f32, point.y as f32) / ppp;
                let size = Vec2::new(client.right as f32, client.bottom as f32) / ppp;
                // SAFETY: reads the state of the window Windows passed in.
                let maximized = unsafe { IsZoomed(hwnd) }.as_bool();
                return LRESULT(isize::try_from(hit(pos, size, maximized)).unwrap_or_default());
            }
        }
        WM_NCMOUSEMOVE => {
            if MAX_HOT.swap(on_max, Ordering::Relaxed) != on_max {
                if on_max {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: `track` is a live local with its size filled in.
                    unsafe { TrackMouseEvent(&raw mut track) }.ok();
                }
                repaint();
            }
        }
        WM_NCMOUSELEAVE => {
            MAX_HOT.store(false, Ordering::Relaxed);
            MAX_DOWN.store(false, Ordering::Relaxed);
            repaint();
        }
        // Left to Windows, a press here would draw its own old button over the window.
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if on_max => {
            MAX_DOWN.store(true, Ordering::Relaxed);
            repaint();
            return LRESULT(0);
        }
        WM_NCLBUTTONUP if on_max => {
            if MAX_DOWN.swap(false, Ordering::Relaxed) {
                // SAFETY: reads the state of the window Windows passed in.
                let command = if unsafe { IsZoomed(hwnd) }.as_bool() {
                    SC_RESTORE
                } else {
                    SC_MAXIMIZE
                };
                // SAFETY: posts a plain system command to the window Windows passed in.
                unsafe {
                    PostMessageW(
                        Some(hwnd),
                        WM_SYSCOMMAND,
                        WPARAM(command as usize),
                        LPARAM(0),
                    )
                }
                .ok();
            }
            repaint();
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: `original` is the procedure the window had, and gets the message unchanged.
    unsafe { CallWindowProcW(original, hwnd, msg, wparam, lparam) }
}

fn repaint() {
    if let Some(ctx) = REPAINT.get() {
        ctx.request_repaint();
    }
}

/// What a point in a window of `size` is, in points: an edge to resize by, the Maximize
/// button, the caption to drag by, or the window's own.
fn hit(pos: Pos2, size: Vec2, maximized: bool) -> u32 {
    if !maximized {
        let (left, right) = (pos.x < EDGE, pos.x > size.x - EDGE);
        let (top, bottom) = (pos.y < EDGE, pos.y > size.y - EDGE);
        let edge = match (top, bottom, left, right) {
            (true, _, true, _) => Some(HTTOPLEFT),
            (true, _, _, true) => Some(HTTOPRIGHT),
            (_, true, true, _) => Some(HTBOTTOMLEFT),
            (_, true, _, true) => Some(HTBOTTOMRIGHT),
            (true, ..) => Some(HTTOP),
            (_, true, ..) => Some(HTBOTTOM),
            (_, _, true, _) => Some(HTLEFT),
            (_, _, _, true) => Some(HTRIGHT),
            _ => None,
        };
        if let Some(edge) = edge {
            return edge;
        }
    }
    let from_right = size.x - pos.x;
    match (pos.y < KEY.y, from_right < KEYS_WIDTH) {
        (true, true) if (KEY.x..KEY.x * 2.0).contains(&from_right) => HTMAXBUTTON,
        (true, true) => HTCLIENT,
        _ if pos.y < CAPTION => HTCAPTION,
        _ => HTCLIENT,
    }
}

/// Minimize, Maximize and Close, like Windows' own.
pub(super) fn window_buttons(ui: &mut Ui, window: Rect) {
    let ctx = ui.ctx().clone();
    PPP.store(ctx.pixels_per_point().to_bits(), Ordering::Relaxed);
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    let strip = Rect::from_min_max(
        Pos2::new(window.right() - KEYS_WIDTH, window.top()),
        Pos2::new(window.right(), window.top() + KEY.y),
    );
    let ui = &mut ui.new_child(
        UiBuilder::new()
            .max_rect(strip)
            .layout(Layout::left_to_right(Align::Min)),
    );
    ui.spacing_mut().item_spacing.x = 0.0;
    if button(ui, "Minimize", Glyph::Minimize).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }
    let (name, glyph) = if maximized {
        ("Restore", Glyph::Restore)
    } else {
        ("Maximize", Glyph::Maximize)
    };
    if button(ui, name, glyph).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }
    if button(ui, "Close", Glyph::Close).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Minimize,
    Maximize,
    Restore,
    Close,
}

#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
fn button(ui: &mut Ui, name: &str, glyph: Glyph) -> Response {
    let (rect, response) = ui.allocate_exact_size(KEY, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, name));
    let max = matches!(glyph, Glyph::Maximize | Glyph::Restore);
    let hovered = response.hovered() || (max && MAX_HOT.load(Ordering::Relaxed));
    let down = response.is_pointer_button_down_on() || (max && MAX_DOWN.load(Ordering::Relaxed));
    let painter = ui.painter();
    let close = glyph == Glyph::Close && hovered;
    let fill = match (close, hovered, down) {
        (true, ..) => CLOSE_HOVER,
        (_, true, true) => Color32::from_white_alpha(10),
        (_, true, false) => Color32::from_white_alpha(15),
        _ => Color32::TRANSPARENT,
    };
    painter.rect_filled(rect, 0, fill);
    if response.has_focus() {
        finish::focus_ring(painter, rect.shrink(4.0));
    }
    let ink = match (close, hovered || response.has_focus()) {
        (true, _) => Color32::WHITE,
        (_, true) => INK,
        _ => DIM,
    };
    let c = rect.center();
    let stroke = Stroke::new(1.0, ink);
    let r = 5.0;
    match glyph {
        Glyph::Minimize => {
            painter.line_segment([c + Vec2::new(-r, 0.0), c + Vec2::new(r, 0.0)], stroke);
        }
        Glyph::Maximize => {
            painter.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(2.0 * r)),
                0,
                stroke,
                egui::StrokeKind::Middle,
            );
        }
        Glyph::Restore => {
            let front =
                Rect::from_center_size(c + Vec2::new(-1.5, 1.5), Vec2::splat(2.0 * r - 2.0));
            painter.rect_stroke(front, 0, stroke, egui::StrokeKind::Middle);
            painter.line_segment(
                [
                    Pos2::new(front.left() + 3.0, front.top() - 3.0),
                    Pos2::new(front.right() + 3.0, front.top() - 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(front.right() + 3.0, front.top() - 3.0),
                    Pos2::new(front.right() + 3.0, front.bottom() - 3.0),
                ],
                stroke,
            );
        }
        Glyph::Close => {
            painter.line_segment([c + Vec2::new(-r, -r), c + Vec2::new(r, r)], stroke);
            painter.line_segment([c + Vec2::new(-r, r), c + Vec2::new(r, -r)], stroke);
        }
    }
    // A tooltip, but not a description that repeats the name to a screen reader.
    response.on_hover_ui(|ui| drop(ui.label(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_gets_the_caption_edges_and_maximize() {
        let size = Vec2::new(1180.0, 640.0);
        let at = |x, y, maximized| hit(Pos2::new(x, y), size, maximized);
        assert_eq!(at(400.0, 20.0, false), HTCAPTION, "the empty top bar drags");
        assert_eq!(
            at(400.0, 200.0, false),
            HTCLIENT,
            "the monitors are the window's own"
        );
        assert_eq!(at(1180.0 - 23.0, 16.0, false), HTCLIENT, "Close is egui's");
        assert_eq!(
            at(1180.0 - 69.0, 16.0, false),
            HTMAXBUTTON,
            "Maximize opens Snap Layouts"
        );
        assert_eq!(
            at(1180.0 - 115.0, 16.0, false),
            HTCLIENT,
            "Minimize is egui's"
        );
        assert_eq!(
            at(1180.0 - 69.0, 40.0, false),
            HTCAPTION,
            "under the buttons is caption again"
        );
        assert_eq!(at(2.0, 300.0, false), HTLEFT);
        assert_eq!(at(1178.0, 638.0, false), HTBOTTOMRIGHT);
        assert_eq!(at(1178.0, 2.0, false), HTTOPRIGHT);
        assert_eq!(
            at(2.0, 300.0, true),
            HTCLIENT,
            "a maximized window has no edges"
        );
        assert_eq!(
            at(1180.0 - 69.0, 1.0, true),
            HTMAXBUTTON,
            "the buttons reach the screen's edge when maximized"
        );
    }
}
