//! What the tests share: the system's fonts, loaded, and the family they draw with.

use nsxiv_ng_render::text::Text;

/// The system's fonts, loaded, with DejaVu Sans, which `ci/check.sh` installs on both
/// systems, as the bar's font. On Linux the stamps' emoji come first from the tests' own font,
/// which the Linux frames of the stamps were drawn in, so whichever version of an emoji font
/// the system has draws them alike; macOS draws them in its own Apple Color Emoji.
#[cfg(target_os = "linux")]
pub(crate) fn text() -> Text {
    let emoji = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/stamp-emoji.ttf");
    let mut database = fontdb::Database::new();
    database.load_font_file(emoji).unwrap();
    database.load_system_fonts();
    Text::with_database("DejaVu Sans", 13.0, database)
}

/// The system's fonts, loaded, with DejaVu Sans, which `ci/check.sh` installs on both
/// systems, as the bar's font.
#[cfg(not(target_os = "linux"))]
pub(crate) fn text() -> Text {
    let (sender, ready) = std::sync::mpsc::channel();
    let mut text = Text::new("DejaVu Sans", 13.0, move || sender.send(()).unwrap());
    ready.recv().unwrap();
    assert!(!text.loading());
    text
}

/// The families annotation text asks for in the tests.
pub(crate) fn families() -> Vec<String> {
    vec!["DejaVu Sans".to_owned()]
}
