//! Fallback faces, right-to-left runs and the limits on glyphs, over the DejaVu faces alone,
//! which `ci/check.sh` installs as `fonts-dejavu-core`, so other installed fonts change
//! nothing here. DejaVu Sans Mono lacks Hebrew and some Arabic letters that DejaVu Sans has.

use std::path::Path;
use std::sync::OnceLock;

use fontdb::Database;
use skrifa::ScalerFont;

use super::fonts::Fonts;
use super::raster::rasterize;
use super::{Text, shape::Placed};
use crate::Canvas;

const BLACK: u32 = 0xff00_0000;
const W: u32 = 160;
const H: u32 = 48;
const ALL: [i32; 4] = [0, 0, W as i32, H as i32];

/// The system's DejaVu faces from the files `fonts-dejavu-core` has, and no other: Homebrew's
/// cask on macOS adds condensed, light and slanted ones, as `fonts-dejavu-extra` does.
fn dejavu() -> Database {
    const CORE: [&str; 6] = [
        "DejaVuSans.ttf",
        "DejaVuSans-Bold.ttf",
        "DejaVuSansMono.ttf",
        "DejaVuSansMono-Bold.ttf",
        "DejaVuSerif.ttf",
        "DejaVuSerif-Bold.ttf",
    ];
    static DEJAVU: OnceLock<Database> = OnceLock::new();
    let database = DEJAVU.get_or_init(|| {
        let mut database = Database::new();
        database.load_system_fonts();
        let core = |source: &fontdb::Source| match source {
            fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => path
                .file_name()
                .is_some_and(|name| CORE.iter().any(|core| name == *core)),
            fontdb::Source::Binary(_) => false,
        };
        let dejavu = |face: &fontdb::FaceInfo| {
            core(&face.source) && (face.families.iter()).any(|(name, _)| name.starts_with("DejaVu"))
        };
        let others: Vec<_> = (database.faces())
            .filter(|face| !dejavu(face))
            .map(|face| face.id)
            .collect();
        others.into_iter().for_each(|id| database.remove_face(id));
        assert!(
            !database.is_empty(),
            "the text tests need fonts-dejavu-core"
        );
        database
    });
    database.clone()
}

/// Text in `family` at 13 px over the DejaVu faces, with `monospace` as fontdb names it
/// where it reads no fontconfig files.
fn text(family: &str) -> Text {
    let mut database = dejavu();
    database.set_monospace_family("Courier New");
    Text {
        family: family.to_owned(),
        size: 13.0,
        loading: None,
        fonts: Fonts::new(database, family),
        map: None,
    }
}

fn family(text: &Text, face: usize) -> String {
    let fonts = text.fonts.as_ref().unwrap();
    let id = fonts.faces[face].id;
    fonts.database.face(id).unwrap().families[0].0.clone()
}

fn layout(text: &mut Text, s: &str) -> Vec<Placed> {
    text.fonts.as_mut().unwrap().layout(s, 13).0
}

/// `s` drawn in black on a clear canvas at `x`, `y`.
fn drawn(text: &mut Text, s: &str, x: i32, y: i32) -> Vec<u32> {
    let mut pixels = vec![0; (W * H) as usize];
    let mut canvas = Canvas::new(&mut pixels, W, H).unwrap();
    text.font(1.0)
        .unwrap()
        .draw(&mut canvas, s, x, y, BLACK, ALL);
    pixels
}

/// The inked part of a frame, row by row, which is where it is drawn put aside.
fn crop(pixels: &[u32]) -> Vec<Vec<u32>> {
    let inked = |i: &u32| pixels[*i as usize] != 0;
    let columns: Vec<_> = (0..W * H).filter(inked).map(|i| i % W).collect();
    let rows: Vec<_> = (0..W * H).filter(inked).map(|i| i / W).collect();
    let [left, right] = [columns.iter().min(), columns.iter().max()].map(|c| *c.unwrap());
    let [top, bottom] = [rows[0], rows[rows.len() - 1]];
    (top..=bottom)
        .map(|row| pixels[(row * W + left) as usize..=(row * W + right) as usize].to_vec())
        .collect()
}

/// The first column that holds ink.
fn left(pixels: &[u32]) -> u32 {
    (0..W)
        .find(|&column| (0..H).any(|row| pixels[(row * W + column) as usize] != 0))
        .unwrap()
}

