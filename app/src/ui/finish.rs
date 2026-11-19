//! The panel's material: graphite modules, inset wells, backlit keys and spaced legends.
//! Each call is a handful of shapes, since the window repaints with every camera frame.

use std::sync::Arc;

use eframe::egui::{
    Align2, Color32, ColorImage, Context, FontFamily, FontId, Galley, Id, Mesh, Painter, Pos2,
    Rect, Stroke, StrokeKind, TextureId, TextureOptions, Vec2,
    epaint::RectShape,
    text::{LayoutJob, TextFormat},
};

use super::TEXT;

const MODULE_TOP: Color32 = Color32::from_rgb(34, 34, 38);
const MODULE_FOOT: Color32 = Color32::from_rgb(25, 25, 28);
const CAP_TOP: Color32 = Color32::from_rgb(50, 50, 55);
const CAP_FOOT: Color32 = Color32::from_rgb(35, 35, 39);
const LIT_TOP: Color32 = Color32::from_rgb(60, 60, 66);
const LIT_FOOT: Color32 = Color32::from_rgb(42, 42, 47);
pub(super) const WELL: Color32 = Color32::from_rgb(12, 12, 14);
const SEAM: Color32 = Color32::from_black_alpha(230);
/// Marks barely there on dark glass, like a scope's graticule.
pub(super) const FAINT: Color32 = Color32::from_rgba_premultiplied(9, 9, 9, 9);
/// Side of the grain tile, in physical pixels.
const TILE: f32 = 128.0;

pub(super) fn legend_family() -> FontFamily {
    FontFamily::Name("legend".into())
}

/// One physical pixel, in points.
fn hair(p: &Painter) -> f32 {
    1.0 / p.pixels_per_point()
}

/// A rounded rect lit from above, as a fan of vertices whose colour runs from `top` to `foot`.
/// The fan has no anti-aliasing of its own, so every caller strokes the edge after it.
fn shaded(rect: Rect, radius: f32, top: Color32, foot: Color32) -> Mesh {
    const STEPS: usize = 3;
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let shade =
        |y: f32| top.lerp_to_gamma(foot, ((y - rect.top()) / rect.height()).clamp(0.0, 1.0));
    let mut mesh = Mesh::default();
    mesh.reserve_vertices(1 + 4 * (STEPS + 1));
    mesh.colored_vertex(rect.center(), shade(rect.center().y));
    let corners = [
        (rect.right_bottom() + Vec2::new(-r, -r), 0.0f32),
        (rect.left_bottom() + Vec2::new(r, -r), 90.0),
        (rect.left_top() + Vec2::new(r, r), 180.0),
        (rect.right_top() + Vec2::new(-r, r), 270.0),
    ];
    for (centre, start) in corners {
        for s in 0..=STEPS {
            let a = (start + 90.0 * s as f32 / STEPS as f32).to_radians();
            let pos = centre + r * Vec2::new(a.cos(), a.sin());
            mesh.colored_vertex(pos, shade(pos.y));
        }
    }
    let n = mesh.vertices.len() as u32 - 1;
    for i in 1..=n {
        mesh.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    mesh
}

/// A tiling noise tile, made once per context and kept in its memory. Its id is kept beside
/// it, since cloning the handle on every lookup takes the texture manager's lock twice.
fn grain_texture(ctx: &Context) -> TextureId {
    let id = Id::unique("viewfinder-grain");
    if let Some(t) = ctx.data(|d| d.get_temp::<TextureId>(id)) {
        return t;
    }
    let side = TILE as usize;
    let mut seed = 0x9e37_79b9u32;
    let pixels = (0..side * side)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            // Premultiplied, so one tile both lightens and darkens.
            let v = (seed >> 24).cast_signed() - 128;
            let a = (v.unsigned_abs() * 2).min(255) as u8;
            if v > 0 {
                Color32::from_rgba_premultiplied(a, a, a, a)
            } else {
                Color32::from_rgba_premultiplied(0, 0, 0, a)
            }
        })
        .collect();
    let image = ColorImage::new([side, side], pixels);
    let handle = ctx.load_texture("grain", image, TextureOptions::NEAREST_REPEAT);
    let tid = handle.id();
    ctx.data_mut(|d| {
        d.insert_temp(id.with("handle"), handle);
        d.insert_temp(id, tid);
    });
    tid
}

