//! Annotation text: a line in the first installed of the caller's families at a weight,
//! shaped once in font units and drawn as outlines at any size through a transform, with the
//! color bitmaps of emoji fonts (sbix, CBDT). The bar's text uses none of it.

use std::collections::HashMap;
use std::ops::Range;

use fontdb::{Database, Family, ID, Query, Source, Style};
use harfrust::{Buffer, Direction, ShapeOptions, ShaperFont};
use skrifa::bitmap::{BitmapData, BitmapStrikes};
use skrifa::font::glyph::bitmap::{Origin, Pixels};
use skrifa::instance::Size;
use skrifa::{DrawMemory, GlyphId, MetadataProvider, PathSink, ScalerFont, Tag};
use tiny_skia::{
    Color, FillRule, FilterQuality, IntSize, Paint, PathBuilder, Pixmap, PixmapMut, PixmapPaint,
    Shader, Transform,
};
use unicode_bidi::BidiInfo;
use unicode_script::{Script, UnicodeScript};

use super::MapFont;
use super::fonts::{Fonts, last_resort};
use super::shape::{Piece, harfrust_script, is_neutral};

/// A face, its weight on a `wght` axis, and its optical size on an `opsz` axis; 0 where the
/// face has no such axis or the line is not in the chosen face.
pub(super) type InstanceKey = (usize, u16, u16);

/// A face annotation text draws with, loaded once.
struct SansFace {
    id: ID,
    font: skrifa::Font,
    /// It has color bitmaps, as an emoji font does.
    color: bool,
}

/// A face at the weight and optical size a line is drawn in.
struct Instance {
    font: skrifa::Font,
    outlines: ScalerFont,
}

/// A glyph of a shaped line, with its origin from the line's start on the baseline in ems,
/// y upward.
#[derive(Clone, Copy)]
struct Shaped {
    key: InstanceKey,
    glyph: u32,
    x: f32,
    y: f32,
}

struct Line {
    glyphs: Vec<Shaped>,
    /// The advance in ems.
    width: f32,
}

/// A color bitmap decoded from one strike, premultiplied, with what places it.
struct Bitmap {
    image: Pixmap,
    /// The outer bearings, in font units.
    bearing: (f32, f32),
    /// The corner's inner bearings, in the strike's pixels.
    inner: (f32, f32),
    ppem: (f32, f32),
    origin: Origin,
}

#[derive(Default)]
pub(super) struct SansFonts {
    /// What maps a face's file, where the frontend gives a way to.
    pub(super) map: Option<MapFont>,
    faces: Vec<SansFace>,
    /// The face for families and a weight; None where none of them is installed and the bar
    /// font has no face to load.
    chosen: HashMap<(Vec<String>, u16), Option<usize>>,
    /// The face for a character and whether it shows as an emoji; None where no face draws it.
    fallbacks: HashMap<(char, bool), Option<usize>>,
    instances: HashMap<InstanceKey, Instance>,
    // shortcut: lines and bitmaps are never evicted; they live in the Text, as long as the
    // viewer does, every edit's lines at every size drawn and every emoji's bitmap at every
    // strike; evict the least recently used if a long session shows the memory grow
    lines: HashMap<(String, InstanceKey), Line>,
    bitmaps: HashMap<(usize, u32, u16), Option<Bitmap>>,
}

/// What a fallback face must draw a character with.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Outline,
    ColorBitmap,
}

impl Fonts {
    /// The face for the first installed of `families` at `weight`, else the bar font's.
    pub(super) fn sans_face(&mut self, families: &[String], weight: u16) -> Option<usize> {
        let key = (families.to_vec(), weight);
        if let Some(&face) = self.sans.chosen.get(&key) {
            return face;
        }
        let id = (families.iter())
            .find_map(|family| find(&self.database, family, weight))
            .unwrap_or(self.faces[0].id);
        let face = self.sans_load(id);
        self.sans.chosen.insert(key, face);
        face
    }

    /// The instance of `face` a line at `size` pixels per em draws in.
    pub(super) fn sans_key(&mut self, face: usize, weight: u16, size: f32) -> InstanceKey {
        let font = &self.sans.faces[face].font;
        let has = |tag: &[u8; 4]| font.axes().iter().any(|axis| axis.tag() == Tag::new(tag));
        let weight = if has(b"wght") { weight } else { 0 };
        // the optical size follows the size in whole pixels, as CoreText sets it by points
        let optical = if has(b"opsz") {
            size.round().clamp(1.0, f32::from(u16::MAX)) as u16
        } else {
            0
        };
        (face, weight, optical)
    }

