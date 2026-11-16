//! Annotation text through `Text::sans`, in DejaVu Sans, which `ci/check.sh` installs on both
//! systems, and the color emoji font each has: Noto Color Emoji, which it installs on Debian,
//! and Apple Color Emoji on macOS.

use std::sync::mpsc;

use nsxiv_ng_render::text::tiny_skia::{Color, Pixmap, Transform};
use nsxiv_ng_render::text::{Text, Weight};

/// The system's fonts, loaded.
fn text() -> Text {
    let (sender, ready) = mpsc::channel();
    let mut text = Text::new("DejaVu Sans", 13.0, move || sender.send(()).unwrap());
    ready.recv().unwrap();
    assert!(!text.loading());
    text
}

fn dejavu() -> Vec<String> {
    vec!["DejaVu Sans".to_owned()]
}

const LINE: &str = "Hello, edit mode";

#[test]
fn a_lines_width_scales_with_its_size() {
    let mut text = text();
    let mut sans = text.sans(&dejavu(), Weight::Regular).unwrap();
    let (small, large) = (sans.width(LINE, 20.0), sans.width(LINE, 50.0));
    // DejaVu Sans's advances are some 0.6 em a character
    assert!((150.0..250.0).contains(&small), "{small}");
    assert!((large / small - 2.5).abs() < 1e-4, "{small} {large}");
}

#[test]
fn bold_is_wider_than_regular() {
    let mut text = text();
    let regular = text
        .sans(&dejavu(), Weight::Regular)
        .unwrap()
        .width(LINE, 40.0);
    let bold = text
        .sans(&dejavu(), Weight::Bold)
        .unwrap()
        .width(LINE, 40.0);
    assert!(bold > regular * 1.05, "{regular} {bold}");
}

#[test]
fn a_family_not_installed_gives_way_to_the_next() {
    let mut text = text();
    let alone = text
        .sans(&dejavu(), Weight::Bold)
        .unwrap()
        .width(LINE, 40.0);
    let families = ["No Such Family".to_owned(), "dejavu sans".to_owned()];
    let after = text
        .sans(&families, Weight::Bold)
        .unwrap()
        .width(LINE, 40.0);
    assert_eq!(alone, after);
}

/// `line` drawn in black at 32 pixels per em onto a clear pixmap, as straight RGBA.
fn drawn(line: &str, transform: Transform) -> Pixmap {
    let mut text = text();
    let mut sans = text.sans(&dejavu(), Weight::Regular).unwrap();
    let mut pixmap = Pixmap::new(160, 80).unwrap();
    sans.draw(
        &mut pixmap.as_mut(),
        line,
        32.0,
        (8.0, 48.0),
        Color::BLACK,
        transform,
    );
    pixmap
}

/// The pixels at least half covered, and of those the ones of some color but black.
fn inked(pixmap: &Pixmap) -> (usize, usize) {
    let covered: Vec<_> = (pixmap.pixels().iter())
        .map(|p| p.demultiply())
        .filter(|p| p.alpha() >= 128)
        .collect();
    let colored = (covered.iter())
        .filter(|p| p.red().max(p.green()).max(p.blue()) > 64)
        .count();
    (covered.len(), colored)
}

#[test]
fn a_line_with_a_color_glyph_draws_pixels_not_of_the_texts_color() {
    let (covered, colored) = inked(&drawn("a", Transform::identity()));
    assert!(covered > 50 && colored == 0, "{covered} {colored}");
    // the check mark in its green box, from the color font the system has
    let (covered, colored) = inked(&drawn("a✅", Transform::identity()));
    assert!(colored > 200, "{covered} {colored}");
}

#[test]
fn the_transform_scales_outlines_and_color_glyphs_alike() {
    let (one, _) = inked(&drawn("a✅", Transform::identity()));
    let (half, _) = inked(&drawn("a✅", Transform::from_scale(0.5, 0.5)));
    // a quarter of the area, give or take the edges
    let ratio = half as f64 / one as f64;
    assert!((0.2..0.3).contains(&ratio), "{one} {half}");
}