#[test]
fn a_missing_family_falls_back_to_a_monospaced_face() {
    for name in ["DejaVu Sans Mono", "monospace", "No Such Family"] {
        assert_eq!(family(&text(name), 0), "DejaVu Sans Mono", "{name}");
    }
    assert_eq!(family(&text("DejaVu Sans"), 0), "DejaVu Sans");
}

#[test]
fn a_family_is_found_whatever_its_case_and_one_not_installed_names_its_substitute() {
    let mut lower = text("dejavu sans");
    assert_eq!(family(&lower, 0), "DejaVu Sans", "as fontconfig matches");
    assert_eq!(lower.substitute(), None);
    assert_eq!(
        text("monospace").substitute(),
        None,
        "a generic family has no name"
    );
    assert_eq!(
        text("No Such Family").substitute(),
        Some("DejaVu Sans Mono")
    );
    // the generic names in any case
    for generic in ["MONOSPACE", "Sans-Serif", "Serif"] {
        assert_eq!(text(generic).substitute(), None, "{generic}");
    }
    assert_eq!(
        family(&text("Sans-Serif"), 0),
        family(&text("sans-serif"), 0)
    );
}

#[test]
fn of_two_families_that_differ_by_case_the_one_named_exactly_serves() {
    // DejaVu Sans again under a name in upper case, listed after it
    let mut database = dejavu();
    let sans = (database.faces())
        .find(|face| {
            face.families[0].0 == "DejaVu Sans"
                && face.weight.0 == 400
                && face.style == fontdb::Style::Normal
        })
        .cloned()
        .unwrap();
    let original = sans.id;
    let mut upper = sans;
    upper.families = vec![(
        "DEJAVU SANS".to_owned(),
        fontdb::Language::English_UnitedStates,
    )];
    let pushed = database.push_face_info(upper);
    let face = |name: &str| Fonts::new(database.clone(), name).unwrap().faces[0].id;
    assert_eq!(face("DEJAVU SANS"), pushed);
    assert_eq!(face("DejaVu Sans"), original);
}

#[test]
fn a_character_the_bar_font_lacks_comes_from_another_font() {
    let mut mono = text("DejaVu Sans Mono");
    let mut sans = text("DejaVu Sans");
    // alef, which DejaVu Sans has, among Latin the bar font keeps; and a private character
    // no font has, which shows the bar font's missing glyph
    let faces: Vec<_> = layout(&mut mono, "aאa").iter().map(|g| g.face).collect();
    assert_eq!(faces, [0, 1, 0]);
    assert_eq!(family(&mono, 1), "DejaVu Sans");
    let missing = layout(&mut mono, "\u{10fffd}");
    assert_eq!((missing[0].face, missing[0].glyph), (0, 0));
    // the fallback face draws and measures it as the font of its own family does, on the
    // bar font's baseline
    assert!(crop(&drawn(&mut mono, "א", 4, 4)) == crop(&drawn(&mut sans, "א", 4, 4)));
    let alef = sans.font(1.0).unwrap().width("א");
    let mut font = mono.font(1.0).unwrap();
    assert_eq!(font.width("aאa"), 2 * font.width("a") + alef);
}

#[test]
fn right_to_left_text_is_drawn_in_visual_order() {
    let mut text = text("DejaVu Sans Mono");
    let mut font = text.font(1.0).unwrap();
    let (alef, bet) = (font.width("א"), font.width("ב"));
    assert_eq!(font.width("אב"), alef + bet);
    // alef comes first in memory and shows at the right, bet to its left
    let word = drawn(&mut text, "אב", 4, 4);
    let mut apart = drawn(&mut text, "ב", 4, 4);
    let first = drawn(&mut text, "א", 4 + bet, 4);
    apart.iter_mut().zip(first).for_each(|(p, q)| *p |= q);
    assert!(word == apart);
    let mut reversed = drawn(&mut text, "א", 4, 4);
    let second = drawn(&mut text, "ב", 4 + alef, 4);
    reversed.iter_mut().zip(second).for_each(|(p, q)| *p |= q);
    assert!(word != reversed);
    // in a left-to-right line, the Hebrew word keeps its order between the Latin words
    let a = text.font(1.0).unwrap().width("a ");
    let line = drawn(&mut text, "a אב z", 4, 4);
    let hebrew = drawn(&mut text, "אב", 4 + a, 4);
    let column = left(&hebrew);
    let start = |pixels: &[u32]| {
        (0..H)
            .map(|row| pixels[(row * W + column) as usize])
            .collect::<Vec<_>>()
    };
    assert!(start(&line) == start(&hebrew));
}