/// Grain pinned to the screen's pixels, so it runs on unbroken across modules.
fn grain(p: &Painter, rect: Rect, radius: f32, strength: f32) {
    let scale = p.pixels_per_point() / TILE;
    let uv = Rect::from_min_max(
        (rect.min.to_vec2() * scale).to_pos2(),
        (rect.max.to_vec2() * scale).to_pos2(),
    );
    let tint = Color32::WHITE.gamma_multiply(strength);
    p.add(RectShape::filled(rect, radius, tint).with_texture(grain_texture(p.ctx()), uv));
}

/// A soft light around `rect`, one feathered shape.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
pub(super) fn glow(p: &Painter, rect: Rect, radius: f32, colour: Color32, reach: f32) {
    p.add(
        RectShape::filled(rect.expand(reach * 0.3), radius + reach * 0.3, colour)
            .with_blur_width(reach),
    );
}

/// A graphite module: lit top edge, darker foot, grain and a machined seam around it.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
pub(super) fn module(p: &Painter, rect: Rect, radius: f32) {
    let h = hair(p);
    p.add(shaded(rect, radius, MODULE_TOP, MODULE_FOOT));
    grain(p, rect, radius, 0.04);
    let r = radius.max(1.0);
    p.line_segment(
        [
            Pos2::new(rect.left() + r, rect.top() + h * 0.5),
            Pos2::new(rect.right() - r, rect.top() + h * 0.5),
        ],
        Stroke::new(h, Color32::from_white_alpha(20)),
    );
    p.line_segment(
        [
            Pos2::new(rect.left() + r, rect.bottom() - h * 0.5),
            Pos2::new(rect.right() - r, rect.bottom() - h * 0.5),
        ],
        Stroke::new(h, Color32::from_black_alpha(90)),
    );
    p.rect_stroke(rect, radius, Stroke::new(h, SEAM), StrokeKind::Outside);
}

/// The bare chassis between modules.
pub(super) fn chassis(p: &Painter, rect: Rect, colour: Color32) {
    p.rect_filled(rect, 0, colour);
    grain(p, rect, 0.0, 0.035);
}

/// A recess cut into a module: dark floor, shadowed top lip and a lit lower lip.
pub(super) fn well(p: &Painter, rect: Rect, radius: f32) {
    p.rect_filled(rect, radius, WELL);
    lips(p, rect, radius);
}

/// The two lips of a recess, for a widget that paints its own floor.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
pub(super) fn lips(p: &Painter, rect: Rect, radius: f32) {
    let h = hair(p);
    p.rect_stroke(
        rect,
        radius,
        Stroke::new(h, Color32::from_black_alpha(200)),
        StrokeKind::Inside,
    );
    let r = radius.max(1.0);
    p.line_segment(
        [
            Pos2::new(rect.left() + r, rect.top() + h * 1.5),
            Pos2::new(rect.right() - r, rect.top() + h * 1.5),
        ],
        Stroke::new(h, Color32::from_black_alpha(140)),
    );
    p.line_segment(
        [
            Pos2::new(rect.left() + r, rect.bottom() + h * 0.5),
            Pos2::new(rect.right() - r, rect.bottom() + h * 0.5),
        ],
        Stroke::new(h, Color32::from_white_alpha(16)),
    );
}

