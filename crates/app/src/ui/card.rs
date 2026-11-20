//! The card apps get while the camera is hidden, drawn on the CPU from egui's own glyphs.

use eframe::egui::epaint::text::{Fonts, TextOptions};
use eframe::egui::{Pos2, Vec2};
use shared::{Format, YUY2};

use super::{INK, LABEL, finish};

/// Ground, Edge and Plot Ink as video-range luma.
const GROUND_Y: u8 = 25;
const EDGE_Y: u8 = 100;
const INK_Y: u8 = 219;
const NEUTRAL: u8 = 128;

pub(crate) fn hidden_card(f: Format) -> Vec<u8> {
    let (w, h) = (f.width as usize, f.height as usize);
    let mut luma = vec![GROUND_Y; w * h];
    // Label type as it would look on a 240 point tall monitor, so it holds its share of a
    // call's tile whatever the format.
    corners(&mut luma, w, h, h.div_euclid(540).max(1));
    legend(&mut luma, w, h, h as f32 / 240.0);
    if f.fourcc == YUY2 {
        return luma
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|y| [y[0], NEUTRAL, y[1], NEUTRAL])
            .collect();
    }
    luma.resize(w * h + w * h.div_euclid(2), NEUTRAL);
    luma
}

/// "Camera Hidden" in the middle, laid out by egui and copied from its glyph atlas.
fn legend(luma: &mut [u8], w: usize, h: usize, ppp: f32) {
    let mut fonts = Fonts::new(TextOptions::default(), super::font_definitions());
    let galley = fonts
        .with_pixels_per_point(ppp)
        .layout_job(finish::label_job("Camera Hidden", LABEL, INK));
    let atlas = fonts.image();
    let size = (galley.size() - Vec2::new(finish::trail(&galley), 0.0)) * ppp;
    let origin = Vec2::new(w as f32 - size.x, h as f32 - size.y) / 2.0;
    for row in &galley.rows {
        for g in row.glyphs.iter().filter(|g| !g.uv_rect.is_nothing()) {
            let at = (Pos2::ZERO + row.pos.to_vec2() + g.pos.to_vec2() + g.uv_rect.offset) * ppp
                + origin;
            let ([u0, v0], [u1, v1]) = (g.uv_rect.min, g.uv_rect.max);
            for v in v0..v1 {
                for u in u0..u1 {
                    let cover = atlas
                        .pixels
                        .get(usize::from(v) * atlas.width() + usize::from(u))
                        .map_or(0, |c| u32::from(c.a()));
                    let x = (at.x.round() as usize).saturating_add(usize::from(u - u0));
                    let y = (at.y.round() as usize).saturating_add(usize::from(v - v0));
                    if x < w
                        && let Some(px) = luma.get_mut(y * w + x)
                    {
                        let ink = u32::from(INK_Y - GROUND_Y) * cover;
                        *px = GROUND_Y.saturating_add(ink.div_euclid(255) as u8);
                    }
                }
            }
        }
    }
}

/// Title-safe corners in Edge, as an empty monitor in the window shows.
fn corners(luma: &mut [u8], w: usize, h: usize, stroke: usize) {
    let (x0, y0) = ((w * 8).div_euclid(100), (h * 8).div_euclid(100));
    let (x1, y1) = (w - x0, h - y0);
    let arm = (w * 2).div_euclid(100);
    let mut fill = |left: usize, top: usize, width: usize, height: usize| {
        for row in luma.chunks_exact_mut(w).skip(top).take(height) {
            if let Some(span) = row.get_mut(left..left + width) {
                span.fill(EDGE_Y);
            }
        }
    };
    for (x, y) in [
        (x0, y0),
        (x1 - arm, y0),
        (x0, y1 - stroke),
        (x1 - arm, y1 - stroke),
    ] {
        fill(x, y, arm, stroke);
    }
    for (x, y) in [
        (x0, y0),
        (x1 - stroke, y0),
        (x0, y1 - arm),
        (x1 - stroke, y1 - arm),
    ] {
        fill(x, y, stroke, arm);
    }
}

#[cfg(test)]
mod tests {
    use shared::NV12;

    use super::*;

    /// With `VIEWFINDER_SHOTS` set, each card's luma is saved there as a PGM to look at.
    #[test]
    fn card_says_hidden_in_every_format() {
        for (width, height, fourcc) in [(1920, 1080, NV12), (1280, 720, YUY2), (640, 360, NV12)] {
            let f = Format {
                width,
                height,
                fourcc,
            };
            let card = hidden_card(f);
            assert_eq!(card.len(), f.bytes(), "{f:?}");
            let (w, h) = (width as usize, height as usize);
            let y = |x: usize, y: usize| {
                let i = if fourcc == YUY2 {
                    (y * w + x) * 2
                } else {
                    y * w + x
                };
                card.get(i).copied().unwrap()
            };
            assert_eq!(y(0, 0), GROUND_Y, "{f:?} ground");
            let middle =
                (w.div_euclid(3)..(w * 2).div_euclid(3)).any(|x| y(x, h.div_euclid(2)) > 150);
            assert!(middle, "{f:?} has the legend across its middle");
            if let Some(dir) = std::env::var_os("VIEWFINDER_SHOTS") {
                let luma: Vec<u8> = if fourcc == YUY2 {
                    card.iter().step_by(2).copied().collect()
                } else {
                    card.get(..w * h).unwrap().to_vec()
                };
                let mut pgm = format!("P5 {w} {h} 255\n").into_bytes();
                pgm.extend(luma);
                std::fs::write(
                    std::path::PathBuf::from(dir).join(format!("card-{w}.pgm")),
                    pgm,
                )
                .unwrap();
            }
        }
    }
}
