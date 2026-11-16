//! One line laid out: unicode-bidi orders its runs, each run splits where the script or the
//! face changes, and harfrust shapes the pieces.

use std::ops::Range;

use harfrust::{Buffer, Direction, ShapeOptions, ShapePlan, ShaperFont, Tag};
use unicode_bidi::{BidiInfo, Level};
use unicode_script::{Script, UnicodeScript};

use super::fonts::Fonts;

/// A glyph on the line: its face and id, and its origin in pixels from the start of the
/// line on the baseline, y downward.
pub(super) struct Placed {
    pub(super) face: usize,
    pub(super) glyph: u32,
    pub(super) x: i32,
    pub(super) y: i32,
}

/// Part of a run that one face shapes in one script.
pub(super) struct Piece {
    pub(super) range: Range<usize>,
    pub(super) face: usize,
    pub(super) script: Script,
}

impl Fonts {
    /// The glyphs of `text` at `pixels` per em, left to right as they show, and the width of
    /// the line.
    pub(super) fn layout(&mut self, text: &str, pixels: u32) -> (Vec<Placed>, i32) {
        let mut glyphs = Vec::new();
        let mut pen = 0;
        // the bar reads left to right, so a line starting in Hebrew still starts at the left
        let bidi = BidiInfo::new(text, Some(Level::ltr()));
        for paragraph in &bidi.paragraphs {
            let (levels, runs) = bidi.visual_runs(paragraph, paragraph.range.clone());
            for run in runs {
                let rtl = levels[run.start].is_rtl();
                let mut pieces = self.pieces(text, run);
                if rtl {
                    pieces.reverse();
                }
                for piece in pieces {
                    pen = self.shape(text, &piece, rtl, pixels, pen, &mut glyphs);
                }
            }
        }
        (glyphs, pen)
    }

    /// The pieces of a run in logical order.
    pub(super) fn pieces(&mut self, text: &str, run: Range<usize>) -> Vec<Piece> {
        let mut pieces: Vec<Piece> = Vec::new();
        let mut joiner = false;
        for (offset, c) in text[run.clone()].char_indices() {
            let joined = std::mem::replace(&mut joiner, c == '\u{200d}');
            let range = run.start + offset..run.start + offset + c.len_utf8();
            let script = c.script();
            let neutral = is_neutral(script);
            let Some(last) = pieces.last_mut() else {
                let face = self.face_for(c);
                pieces.push(Piece {
                    range,
                    face,
                    script,
                });
                continue;
            };
            // marks, variation selectors and joiners stay with their base, and spaces and
            // punctuation in the face around them, but after a symbol or emoji, of no script,
            // from a fallback face in the bar font where it has them, so a separator after it
            // looks as every other separator; not after a joiner, as within an emoji sequence
            // shortcut: a mark the face before it lacks shows as the missing glyph, even where
            // another face has both the base and the mark; choose one face for the whole
            // cluster when such marks show up
            let face = if script == Script::Inherited {
                last.face
            } else if neutral
                && !joined
                && is_neutral(last.script)
                && last.face != 0
                && self.faces[0].has(c)
            {
                0
            } else if neutral && self.faces[last.face].has(c) {
                last.face
            } else {
                self.face_for(c)
            };
            if face == last.face && (neutral || is_neutral(last.script) || script == last.script) {
                last.range.end = range.end;
                if !neutral {
                    last.script = script;
                }
            } else {
                pieces.push(Piece {
                    range,
                    face,
                    script,
                });
            }
        }
        pieces
    }

    /// Shapes a piece onto the line from `pen`, and returns where the pen ends.
    fn shape(
        &mut self,
        text: &str,
        piece: &Piece,
        rtl: bool,
        pixels: u32,
        mut pen: i32,
        glyphs: &mut Vec<Placed>,
    ) -> i32 {
        let direction = if rtl {
            Direction::RightToLeft
        } else {
            Direction::LeftToRight
        };
        let script = harfrust_script(piece.script);
        let font = &self.faces[piece.face].font;
        let plan = self
            .plans
            .entry((
                piece.face,
                script.unwrap_or(harfrust::Script::UNKNOWN),
                direction,
            ))
            .or_insert_with(|| ShapePlan::new(font, direction, script, None, &[]));
        let mut buffer = Buffer::new();
        buffer.push_str(&text[piece.range.clone()]);
        // the text around the piece, for letters that join across a change of face
        buffer.set_pre_context(&text[..piece.range.start]);
        buffer.set_post_context(&text[piece.range.end..]);
        buffer.set_direction(direction);
        buffer.set_script(script);
        let options = ShapeOptions::new().plan(Some(plan));
        if harfrust::shape(&ShaperFont::new(font), &mut buffer, options).is_err() {
            return pen;
        }
        let scale = pixels as f32 / f32::from(font.units_per_em());
        let pixel = |units: i32| (units as f32 * scale).round() as i32;
        for (info, position) in buffer.glyph_infos().iter().zip(buffer.glyph_positions()) {
            glyphs.push(Placed {
                face: piece.face,
                glyph: info.glyph_id,
                x: pen + pixel(position.x_offset),
                y: -pixel(position.y_offset),
            });
            pen += pixel(position.x_advance);
        }
        pen
    }
}

/// A script as harfrust names it.
pub(super) fn harfrust_script(script: Script) -> Option<harfrust::Script> {
    <[u8; 4]>::try_from(script.short_name().as_bytes())
        .ok()
        .and_then(|name| harfrust::Script::from_iso15924_tag(Tag::new(&name)))
}

/// Characters of no script of their own, which take the script around them.
pub(super) fn is_neutral(script: Script) -> bool {
    matches!(script, Script::Common | Script::Inherited | Script::Unknown)
}
