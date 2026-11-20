//! Keepsake's material. Each call is a handful of shapes, since the window repaints with
//! every camera frame.

use std::sync::Arc;

use eframe::egui::{
    Align2, Color32, FontFamily, FontId, Galley, Id, Painter, Pos2, Rect, Shape, Stroke,
    StrokeKind, Vec2,
    text::{LayoutJob, TextFormat},
};

use super::INK;

/// Label type: Archivo at 118% width and 500, for every label, button and status word.
pub(super) fn label_family() -> FontFamily {
    FontFamily::Name("label".into())
}

/// Archivo at 125% width and 300, for numbers and big readouts.
pub(super) fn number_family() -> FontFamily {
    FontFamily::Name("number".into())
}

/// Archivo at 125% width and 600, for the name in the title bar.
pub(super) fn name_family() -> FontFamily {
    FontFamily::Name("name".into())
}

pub(super) fn hair(p: &Painter) -> f32 {
    1.0 / p.pixels_per_point()
}

pub(super) fn outline(p: &Painter, rect: Rect, colour: Color32) {
    p.rect_stroke(rect, 0, Stroke::new(hair(p), colour), StrokeKind::Inside);
}

pub(super) fn focus_ring(p: &Painter, rect: Rect) {
    p.rect_stroke(
        rect.expand(2.0),
        0,
        Stroke::new(hair(p), INK),
        StrokeKind::Outside,
    );
}

/// Hollow when off, so on and off differ in shape as well as colour.
pub(super) fn code(p: &Painter, rect: Rect, colour: Color32, filled: bool) {
    if filled {
        p.rect_filled(rect, 0, colour);
    } else {
        outline(p, rect, colour);
    }
}

pub(super) fn warning(p: &Painter, centre: Pos2, colour: Color32) {
    let (w, h) = (5.0, 4.5);
    p.add(Shape::convex_polygon(
        vec![
            centre + Vec2::new(0.0, -h),
            centre + Vec2::new(w, h),
            centre + Vec2::new(-w, h),
        ],
        colour,
        Stroke::NONE,
    ));
}

const FADE: f32 = 0.12;

pub(super) fn lit(p: &Painter, id: Id, on: bool) -> f32 {
    let ctx = p.ctx();
    let fade = if ctx.global_style().animation_time > 0.0 {
        FADE
    } else {
        0.0
    };
    ctx.animate_bool_with_time(id.with("lit"), on, fade)
}

fn spaced(text: &str, family: FontFamily, size: f32, tracking: f32, colour: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = TextFormat {
        font_id: FontId::new(size, family),
        color: colour,
        extra_letter_spacing: size * tracking,
        ..Default::default()
    };
    job.append(&text.to_uppercase(), 0.0, format);
    job
}

pub(super) fn label_job(text: &str, size: f32, colour: Color32) -> LayoutJob {
    spaced(text, label_family(), size, 0.12, colour)
}

pub(super) fn name_job(text: &str, size: f32, colour: Color32) -> LayoutJob {
    spaced(text, name_family(), size, 0.22, colour)
}

pub(super) fn label(
    p: &Painter,
    pos: Pos2,
    align: Align2,
    text: &str,
    size: f32,
    colour: Color32,
) -> Rect {
    let galley = p.layout_job(label_job(text, size, colour));
    place(p, pos, align, galley, colour)
}

pub(super) fn place(
    p: &Painter,
    pos: Pos2,
    align: Align2,
    galley: Arc<Galley>,
    colour: Color32,
) -> Rect {
    let size = galley.size();
    let rect = align.anchor_size(pos, size);
    p.galley(rect.min, galley, colour);
    rect
}