    /// The width of `text` in ems.
    pub(super) fn sans_width(&mut self, key: InstanceKey, text: &str) -> f32 {
        self.sans_line(key, text).width
    }

    /// Draws `text` at `size` pixels per em with its baseline's start at `origin`, through
    /// `transform`: outlines in `color`, color bitmaps as they are.
    pub(super) fn sans_draw(
        &mut self,
        key: InstanceKey,
        target: &mut PixmapMut,
        text: &str,
        size: f32,
        origin: (f32, f32),
        (color, transform): (Color, Transform),
    ) {
        let glyphs = self.sans_line(key, text).glyphs.clone();
        // device pixels to the unit, which picks the bitmap strike
        let scale = transform.sx.hypot(transform.ky);
        let paint = Paint {
            shader: Shader::SolidColor(color),
            anti_alias: true,
            ..Paint::default()
        };
        for shaped in glyphs {
            let (x, y) = (origin.0 + shaped.x * size, origin.1 - shaped.y * size);
            let face = shaped.key.0;
            let upem = f32::from(self.sans.faces[face].font.units_per_em());
            let s = size / upem;
            if self.sans.faces[face].color
                && let Some(bitmap) = self.sans_bitmap(face, shaped.glyph, size * scale)
            {
                let (kx, ky) = (size / bitmap.ppem.0, size / bitmap.ppem.1);
                let left = x + bitmap.bearing.0 * s + bitmap.inner.0 * kx;
                let corner = y - (bitmap.bearing.1 * s + bitmap.inner.1 * ky);
                let top = match bitmap.origin {
                    Origin::BottomLeft => corner - bitmap.image.height() as f32 * ky,
                    Origin::TopLeft => corner,
                };
                let paint = PixmapPaint {
                    quality: FilterQuality::Bicubic,
                    ..PixmapPaint::default()
                };
                let placed = transform.pre_translate(left, top).pre_scale(kx, ky);
                target.draw_pixmap(0, 0, bitmap.image.as_ref(), &paint, placed, None);
                continue;
            }
            let instance = self.sans_instance(shaped.key);
            if let Some(path) = outline(&instance.outlines, shaped.glyph) {
                let placed = transform.pre_translate(x, y).pre_scale(s, -s);
                target.fill_path(&path, &paint, FillRule::Winding, placed, None);
            }
        }
    }

    fn sans_load(&mut self, id: ID) -> Option<usize> {
        if let Some(face) = self.sans.faces.iter().position(|face| face.id == id) {
            return Some(face);
        }
        // on the file's bytes where the frontend maps it, as the 192 MB of Apple Color Emoji;
        // else the face's whole file copied into memory
        let mapped = (self.sans.map.as_ref())
            .zip(self.database.face_source(id))
            .and_then(|(map, (source, index))| match source {
                Source::File(path) | Source::SharedFile(path, _) => Some((map(&path)?, index)),
                Source::Binary(_) => None,
            })
            .and_then(|(bytes, index)| skrifa::Font::new(bytes, index));
        let font = match mapped {
            Some(font) => font,
            None => (self.database)
                .with_face_data(id, |data, index| skrifa::Font::new(data.to_vec(), index))??,
        };
        let color = matches!(
            font.bitmap_strikes().source(),
            Some(skrifa::font::glyph::bitmap::BitmapSource::Sbix)
                | Some(skrifa::font::glyph::bitmap::BitmapSource::Cbdt)
        );
        self.sans.faces.push(SansFace { id, font, color });
        Some(self.sans.faces.len() - 1)
    }

    fn sans_instance(&mut self, key: InstanceKey) -> &Instance {
        let faces = &self.sans.faces;
        self.sans.instances.entry(key).or_insert_with(|| {
            let base = &faces[key.0].font;
            let mut variations = Vec::new();
            if key.1 != 0 {
                variations.push((Tag::new(b"wght"), f32::from(key.1)));
            }
            if key.2 != 0 {
                variations.push((Tag::new(b"opsz"), f32::from(key.2)));
            }
            let font = if variations.is_empty() {
                base.clone()
            } else {
                base.instance_builder().variations(variations).build()
            };
            Instance {
                outlines: ScalerFont::force_unhinted(font.clone(), None),
                font,
            }
        })
    }

