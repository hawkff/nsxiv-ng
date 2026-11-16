//! Text: fonts loaded off the main thread, measuring, and drawing shaped text.
//!
//! One line at a time, for the bar, the title bar and the text of what went wrong. fontdb
//! finds the system's fonts, unicode-bidi orders the runs of a line, harfrust shapes them,
//! skrifa gives the glyph outlines and tiny-skia fills them into coverage masks. Annotations
//! draw with [`Sans`] instead, at any size through a transform, color emoji included.

mod fonts;
mod raster;
mod sans;
mod shape;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// The 2D library [`Sans`] draws with, for callers that draw around its text.
pub use tiny_skia;
use tiny_skia::{Color, PixmapMut, Transform};

use crate::Canvas;
use fonts::Fonts;

/// The largest text in pixels, far past any bar, which keeps every glyph's mask small.
const MAX_PIXELS: f64 = 1024.0;

/// A font file's bytes, shared, as a map of the file is.
pub type FontBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// Maps a font file, or gives None where it is to be read into memory instead.
pub type MapFont = Arc<dyn Fn(&Path) -> Option<FontBytes> + Send + Sync>;

/// The bar font, a family and a size, over the system's fonts. Those load on a thread of
/// their own, because reading them takes tens to hundreds of milliseconds.
pub struct Text {
    family: String,
    size: f64,
    loading: Option<Receiver<fontdb::Database>>,
    fonts: Option<Fonts>,
    map: Option<MapFont>,
}

impl Text {
    /// Starts loading the system's fonts, for text in `family` at `size` logical pixels.
    /// `family` names an installed family, or `monospace`, `sans-serif` or `serif`; when it
    /// is not installed, an installed monospaced face serves. `ready` runs on the loading
    /// thread once the fonts have loaded, for the caller to draw again: until then
    /// [`Text::font`] gives None.
    pub fn new(family: &str, size: f64, ready: impl FnOnce() + Send + 'static) -> Text {
        let (loaded, loading) = mpsc::channel();
        std::thread::Builder::new()
            .name("fonts".to_owned())
            .spawn(move || {
                let mut fonts = fontdb::Database::new();
                fonts.load_system_fonts();
                // fontdb's generic monospace is Courier New; macOS's own monospaced face is Menlo
                #[cfg(target_os = "macos")]
                fonts.set_monospace_family("Menlo");
                // the text may be gone already, and then nobody waits
                let _ = loaded.send(fonts);
                ready();
            })
            .expect("a font thread");
        Text {
            family: family.to_owned(),
            size,
            loading: Some(loading),
            fonts: None,
            map: None,
        }
    }

    /// Text in `family` at `size` logical pixels over the faces of `database`, which are in at
    /// once: [`Text::new`] over these fonts rather than the system's.
    pub fn with_database(family: &str, size: f64, database: fontdb::Database) -> Text {
        Text {
            family: family.to_owned(),
            size,
            loading: None,
            fonts: Fonts::new(database, family),
            map: None,
        }
    }

    /// The bar font at the device scale, `size × scale` rounded to whole pixels, as Imlib2
    /// sizes a font. None while the fonts load, when no font is installed, and at a size that is
    /// not 1 to 1024 pixels.
    pub fn font(&mut self, scale: f64) -> Option<Font<'_>> {
        if self.loading() {
            return None;
        }
        let pixels = (self.size * scale).round();
        let fonts = (self.fonts.as_mut()).filter(|_| (1.0..=MAX_PIXELS).contains(&pixels))?;
        Some(Font {
            fonts,
            pixels: pixels as u32,
        })
    }

    /// The family asked for.
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Once the fonts have loaded, the family that serves where the one asked for by name is
    /// not installed; None for an installed family, a generic one, or none installed at all.
    pub fn substitute(&mut self) -> Option<&str> {
        if self.loading() {
            return None;
        }
        self.fonts.as_ref()?.substitute.as_deref()
    }

    /// Annotation text in the first installed of `families`, names or `sans-serif`, `serif`
    /// and `monospace`, at `weight`, else in the bar font. None while the fonts load and when
    /// no font is installed.
    pub fn sans(&mut self, families: &[String], weight: Weight) -> Option<Sans<'_>> {
        if self.loading() {
            return None;
        }
        let fonts = self.fonts.as_mut()?;
        let weight = match weight {
            Weight::Regular => 400,
            Weight::Bold => 700,
        };
        let face = fonts.sans_face(families, weight)?;
        Some(Sans {
            fonts,
            face,
            weight,
        })
    }

    /// Annotation text's faces from `map`'s bytes where it gives them, a map of the file
    /// rather than a copy, as for a large emoji font; read into memory where it gives none.
    pub fn map_fonts(&mut self, map: MapFont) {
        if let Some(fonts) = &mut self.fonts {
            fonts.sans.map = Some(Arc::clone(&map));
        }
        self.map = Some(map);
    }

    /// Whether the system's fonts are still loading: false once they have loaded, whether or
    /// not one serves the family, so a caller can tell a font still to come from none at all.
    pub fn loading(&mut self) -> bool {
        if let Some(loading) = &self.loading {
            match loading.try_recv() {
                Ok(database) => {
                    self.fonts = Fonts::new(database, &self.family);
                    if let Some(fonts) = &mut self.fonts {
                        fonts.sans.map.clone_from(&self.map);
                    }
                }
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => {}
            }
            self.loading = None;
        }
        false
    }
}