/// A monitor's dark glass falling off toward its edges.
/// Fades the edge of a scrolled list into the module behind it, so a line cut by the edge
/// melts away instead of ending mid glyph. `module` is the module's rect, for its colour at
/// that height. `down` fades toward the bottom.
pub(super) fn fade(p: &Painter, rect: Rect, module: Rect, down: bool) {
    let at = |y: f32| {
        MODULE_TOP.lerp_to_gamma(
            MODULE_FOOT,
            ((y - module.top()) / module.height()).clamp(0.0, 1.0),
        )
    };
    let (solid_y, clear_y) = if down {
        (rect.bottom(), rect.top())
    } else {
        (rect.top(), rect.bottom())
    };
    let (solid, clear) = (at(solid_y), at(clear_y).gamma_multiply(0.0));
    let mut mesh = Mesh::default();
    for (y, c) in [(clear_y, clear), (solid_y, solid)] {
        mesh.colored_vertex(Pos2::new(rect.left(), y), c);
        mesh.colored_vertex(Pos2::new(rect.right(), y), c);
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    p.add(mesh);
}

pub(super) fn vignette(p: &Painter, rect: Rect) {
    let inner = rect.shrink2(rect.size() * 0.18);
    let (clear, dark) = (Color32::TRANSPARENT, Color32::from_black_alpha(110));
    let mut mesh = Mesh::default();
    for (q, c) in [(rect, dark), (inner, clear)] {
        for pos in [
            q.left_top(),
            q.right_top(),
            q.right_bottom(),
            q.left_bottom(),
        ] {
            mesh.colored_vertex(pos, c);
        }
    }
    for i in 0..4u32 {
        let j = (i + 1) % 4;
        mesh.add_triangle(i, j, 4 + i);
        mesh.add_triangle(j, 4 + j, 4 + i);
    }
    p.add(mesh);
}

/// How long a lamp or key takes to light or fade, in seconds.
const FADE: f32 = 0.12;

/// How a key is lit and handled this frame.
#[derive(Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent things about one key, not states of one machine"
)]
pub(super) struct Key {
    /// Keeps the key's fade going from frame to frame.
    pub id: Id,
    /// The colour of its legend, edge and lamp when on.
    pub light: Color32,
    pub on: bool,
    pub fault: bool,
    pub hot: bool,
    pub down: bool,
    pub focus: bool,
}

impl Key {
    pub(super) const fn new(id: Id, light: Color32, on: bool) -> Self {
        Self {
            id,
            light,
            on,
            fault: false,
            hot: false,
            down: false,
            focus: false,
        }
    }
}

/// How far `id` has lit, from 0 to 1, fading over `FADE`. egui only repaints while it moves.
/// With Windows' animations off, `ui::follow_system` sets egui's animation time to zero, and
/// lamps switch at once.
pub(super) fn lit(p: &Painter, id: Id, on: bool) -> f32 {
    let ctx = p.ctx();
    let fade = if ctx.global_style().animation_time > 0.0 {
        FADE
    } else {
        0.0
    };
    ctx.animate_bool_with_time(id.with("lit"), on, fade)
}

/// Keyboard focus, as Windows 11 draws it: a white ring standing off the control.
pub(super) fn focus_ring(p: &Painter, rect: Rect, radius: f32) {
    p.rect_stroke(
        rect.expand(1.0),
        radius + 1.0,
        Stroke::new(2.0, TEXT),
        StrokeKind::Outside,
    );
}