    fn sans_line(&mut self, key: InstanceKey, text: &str) -> &Line {
        let cached = (text.to_owned(), key);
        if !self.sans.lines.contains_key(&cached) {
            let line = self.sans_layout(key, text);
            self.sans.lines.insert(cached.clone(), line);
        }
        &self.sans.lines[&cached]
    }

    /// Lays out a line: unicode-bidi orders its runs, each run splits where the face or the
    /// script changes, and harfrust shapes the pieces, each in its script.
    fn sans_layout(&mut self, key: InstanceKey, text: &str) -> Line {
        let (mut glyphs, mut pen) = (Vec::new(), 0.0);
        let bidi = BidiInfo::new(text, None);
        for paragraph in &bidi.paragraphs {
            let (levels, runs) = bidi.visual_runs(paragraph, paragraph.range.clone());
            for run in runs {
                let rtl = levels[run.start].is_rtl();
                let mut pieces = self.sans_pieces(key.0, text, run);
                if rtl {
                    pieces.reverse();
                }
                for piece in pieces {
                    // fallback faces draw at their defaults
                    let key = if piece.face == key.0 {
                        key
                    } else {
                        (piece.face, 0, 0)
                    };
                    pen = self.sans_shape(key, text, &piece, rtl, pen, &mut glyphs);
                }
            }
        }
        Line { glyphs, width: pen }
    }

    /// The run's clusters in logical order, each with the face that draws it, those of one face
    /// and one script together.
    pub(super) fn sans_pieces(
        &mut self,
        chosen: usize,
        text: &str,
        run: Range<usize>,
    ) -> Vec<Piece> {
        let mut clusters: Vec<Range<usize>> = Vec::new();
        let mut previous = None;
        for (offset, c) in text[run.clone()].char_indices() {
            let at = run.start + offset;
            match clusters.last_mut() {
                Some(last) if continues(previous, c) => last.end = at + c.len_utf8(),
                _ => clusters.push(at..at + c.len_utf8()),
            }
            previous = Some(c);
        }
        // characters of no script of their own take the script around them, as in the bar
        let mut pieces: Vec<Piece> = Vec::new();
        for cluster in clusters {
            let piece = &text[cluster.clone()];
            let face = self.sans_cluster_face(chosen, piece);
            let script = piece.chars().next().map_or(Script::Unknown, |c| c.script());
            let neutral = is_neutral(script);
            match pieces.last_mut() {
                Some(last)
                    if last.face == face
                        && (neutral || is_neutral(last.script) || last.script == script) =>
                {
                    last.range.end = cluster.end;
                    if !neutral {
                        last.script = script;
                    }
                }
                _ => pieces.push(Piece {
                    range: cluster,
                    face,
                    script,
                }),
            }
        }
        pieces
    }

    /// The face for a cluster: an emoji's from a color font first, else the chosen face where
    /// it has the character, else the first face found that draws it, else the chosen one,
    /// whose missing glyph then shows.
    fn sans_cluster_face(&mut self, chosen: usize, cluster: &str) -> usize {
        let Some(base) = cluster.chars().next() else {
            return chosen;
        };
        let emoji = !cluster.contains('\u{fe0e}')
            && (cluster.contains(['\u{fe0f}', '\u{200d}']) || presents_as_emoji(base));
        if !emoji && has(&self.sans.faces[chosen].font, base) {
            return chosen;
        }
        if let Some(&face) = self.sans.fallbacks.get(&(base, emoji)) {
            return face.unwrap_or(chosen);
        }
        let kinds = if emoji {
            [Kind::ColorBitmap, Kind::Outline]
        } else {
            [Kind::Outline, Kind::ColorBitmap]
        };
        let face = kinds.into_iter().find_map(|kind| {
            let loaded = (self.sans.faces.iter()).position(|face| draws(&face.font, base, kind));
            loaded.or_else(|| self.sans_search(base, kind))
        });
        self.sans.fallbacks.insert((base, emoji), face);
        face.unwrap_or(chosen)
    }