/// The bar font at one size in pixels, with the fonts that serve the characters it lacks.
pub struct Font<'a> {
    fonts: &'a mut Fonts,
    pixels: u32,
}

impl Font<'_> {
    /// The height of a line in pixels, from the top of the bar font's tallest glyph to the
    /// bottom of its deepest, as Imlib2 measures text. Text of any content has this height.
    pub fn height(&mut self) -> i32 {
        let (ascent, descent) = self.fonts.line(self.pixels);
        ascent + descent
    }

    /// The width of `text` in pixels: the sum of its glyphs' advances, each rounded to a
    /// whole pixel.
    pub fn width(&mut self, text: &str) -> i32 {
        self.fonts.layout(text, self.pixels).1
    }

    /// Draws `text` in `color`, a canvas pixel, with its line's top left corner at `x`, `y`.
    /// Only pixels inside `clip`, `[x, y, width, height]`, change.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: i32,
        y: i32,
        color: u32,
        clip: [i32; 4],
    ) {
        // in i64, so no origin on the canvas or off it overflows
        let baseline = i64::from(y) + i64::from(self.fonts.line(self.pixels).0);
        let (glyphs, _) = self.fonts.layout(text, self.pixels);
        for placed in glyphs {
            if let Some(glyph) = self.fonts.glyph(placed.face, placed.glyph, self.pixels) {
                raster::blend(
                    canvas,
                    glyph,
                    [
                        i64::from(x) + i64::from(placed.x),
                        baseline + i64::from(placed.y),
                    ],
                    color,
                    clip,
                );
            }
        }
    }
}

/// The weight of annotation text: a face of that weight where the family has one, or the
/// weight set on a variable face's `wght` axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Bold,
}

/// Annotation text in one face, shaped in font units and drawn as outlines through any
/// transform, so it measures the same at every size. Unlike the bar's [`Font`] it is not
/// hinted and not rounded to whole pixels. A character the face lacks comes from another
/// installed face, and one that shows as an emoji from a font of color bitmaps first.
// shortcut: a font whose emoji are COLR layers alone (no sbix or CBDT bitmaps) draws them as
// nothing or as their outlines; paint COLR glyphs (read-fonts' paint traversal over tiny-skia)
// once a system that ships only such a font gets edit mode
pub struct Sans<'a> {
    fonts: &'a mut Fonts,
    face: usize,
    weight: u16,
}

impl Sans<'_> {
    /// The width of `line` at `size` pixels per em: its advance, unrounded. Zero for a size
    /// that is not positive and finite.
    pub fn width(&mut self, line: &str, size: f32) -> f32 {
        if !(size > 0.0 && size.is_finite()) {
            return 0.0;
        }
        let key = self.fonts.sans_key(self.face, self.weight, size);
        self.fonts.sans_width(key, line) * size
    }

    /// Draws `line` at `size` pixels per em with its baseline's start at `origin`, through
    /// `transform`, onto premultiplied RGBA: outlines in `color`, color glyphs in their own.
    pub fn draw(
        &mut self,
        target: &mut PixmapMut,
        line: &str,
        size: f32,
        origin: (f32, f32),
        color: Color,
        transform: Transform,
    ) {
        if !(size > 0.0 && size.is_finite()) {
            return;
        }
        let key = self.fonts.sans_key(self.face, self.weight, size);
        (self.fonts).sans_draw(key, target, line, size, origin, (color, transform));
    }
}

#[cfg(test)]
mod loading_tests {
    use super::*;

    #[test]
    fn the_fonts_have_loaded_once_ready_ran_whether_or_not_one_serves() {
        for family in ["monospace", "no such family at all"] {
            let (sender, ready) = mpsc::channel();
            let mut text = Text::new(family, 13.0, move || sender.send(()).unwrap());
            ready.recv().unwrap();
            assert!(!text.loading(), "{family}");
            assert!(!text.loading(), "{family}: and stays so");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_generic_monospace_is_menlo_on_macos() {
        let (sender, ready) = mpsc::channel();
        let mut text = Text::new("monospace", 13.0, move || sender.send(()).unwrap());
        ready.recv().unwrap();
        assert!(!text.loading());
        let fonts = text.fonts.as_ref().unwrap();
        let face = fonts.database.face(fonts.faces[0].id).unwrap();
        assert_eq!(face.families[0].0, "Menlo");
    }
}
