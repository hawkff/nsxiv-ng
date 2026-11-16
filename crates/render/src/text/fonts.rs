//! The faces text draws with: the bar font first, then installed faces that serve the
//! characters it lacks, each found once and kept.

use std::collections::HashMap;

use fontdb::{Database, Family, ID, Query, Style};
use skrifa::raw::TableProvider;
use skrifa::raw::tables::cmap::CmapSubtable;
use skrifa::{FontRef, MetadataProvider, ScalerFont};

use super::raster::Glyph;
use super::sans::SansFonts;

pub(super) struct Face {
    pub(super) id: ID,
    pub(super) font: skrifa::Font,
}

impl Face {
    fn load(database: &Database, id: ID) -> Option<Face> {
        // shortcut: the face's whole file is copied into memory, tens of megabytes for a CJK
        // collection, because keeping it mapped takes unsafe code; map it from a crate that
        // may use unsafe code if the memory matters
        let font = database
            .with_face_data(id, |data, index| skrifa::Font::new(data.to_vec(), index))??;
        Some(Face { id, font })
    }

    pub(super) fn has(&self, c: char) -> bool {
        self.font.charmap().map_unicode(c).is_some()
    }

    /// Whether the face maps `c` to a glyph it has an outline for.
    fn draws(&self, c: char) -> bool {
        (self.font.charmap().map_unicode(c))
            .is_some_and(|glyph| self.font.outline_glyphs().get(glyph).is_some())
    }
}

pub(super) struct Fonts {
    pub(super) database: Database,
    /// The bar font, then the fallbacks in the order they were found.
    pub(super) faces: Vec<Face>,
    /// The face for each character the bar font lacks, or None where no face can draw it.
    fallbacks: HashMap<char, Option<usize>>,
    pub(super) plans: HashMap<(usize, harfrust::Script, harfrust::Direction), harfrust::ShapePlan>,
    pub(super) scalers: HashMap<(usize, u32), ScalerFont>,
    /// Masks by face, glyph and size in pixels; None for a glyph that draws nothing.
    // shortcut: masks are never evicted, which the characters of file names bound in
    // practice; evict the least recently used once long sessions show the memory grow
    pub(super) glyphs: HashMap<(usize, u32, u32), Option<Glyph>>,
    /// The family that serves where the family asked for by name is not installed.
    pub(super) substitute: Option<String>,
    /// Annotation text's faces, apart from the bar's, so the bar's fallbacks never change.
    pub(super) sans: SansFonts,
}

impl Fonts {
    /// The fonts for text in `family`, or None when no font is installed.
    pub(super) fn new(database: Database, family: &str) -> Option<Fonts> {
        let (id, substitute) = bar_face(&database, family)?;
        let face = Face::load(&database, id)?;
        Some(Fonts {
            substitute,
            database,
            faces: vec![face],
            fallbacks: HashMap::new(),
            plans: HashMap::new(),
            scalers: HashMap::new(),
            glyphs: HashMap::new(),
            sans: SansFonts::default(),
        })
    }

    /// The face that draws `c`: the bar font when it has it, else the first face found
    /// that can draw it, else the bar font, whose missing glyph then shows.
    pub(super) fn face_for(&mut self, c: char) -> usize {
        if self.faces[0].has(c) {
            return 0;
        }
        if let Some(&face) = self.fallbacks.get(&c) {
            return face.unwrap_or(0);
        }
        let face = (self.faces.iter().position(|face| face.draws(c))).or_else(|| self.search(c));
        self.fallbacks.insert(c, face);
        face.unwrap_or(0)
    }