#[test]
fn letters_join_across_a_change_of_face() {
    let mut text = text("DejaVu Sans Mono");
    // beh, which DejaVu Sans Mono has, before feh with three dots below, which only DejaVu
    // Sans has: beh still joins the feh after it, so it takes its initial form, which is the
    // first of two behs, the rightmost glyph, and not the beh that stands alone
    let joined = layout(&mut text, "\u{628}\u{6a5}");
    let faces: Vec<_> = joined.iter().map(|g| g.face).collect();
    assert_eq!(faces, [1, 0]);
    let initial = layout(&mut text, "\u{628}\u{628}")[1].glyph;
    let alone = layout(&mut text, "\u{628}")[0].glyph;
    assert_ne!(initial, alone);
    assert_eq!(joined[1].glyph, initial);
    // and feh joins the beh before it, so it is not the feh that stands alone
    assert_ne!(joined[0].glyph, layout(&mut text, "\u{6a5}")[0].glyph);
}

#[test]
fn a_glyph_past_the_mask_limit_draws_nothing() {
    let text = text("DejaVu Sans Mono");
    let font = &text.fonts.as_ref().unwrap().faces[0].font;
    let glyph = font.charmap().map_unicode('M').unwrap().to_u32();
    let at = |pixels: f32| rasterize(&ScalerFont::unhinted(font.clone(), Some(pixels)), glyph);
    // M is about half an em wide and three quarters high: 1.5 million pixels at 2000 px,
    // 5.8 million at 4000; a mask of 10¹² bytes at 10⁶ px would abort the process if it
    // were allocated
    assert!(at(2000.0).is_some());
    assert!(at(4000.0).is_none());
    assert!(at(1.0e6).is_none());
}

#[test]
fn annotation_text_is_split_where_its_script_changes() {
    use unicode_script::Script;
    let mut sans = text("DejaVu Sans");
    let fonts = sans.fonts.as_mut().unwrap();
    let face = fonts.sans_face(&["DejaVu Sans".to_owned()], 400).unwrap();
    // DejaVu Sans has both: one face, a piece a script, what has none in the piece around it
    let line = "1 ab где.";
    let pieces = fonts.sans_pieces(face, line, 0..line.len());
    let split: Vec<_> = (pieces.iter())
        .map(|p| (&line[p.range.clone()], p.face, p.script))
        .collect();
    assert_eq!(
        split,
        [
            ("1 ab ", face, Script::Latin),
            ("где.", face, Script::Cyrillic)
        ]
    );
}

#[test]
fn annotation_faces_are_built_on_the_bytes_a_frontend_maps() {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    let families = ["DejaVu Sans".to_owned()];
    let width = |text: &mut Text| {
        let mut sans = text.sans(&families, super::Weight::Bold).unwrap();
        sans.width("Hello", 20.0)
    };
    let read = width(&mut text("DejaVu Sans Mono"));
    // the file's bytes as a map would give them, read here, and the files asked for
    let asked = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let seen = Arc::clone(&asked);
    let mut mapped = text("DejaVu Sans Mono");
    mapped.map_fonts(Arc::new(move |path| {
        seen.lock().unwrap().push(path.to_owned());
        let bytes: super::FontBytes = Arc::new(std::fs::read(path).ok()?);
        Some(bytes)
    }));
    assert_eq!(width(&mut mapped), read);
    let asked = asked.lock().unwrap();
    assert!(
        asked.len() == 1 && asked[0].ends_with("DejaVuSans-Bold.ttf"),
        "{asked:?}"
    );
}