/// A backlit key cap, graphite whether lit or not. Lit brightens the cap a touch and lights
/// its edge, with a faint glow around it. A fault rings it in `warn`. Returns how far it has
/// lit, for its legend.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the drawing must stay the same"
)]
pub(super) fn key(p: &Painter, rect: Rect, k: Key, reach: f32, warn: Color32) -> f32 {
    let radius = 4.0;
    let h = hair(p);
    let t = lit(p, k.id, k.on);
    if t > 0.0 && reach > 0.0 {
        glow(p, rect, radius, k.light.gamma_multiply(0.07 * t), reach);
    }
    let lift = |c: Color32, by: u8| {
        Color32::from_rgb(
            c.r().saturating_add(by),
            c.g().saturating_add(by),
            c.b().saturating_add(by),
        )
    };
    let (mut top, mut foot) = (
        CAP_TOP.lerp_to_gamma(LIT_TOP, t),
        CAP_FOOT.lerp_to_gamma(LIT_FOOT, t),
    );
    if k.hot {
        (top, foot) = (lift(top, 9), lift(foot, 9));
    }
    if k.down {
        (top, foot) = (foot, top);
    }
    p.rect_stroke(
        rect,
        radius,
        Stroke::new(h * 1.5, SEAM),
        StrokeKind::Outside,
    );
    p.add(shaded(rect, radius, top, foot));
    let rim = Color32::from_white_alpha(if k.down { 6 } else { 24 })
        .lerp_to_gamma(k.light.gamma_multiply(0.5), t);
    p.line_segment(
        [
            Pos2::new(rect.left() + radius, rect.top() + h * 0.5),
            Pos2::new(rect.right() - radius, rect.top() + h * 0.5),
        ],
        Stroke::new(h, rim),
    );
    let edge = if k.fault {
        Stroke::new(1.0, warn)
    } else {
        Stroke::new(h, Color32::from_black_alpha(160))
    };
    p.rect_stroke(rect, radius, edge, StrokeKind::Inside);
    if t > 0.0 {
        p.rect_stroke(
            rect,
            radius,
            Stroke::new(1.0, k.light.gamma_multiply(0.9 * t)),
            StrokeKind::Inside,
        );
    }
    if k.focus {
        focus_ring(p, rect, radius);
    }
    t
}

/// The small lamp in a key's corner, so on, off and fault differ in shape as well as
/// colour: a lit bar, an empty outline, or a warning triangle. `t` is how far it has lit.
pub(super) fn lamp(p: &Painter, centre: Pos2, fault: bool, colour: Color32, t: f32) {
    if fault {
        let (w, h) = (4.0, 3.5);
        let tri = vec![
            centre + Vec2::new(0.0, -h),
            centre + Vec2::new(w, h),
            centre + Vec2::new(-w, h),
        ];
        p.add(eframe::egui::Shape::convex_polygon(
            tri,
            colour,
            Stroke::NONE,
        ));
        return;
    }
    let bar = Rect::from_center_size(centre, Vec2::new(10.0, 3.0));
    if t < 1.0 {
        p.rect_stroke(
            bar,
            1.5,
            Stroke::new(1.0, Color32::from_gray(78).gamma_multiply(1.0 - t)),
            StrokeKind::Inside,
        );
    }
    if t > 0.0 {
        glow(p, bar, 1.5, colour.gamma_multiply(0.5 * t), 6.0);
        p.rect_filled(bar, 1.5, colour.gamma_multiply(t));
    }
}

/// The legend colour of a lit key: its light, pushed toward white so it reads as glowing.
pub(super) fn bright(c: Color32) -> Color32 {
    c.lerp_to_gamma(Color32::WHITE, 0.3)
}

/// Spaced capitals, as printed on a panel.
pub(super) fn legend_job(text: &str, size: f32, colour: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = TextFormat {
        font_id: FontId::new(size, legend_family()),
        color: colour,
        extra_letter_spacing: (size * 0.11).round_ties_even().max(1.0),
        ..Default::default()
    };
    job.append(&text.to_uppercase(), 0.0, format);
    job
}

/// Paints a legend anchored at `pos`, and returns where it went.
pub(super) fn legend(
    p: &Painter,
    pos: Pos2,
    align: Align2,
    text: &str,
    size: f32,
    colour: Color32,
) -> Rect {
    let galley = p.layout_job(legend_job(text, size, colour));
    place(p, pos, align, galley, colour)
}

/// Paints a galley anchored at `pos`. Letter spacing leaves a trailing gap after
/// the last letter, so it is left out of the size used to anchor.
pub(super) fn place(
    p: &Painter,
    pos: Pos2,
    align: Align2,
    galley: Arc<Galley>,
    colour: Color32,
) -> Rect {
    let trail = galley
        .job
        .sections
        .first()
        .map_or(0.0, |s| s.format.extra_letter_spacing);
    let size = galley.size() - Vec2::new(trail, 0.0);
    let rect = align.anchor_size(pos, size);
    p.galley(rect.min, galley, colour);
    rect
}