    /// Loads the first installed face with an outline for `c`, upright and regular ones
    /// first, so the fallback looks like the bar font where it can, and no last-resort face
    /// ([`last_resort`]), whose glyphs are placeholders. A face that maps `c` to a glyph with
    /// no outline, as a color emoji font does, would draw nothing.
    // shortcut: an emoji in the bar shows as the bar font's missing glyph, as color glyphs
    // are annotation text's alone; drawing them here takes Sans's bitmap decoding for the
    // bar's faces, a color glyph beside the coverage mask in raster.rs and a blend of its
    // premultiplied pixels onto the canvas, some eighty lines, once file names with emoji
    // show it
    // shortcut: a character no installed face can draw costs one pass over every face, on
    // the thread that draws; index the faces' character maps once, off that thread, if that
    // pass shows in redraws
    fn search(&mut self, c: char) -> Option<usize> {
        let mut candidates: Vec<_> = self
            .database
            .faces()
            .filter(|info| !self.faces.iter().any(|face| face.id == info.id))
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
            let draws = |data: &[u8], index| {
                skrifa::FontRef::from_index(data, index).is_ok_and(|font| {
                    !last_resort(&font)
                        && (font.charmap().map(c))
                            .is_some_and(|glyph| font.outline_glyphs().get(glyph).is_some())
                })
            };
            self.database.with_face_data(id, draws) == Some(true)
        })?;
        self.faces.push(Face::load(&self.database, id)?);
        Some(self.faces.len() - 1)
    }
}

/// Whether the face maps characters through a cmap subtable of format 13, which OpenType
/// gives last-resort fonts: a glyph for each range, a placeholder for any character of it, as
/// macOS's `.LastResort` draws a box for every character of a block. Such a face maps every
/// character and draws none of them.
pub(super) fn last_resort(font: &FontRef) -> bool {
    (font.cmap().ok().and_then(|cmap| cmap.best_subtable()))
        .is_some_and(|(_, _, subtable)| matches!(subtable, CmapSubtable::Format13(_)))
}

/// The regular face of `family`, a name matched exactly, else without regard to case, as
/// fontconfig matches a family name; else of an installed monospaced family, else any
/// face. With the face, the family that serves instead of one asked for by name that is not
/// installed. The generic names go without regard to case too.
fn bar_face(database: &Database, family: &str) -> Option<(ID, Option<String>)> {
    let wanted = family.to_lowercase();
    let generic = match wanted.as_str() {
        "monospace" => Some(Family::Monospace),
        "sans-serif" => Some(Family::SansSerif),
        "serif" => Some(Family::Serif),
        _ => None,
    };
    let found = match generic {
        Some(generic) => regular(database, generic),
        None => regular(database, Family::Name(family)).or_else(|| {
            (database.faces().flat_map(|info| &info.families))
                .find(|(name, _)| name.to_lowercase() == wanted)
                .and_then(|(name, _)| regular(database, Family::Name(name)))
        }),
    };
    if let Some(id) = found {
        return Some((id, None));
    }
    // fontdb's own monospace family is Courier New, which Linux seldom has. A color emoji font
    // says it is monospaced too, and draws no letter: a face that draws one goes first
    let letters = |info: &&fontdb::FaceInfo| {
        let draws = |data: &[u8], index| {
            FontRef::from_index(data, index).is_ok_and(|font| {
                (font.charmap().map('a'))
                    .is_some_and(|glyph| font.outline_glyphs().get(glyph).is_some())
            })
        };
        database.with_face_data(info.id, draws) == Some(true)
    };
    let monospaced: Vec<_> = database.faces().filter(|info| info.monospaced).collect();
    let id = (monospaced.iter().copied().find(letters))
        .or(monospaced.first().copied())
        .and_then(|monospaced| regular(database, Family::Name(&monospaced.families.first()?.0)))
        .or_else(|| Some(database.faces().next()?.id))?;
    let used = (database.face(id)?.families.first()).map(|(name, _)| name.clone());
    Some((id, used.filter(|_| generic.is_none())))
}

fn regular(database: &Database, family: Family) -> Option<ID> {
    database.query(&Query {
        families: &[family],
        ..Query::default()
    })
}