#[test]
fn a_separator_after_a_character_from_another_face_is_the_bar_fonts() {
    // the reference mark, of no script, which only DejaVu Sans has, then a separator, which
    // the bar font has too
    let mut mono = text("DejaVu Sans Mono");
    let faces: Vec<_> = (layout(&mut mono, "a \u{203b} | a").iter())
        .map(|g| g.face)
        .collect();
    assert_eq!(faces, [0, 0, 1, 0, 0, 0, 0]);
    // and the space between two words of a script it lacks stays in their face, the Hebrew's
    let faces: Vec<_> = (layout(&mut mono, "\u{5d0}\u{5d1} \u{5d2}\u{5d3}").iter())
        .map(|g| g.face)
        .collect();
    assert_eq!(faces, [1; 5]);
}

#[test]
fn a_monospaced_face_that_draws_no_letter_is_not_taken_for_the_bars_monospace() {
    // a color emoji font, which says it is monospaced, found before DejaVu Sans Mono, as a
    // system may list it
    let emoji =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/stamp-emoji.ttf");
    let mut database = Database::new();
    database.load_font_file(emoji).unwrap();
    for face in dejavu().faces() {
        if let fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) = &face.source {
            database.load_font_file(path).unwrap();
        }
    }
    assert!(database.faces().next().unwrap().monospaced);
    database.set_monospace_family("Courier New");
    let text = Text {
        family: "monospace".to_owned(),
        size: 13.0,
        loading: None,
        fonts: Fonts::new(database, "monospace"),
        map: None,
    };
    assert_eq!(family(&text, 0), "DejaVu Sans Mono");
}

#[test]
fn a_status_line_with_an_emoji_draws_its_separators_in_the_bar_font() {
    // the system's fonts, where macOS's .LastResort maps the check mark to a box, and with it
    // the separator after it
    let (sender, ready) = std::sync::mpsc::channel();
    let mut text = Text::new("monospace", 13.0, move || sender.send(()).unwrap());
    ready.recv().unwrap();
    assert!(!text.loading());
    let line = "EDIT [stamp] | \u{2705} | w4 | red";
    let glyphs = text.fonts.as_mut().unwrap().layout(line, 13).0;
    assert_eq!(glyphs.len(), line.chars().count());
    let emoji = line.chars().position(|c| c == '\u{2705}').unwrap();
    let separator = text.fonts.as_mut().unwrap().layout("|", 13).0[0].glyph;
    for (i, glyph) in glyphs.iter().enumerate().filter(|&(i, _)| i != emoji) {
        assert_eq!(glyph.face, 0, "{i}");
    }
    assert_eq!(glyphs[emoji + 2].glyph, separator);
    // and no face loaded is a last-resort face
    let fonts = text.fonts.as_ref().unwrap();
    for face in &fonts.faces {
        let last = fonts.database.with_face_data(face.id, |data, index| {
            skrifa::FontRef::from_index(data, index)
                .is_ok_and(|font| super::fonts::last_resort(&font))
        });
        assert_eq!(last, Some(false));
    }
}

#[test]
fn after_a_joiner_a_separator_stays_in_the_face_before_where_it_has_it() {
    let mut mono = text("DejaVu Sans Mono");
    let fonts = mono.fonts.as_mut().unwrap();
    let mut pieces = |line: &str| -> Vec<_> {
        let pieces = fonts.pieces(line, 0..line.len());
        pieces.iter().map(|p| (p.range.clone(), p.face)).collect()
    };
    // the reference mark, which only DejaVu Sans has, joined to a bar the bar font has too, as
    // the parts of an emoji sequence are, and with a variation selector: one piece each
    assert_eq!(pieces("\u{203b}\u{200d}|"), [(0..7, 1)]);
    assert_eq!(pieces("\u{203b}\u{fe0f}"), [(0..6, 1)]);
    // a bar after the mark alone is the bar font's
    assert_eq!(pieces("\u{203b}|"), [(0..3, 1), (3..4, 0)]);
    // what the face before lacks still comes from the face that has it: the mark after a bar,
    // and the feh with three dots below after a beh, which only DejaVu Sans has
    assert_eq!(pieces("|\u{200d}\u{203b}"), [(0..4, 0), (4..7, 1)]);
    assert_eq!(pieces("\u{628}\u{200d}\u{6a5}"), [(0..5, 0), (5..7, 1)]);
    // and a joiner joins no two scripts in one piece
    assert_eq!(pieces("a\u{200d}\u{431}"), [(0..4, 0), (4..6, 0)]);
}