    /// Loads the first installed face that draws `c` as `kind`, upright and regular ones first.
    // shortcut: a character no installed face draws costs a pass over every face, on the thread
    // that draws, as the bar's search does; index the faces' character maps once if it shows
    fn sans_search(&mut self, c: char, kind: Kind) -> Option<usize> {
        let mut candidates: Vec<_> = (self.database.faces())
            .filter(|info| !self.sans.faces.iter().any(|face| face.id == info.id))
            .map(|info| {
                (
                    info.style != Style::Normal,
                    info.weight.0.abs_diff(400),
                    info.id,
                )
            })
            .collect();
        candidates.sort_by_key(|&(slanted, weight, _)| (slanted, weight));
        let (.., id) = candidates.into_iter().find(|&(.., id)| {
            let found = |data: &[u8], index| {
                skrifa::FontRef::from_index(data, index).is_ok_and(|font| {
                    let Some(glyph) = font.charmap().map(c) else {
                        return false;
                    };
                    match kind {
                        // no placeholder of a last-resort face
                        Kind::Outline => {
                            !last_resort(&font) && font.outline_glyphs().get(glyph).is_some()
                        }
                        Kind::ColorBitmap => (BitmapStrikes::new(&font))
                            .glyph_for_size(Size::unscaled(), glyph)
                            .is_some_and(|bitmap| !matches!(bitmap.data, BitmapData::Mask(_))),
                    }
                })
            };
            self.database.with_face_data(id, found) == Some(true)
        })?;
        self.sans_load(id)
    }

    /// Shapes a piece onto the line from `pen`, in ems, and returns where the pen ends.
    fn sans_shape(
        &mut self,
        key: InstanceKey,
        text: &str,
        piece: &Piece,
        rtl: bool,
        pen: f32,
        glyphs: &mut Vec<Shaped>,
    ) -> f32 {
        let font = &self.sans_instance(key).font;
        let range = piece.range.clone();
        let mut buffer = Buffer::new();
        buffer.push_str(&text[range.clone()]);
        buffer.set_pre_context(&text[..range.start]);
        buffer.set_post_context(&text[range.end..]);
        buffer.set_direction(if rtl {
            Direction::RightToLeft
        } else {
            Direction::LeftToRight
        });
        buffer.set_script(harfrust_script(piece.script));
        if harfrust::shape(&ShaperFont::new(font), &mut buffer, ShapeOptions::new()).is_err() {
            return pen;
        }
        let upem = f32::from(font.units_per_em());
        let mut pen = pen;
        for (info, position) in buffer.glyph_infos().iter().zip(buffer.glyph_positions()) {
            glyphs.push(Shaped {
                key,
                glyph: info.glyph_id,
                x: pen + position.x_offset as f32 / upem,
                y: position.y_offset as f32 / upem,
            });
            pen += position.x_advance as f32 / upem;
        }
        pen
    }

    /// The color bitmap of `glyph` from the strike that suits `pixels` per em, decoded once.
    fn sans_bitmap(&mut self, face: usize, glyph: u32, pixels: f32) -> Option<&Bitmap> {
        let font = &self.sans.faces[face].font;
        let found = (font.bitmap_strikes()).select_bitmap(GlyphId::new(glyph), Some(pixels))?;
        let key = (face, glyph, found.ppem());
        self.sans.bitmaps.entry(key).or_insert_with(|| {
            let image = match found.pixels() {
                Pixels::Png(data) => Pixmap::decode_png(data).ok()?,
                Pixels::Bgra(bgra) => {
                    // premultiplied BGRA, as tiny-skia's pixels are but for the order
                    let mut data = bgra.data().to_vec();
                    data.as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .for_each(|p| p.swap(0, 2));
                    Pixmap::from_vec(data, IntSize::from_wh(bgra.width(), bgra.height())?)?
                }
                Pixels::Mask(_) => return None,
            };
            Some(Bitmap {
                image,
                bearing: (found.x_bearing() as f32, found.y_bearing() as f32),
                inner: (
                    found.inner_x_bearing() as f32,
                    found.inner_y_bearing() as f32,
                ),
                ppem: (f32::from(found.ppem_x()), f32::from(found.ppem_y())),
                origin: found.origin(),
            })
        });
        self.sans.bitmaps[&key].as_ref()
    }
}

/// The face of `family` nearest `weight`: a family named exactly, else without regard to case,
/// or a generic one.
fn find(database: &Database, family: &str, weight: u16) -> Option<ID> {
    let query = |family: Family| {
        database.query(&Query {
            families: &[family],
            weight: fontdb::Weight(weight),
            ..Query::default()
        })
    };
    match family.to_lowercase().as_str() {
        "sans-serif" => query(Family::SansSerif),
        "serif" => query(Family::Serif),
        "monospace" => query(Family::Monospace),
        wanted => query(Family::Name(family)).or_else(|| {
            (database.faces().flat_map(|info| &info.families))
                .find(|(name, _)| name.to_lowercase() == wanted)
                .and_then(|(name, _)| query(Family::Name(name)))
        }),
    }
}

