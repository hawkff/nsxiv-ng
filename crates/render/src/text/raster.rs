//! Glyphs as coverage masks, made once for each face, glyph and size, and blended onto the
//! canvas.

use skrifa::{DrawMemory, GlyphId, HintOptions, PathSink, ScalerFont};
use tiny_skia::{FillRule, Mask, PathBuilder, Transform};

use super::fonts::Fonts;
use crate::Canvas;
use crate::canvas::over;

/// The most pixels a glyph's mask may have: a 2048 × 2048 square, twice the largest text on
/// each side, so a font with extreme outline bounds cannot make one allocation take the
/// memory.
const MAX_MASK_PIXELS: f32 = 2048.0 * 2048.0;

/// A glyph's coverage, 0 to 255 a pixel, rows top to bottom, placed from its origin on the
/// baseline.
pub(super) struct Glyph {
    left: i32,
    top: i32,
    width: usize,
    coverage: Vec<u8>,
}

impl Fonts {
    /// The ascent and the descent of a line in whole pixels: the bar font's box, which holds
    /// every glyph it has.
    pub(super) fn line(&mut self, pixels: u32) -> (i32, i32) {
        let bounds = self.scaler(0, pixels).metrics().bounds();
        (bounds.y_max.ceil() as i32, (-bounds.y_min).ceil() as i32)
    }

    pub(super) fn glyph(&mut self, face: usize, glyph: u32, pixels: u32) -> Option<&Glyph> {
        let key = (face, glyph, pixels);
        if !self.glyphs.contains_key(&key) {
            let made = rasterize(self.scaler(face, pixels), glyph);
            self.glyphs.insert(key, made);
        }
        self.glyphs[&key].as_ref()
    }

    fn scaler(&mut self, face: usize, pixels: u32) -> &ScalerFont {
        self.scalers.entry((face, pixels)).or_insert_with(|| {
            // hinted as FreeType hints by default, the font's own instructions where it has
            // them and the autohinter otherwise, which keeps small stems sharp; where hinting
            // fails to set up, unhinted, as unhinted would run the failing instructions again
            // for a font that relies on them
            let font = &self.faces[face].font;
            let hinted = ScalerFont::hinted(font.clone(), pixels as f32, HintOptions::default());
            match hinted.hinting_error() {
                Some(_) => ScalerFont::force_unhinted(font.clone(), Some(pixels as f32)),
                None => hinted,
            }
        })
    }
}

/// The glyph's outline filled with anti-aliasing, or None for a glyph with no outline or
/// one whose mask would pass [`MAX_MASK_PIXELS`].
pub(super) fn rasterize(scaler: &ScalerFont, glyph: u32) -> Option<Glyph> {
    // shortcut: a glyph with no outline, as a color emoji in bitmap strikes (sbix, CBDT) or
    // COLR layers, draws nothing; draw those through skrifa's bitmaps and color glyphs and
    // tiny-skia when color emoji matter
    let outline = scaler.outline(GlyphId::new(glyph))?;
    let mut pen = Pen(PathBuilder::new());
    outline.draw(DrawMemory::Scratch, &mut pen).ok()?;
    let path = pen.0.finish()?;
    let bounds = path.bounds();
    let (left, top) = (bounds.left().floor(), bounds.top().floor());
    let (right, bottom) = (bounds.right().ceil(), bounds.bottom().ceil());
    let placed = |v: f32| v.is_finite() && v.abs() < i32::MAX as f32;
    if ![left, top, right, bottom].into_iter().all(placed)
        || (right - left) * (bottom - top) > MAX_MASK_PIXELS
    {
        return None;
    }
    let (width, height) = ((right - left) as u32, (bottom - top) as u32);
    let mut mask = Mask::new(width, height)?;
    let shift = Transform::from_translate(-left, -top);
    mask.fill_path(&path, FillRule::Winding, true, shift);
    Some(Glyph {
        left: left as i32,
        top: top as i32,
        width: width as usize,
        coverage: mask.take(),
    })
}

/// Collects an outline, which skrifa gives y upward, as a path y downward.
struct Pen(PathBuilder);

impl PathSink for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, -y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, -y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, -cy0, x, -y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.cubic_to(cx0, -cy0, cx1, -cy1, x, -y);
    }

    fn close(&mut self) {
        self.0.close();
    }
}

/// Blends `glyph` in `color` with its origin at `origin`, inside `clip` and the canvas.
pub(super) fn blend(
    canvas: &mut Canvas,
    glyph: &Glyph,
    origin: [i64; 2],
    color: u32,
    clip: [i32; 4],
) {
    let (left, top) = (
        origin[0] + i64::from(glyph.left),
        origin[1] + i64::from(glyph.top),
    );
    let height = (glyph.coverage.len() / glyph.width) as i64;
    let span = |start: i64, length: i64, from: i32, size: i32, limit: u32| {
        let low = start.max(i64::from(from)).max(0);
        let high = (start + length)
            .min(i64::from(from) + i64::from(size))
            .min(i64::from(limit));
        low..high.max(low)
    };
    let columns = span(left, glyph.width as i64, clip[0], clip[2], canvas.width());
    let rows = span(top, height, clip[1], clip[3], canvas.height());
    let stride = canvas.width() as usize;
    let pixels = canvas.pixels_mut();
    for row in rows {
        let coverage = &glyph.coverage[(row - top) as usize * glyph.width..];
        for column in columns.clone() {
            let amount = coverage[(column - left) as usize];
            if amount > 0 {
                let pixel = &mut pixels[row as usize * stride + column as usize];
                *pixel = over(tint(color, amount), *pixel);
            }
        }
    }
}

/// A premultiplied color at `coverage` out of 255, rounded.
fn tint(color: u32, coverage: u8) -> u32 {
    if coverage == 255 {
        return color;
    }
    let mut pixel = 0;
    for shift in [0, 8, 16, 24] {
        let channel = ((color >> shift) & 0xff) * u32::from(coverage) + 127;
        pixel |= (channel / 255) << shift;
    }
    pixel
}