fn has(font: &skrifa::Font, c: char) -> bool {
    font.charmap().map_unicode(c).is_some()
}

fn draws(font: &skrifa::Font, c: char, kind: Kind) -> bool {
    let Some(glyph) = font.charmap().map_unicode(c) else {
        return false;
    };
    match kind {
        Kind::Outline => font.outline_glyphs().get(glyph).is_some(),
        Kind::ColorBitmap => (font.bitmap_strikes().select_bitmap(glyph, None))
            .is_some_and(|bitmap| !matches!(bitmap.pixels(), Pixels::Mask(_))),
    }
}

fn outline(scaler: &ScalerFont, glyph: u32) -> Option<tiny_skia::Path> {
    let outline = scaler.outline(GlyphId::new(glyph))?;
    let mut pen = Pen(PathBuilder::new());
    outline.draw(DrawMemory::Scratch, &mut pen).ok()?;
    pen.0.finish()
}

/// Collects an outline in font units, y upward, as the font gives it.
struct Pen(PathBuilder);

impl PathSink for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, cy0, x, y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.cubic_to(cx0, cy0, cx1, cy1, x, y);
    }

    fn close(&mut self) {
        self.0.close();
    }
}

/// `c` belongs to the cluster before it: a joiner and what it joins, a variation selector, a
/// keycap, a skin tone, a tag, a regional indicator after another, or a combining mark.
fn continues(previous: Option<char>, c: char) -> bool {
    let regional = |c: char| ('\u{1f1e6}'..='\u{1f1ff}').contains(&c);
    previous == Some('\u{200d}')
        || c == '\u{200d}'
        || c == '\u{20e3}'
        || ('\u{fe00}'..='\u{fe0f}').contains(&c)
        || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
        || ('\u{e0020}'..='\u{e007f}').contains(&c)
        || previous.is_some_and(regional) && regional(c)
        || c.script() == Script::Inherited
}

/// Characters that show as emoji with no variation selector: Unicode's `Emoji_Presentation`
/// in the Basic Multilingual Plane, and the blocks of U+1F000 to U+1FAFF, where emoji are.
// shortcut: in those blocks a few symbols without Emoji_Presentation (mahjong tiles, playing
// cards, enclosed letters) count as emoji and prefer a color font; take Unicode's table
// whole if one of them shows wrongly
fn presents_as_emoji(c: char) -> bool {
    const BMP: [(u32, u32); 31] = [
        (0x231a, 0x231b),
        (0x23e9, 0x23ec),
        (0x23f0, 0x23f0),
        (0x23f3, 0x23f3),
        (0x25fd, 0x25fe),
        (0x2614, 0x2615),
        (0x2648, 0x2653),
        (0x267f, 0x267f),
        (0x2693, 0x2693),
        (0x26a1, 0x26a1),
        (0x26aa, 0x26ab),
        (0x26bd, 0x26be),
        (0x26c4, 0x26c5),
        (0x26ce, 0x26ce),
        (0x26d4, 0x26d4),
        (0x26ea, 0x26ea),
        (0x26f2, 0x26f3),
        (0x26f5, 0x26f5),
        (0x26fa, 0x26fa),
        (0x26fd, 0x26fd),
        (0x2705, 0x2705),
        (0x270a, 0x270b),
        (0x2728, 0x2728),
        (0x274c, 0x274c),
        (0x274e, 0x274e),
        (0x2753, 0x2755),
        (0x2757, 0x2757),
        (0x2795, 0x2797),
        (0x27b0, 0x27b0),
        (0x27bf, 0x27bf),
        (0x2b1b, 0x2b1c),
    ];
    let c = u32::from(c);
    (0x1f000..=0x1faff).contains(&c)
        || c == 0x2b50
        || c == 0x2b55
        || BMP.iter().any(|&(a, b)| (a..=b).contains(&c))
}

#[cfg(test)]
mod tests {
    use super::presents_as_emoji;

    #[test]
    fn only_the_emoji_blocks_past_the_basic_plane_show_as_emoji() {
        assert!(presents_as_emoji('\u{1f525}'), "a fire");
        assert!(
            !presents_as_emoji('\u{20000}'),
            "an ideograph of Extension B"
        );
        assert!(
            !presents_as_emoji('\u{1fb00}'),
            "a block mosaic of legacy computing"
        );
    }
}
