//! Text drawn with DejaVu Sans Mono, which `ci/check.sh` installs as `fonts-dejavu-core`,
//! named so no other installed font changes what these tests see. The tests of fallback
//! faces and right-to-left runs are unit tests over the DejaVu faces alone.

use std::sync::mpsc;
use std::time::Duration;

use nsxiv_ng_render::canvas::rgb;
use nsxiv_ng_render::text::Text;
use nsxiv_ng_render::{Canvas, bar};

const BLACK: u32 = 0xff00_0000;
const W: u32 = 160;
const H: u32 = 48;
const ALL: [i32; 4] = [0, 0, W as i32, H as i32];

/// The text once its fonts have loaded.
fn text(family: &str) -> Text {
    let (ready, loaded) = mpsc::channel();
    let mut text = Text::new(family, 13.0, move || ready.send(()).unwrap());
    loaded.recv_timeout(Duration::from_secs(60)).unwrap();
    assert!(
        text.font(1.0).is_some(),
        "no font: the text tests need fonts-dejavu-core"
    );
    text
}

/// `s` drawn in black on a clear canvas at `x`, `y`, within `clip`.
fn drawn(text: &mut Text, scale: f64, s: &str, x: i32, y: i32, clip: [i32; 4]) -> Vec<u32> {
    let mut pixels = vec![0; (W * H) as usize];
    let mut canvas = Canvas::new(&mut pixels, W, H).unwrap();
    text.font(scale)
        .unwrap()
        .draw(&mut canvas, s, x, y, BLACK, clip);
    pixels
}

/// The columns and rows that hold ink, as ranges, counting pixels covered more than `least`.
fn ink_over(pixels: &[u32], least: u32) -> ([u32; 2], [u32; 2]) {
    let inked = (0..W * H).filter(|&i| pixels[i as usize] >> 24 > least);
    let (mut columns, mut rows) = ([W, 0], [H, 0]);
    for i in inked {
        let (column, row) = (i % W, i / W);
        columns = [columns[0].min(column), columns[1].max(column + 1)];
        rows = [rows[0].min(row), rows[1].max(row + 1)];
    }
    (columns, rows)
}

fn ink(pixels: &[u32]) -> ([u32; 2], [u32; 2]) {
    ink_over(pixels, 0)
}

#[test]
fn the_measured_width_is_the_drawn_extent() {
    let mut text = text("DejaVu Sans Mono");
    // full blocks fill their advance, so the columns they cover at least half span exactly
    // the width measured; DejaVu's blocks reach a quarter pixel further left at 26 px
    for (scale, s) in [(1.0, "█"), (1.0, "███"), (2.0, "████"), (1.5, "██")] {
        let width = text.font(scale).unwrap().width(s) as u32;
        let frame = drawn(&mut text, scale, s, 5, 3, ALL);
        assert_eq!(ink_over(&frame, 127).0, [5, 5 + width], "{s} at {scale}");
        let [left, right] = ink(&frame).0;
        assert!(
            left >= 4 && right <= 6 + width,
            "{s} at {scale}: {left}..{right}"
        );
    }
    // a monospaced font advances in whole pixels, every character alike
    let mut font = text.font(1.0).unwrap();
    assert_eq!(font.width("mmmm"), 4 * font.width("i"));
    assert_eq!(font.width(""), 0);
}

#[test]
fn text_stays_inside_its_box() {
    let mut text = text("DejaVu Sans Mono");
    for s in ["Ag", "ÅÉ|gjpq_", "█▀▄", "photo.jpg 1/4"] {
        let mut font = text.font(1.0).unwrap();
        let (width, height) = (font.width(s) as u32, font.height() as u32);
        let (columns, rows) = ink(&drawn(&mut text, 1.0, s, 7, 9, ALL));
        assert!(
            columns[0] >= 7 && columns[1] <= 7 + width,
            "{s}: {columns:?}"
        );
        assert!(
            rows[0] >= 9 && rows[1] <= 9 + height,
            "{s}: {rows:?} in {height}"
        );
    }
}

#[test]
fn drawing_changes_nothing_outside_the_clip() {
    let mut text = text("DejaVu Sans Mono");
    let whole = drawn(&mut text, 1.0, "████ Ag", 2, 2, ALL);
    let clip = [10, 6, 12, 7];
    let clipped = drawn(&mut text, 1.0, "████ Ag", 2, 2, clip);
    for (i, (&pixel, &unclipped)) in clipped.iter().zip(&whole).enumerate() {
        let (x, y) = ((i as u32 % W) as i32, (i as u32 / W) as i32);
        let inside =
            (clip[0]..clip[0] + clip[2]).contains(&x) && (clip[1]..clip[1] + clip[3]).contains(&y);
        assert_eq!(pixel, if inside { unclipped } else { 0 }, "{x}, {y}");
    }
    // and nothing off the canvas, nor a clip that leaves it, breaks drawing
    drawn(&mut text, 1.0, "████", -9, -9, [-50, -50, 500, 500]);
    drawn(&mut text, 1.0, "████", W as i32 - 3, H as i32 - 3, ALL);
}

#[test]
fn covered_pixels_take_the_color_and_the_rest_blend() {
    let mut text = text("DejaVu Sans Mono");
    let mut pixels = vec![rgb(255, 255, 255); (W * H) as usize];
    let mut canvas = Canvas::new(&mut pixels, W, H).unwrap();
    let red = rgb(255, 0, 0);
    let mut font = text.font(1.0).unwrap();
    let height = font.height();
    font.draw(&mut canvas, "█a", 0, 0, red, ALL);
    // inside the block, red; where 'a' has edges, a mix of red and white
    assert_eq!(pixels[(height as u32 / 2 * W + 3) as usize], red);
    assert!(
        pixels
            .iter()
            .any(|&p| p >> 16 & 0xff == 255 && (1..255).contains(&(p & 0xff)))
    );
    assert!(
        pixels
            .iter()
            .all(|&p| p >> 24 == 255 && p >> 16 & 0xff == 255)
    );
}

#[test]
fn a_size_past_the_limits_gives_no_font() {
    let mut text = text("DejaVu Sans Mono");
    // 13 × 78.7 rounds to 1023, and 13 × 79 to 1027
    assert!(text.font(78.7).is_some());
    for scale in [79.0, f64::INFINITY, f64::NAN, 0.03, -1.0, f64::NEG_INFINITY] {
        assert!(text.font(scale).is_none(), "{scale}");
    }
}

#[test]
fn an_origin_at_the_ends_of_the_coordinates_draws_nothing() {
    let mut text = text("DejaVu Sans Mono");
    let none = [0, 0, 0, 0];
    for (x, y) in [
        (0, i32::MAX),
        (i32::MAX, 0),
        (i32::MIN, i32::MIN),
        (i32::MAX, i32::MAX),
    ] {
        for clip in [none, ALL, [i32::MIN, i32::MIN, i32::MAX, i32::MAX]] {
            for s in ["", "Ag█"] {
                assert!(drawn(&mut text, 1.0, s, x, y, clip).iter().all(|&p| p == 0));
            }
        }
    }
}

const GRAY: u32 = 0xffee_eeee;

#[test]
fn the_status_bar_fits_the_right_part_and_cuts_the_left() {
    let mut text = text("DejaVu Sans Mono");
    let height = bar::height(&mut text).unwrap();
    let mut font = text.font(1.0).unwrap();
    assert_eq!(height, f64::from(font.height() + 8));
    let (a, line) = (font.width("a"), font.height());
    assert_eq!(font.width("…"), a);
    // room for "100% | 2/4" but not the whole right part, and eight characters on the left
    let (width, height) = (24 + 18 * a, height as i32);
    let status = ("a-long-name.jpg", "200x200 | 100% | 2/4");
    let mut bar = vec![0; (width * height) as usize];
    let area = [0, 0, width, height];
    let mut canvas = Canvas::new(&mut bar, width as u32, height as u32).unwrap();
    bar::status(&mut canvas, &mut text, area, 1.0, status, (GRAY, BLACK));
    let mut expected = vec![GRAY; (width * height) as usize];
    let mut canvas = Canvas::new(&mut expected, width as u32, height as u32).unwrap();
    let mut font = text.font(1.0).unwrap();
    let top = (height - line) / 2;
    font.draw(
        &mut canvas,
        "100% | 2/4",
        width - 10 * a - 8,
        top,
        BLACK,
        area,
    );
    font.draw(&mut canvas, "…ame.jpg", 8, top, BLACK, area);
    assert!(bar == expected);
}

#[test]
fn the_title_bar_cuts_the_title_before_the_close_button() {
    let mut text = text("DejaVu Sans Mono");
    let mut font = text.font(2.0).unwrap();
    let (a, line) = (font.width("a"), font.height());
    // at scale 2 the padding is 16 pixels; room for six characters before the square
    let (width, height) = (32 + 6 * a + 40, 40);
    let area = [0, 0, width, height];
    let mut title = vec![0; (width * height) as usize];
    let mut canvas = Canvas::new(&mut title, width as u32, height as u32).unwrap();
    bar::title(
        &mut canvas,
        &mut text,
        "nsxiv-ng: photo.jpg",
        2.0,
        (GRAY, BLACK),
    );
    let mut expected = vec![GRAY; (width * height) as usize];
    let mut canvas = Canvas::new(&mut expected, width as u32, height as u32).unwrap();
    let mut font = text.font(2.0).unwrap();
    let top = (height - line) / 2;
    font.draw(&mut canvas, "×", width - (height + a) / 2, top, BLACK, area);
    font.draw(&mut canvas, "…o.jpg", 16, top, BLACK, area);
    assert!(title == expected);
}

#[test]
fn what_went_wrong_is_centered_in_its_area() {
    let mut text = text("DejaVu Sans Mono");
    let message = "could not load: unknown format";
    let area = [10, 4, 140, 30];
    let mut pixels = vec![0; (W * H) as usize];
    let mut canvas = Canvas::new(&mut pixels, W, H).unwrap();
    bar::error(&mut canvas, &mut text, message, area, 1.0, BLACK);
    let mut font = text.font(1.0).unwrap();
    let (width, line) = (font.width(message), font.height());
    let expected = drawn(
        &mut text,
        1.0,
        message,
        10 + (140 - width) / 2,
        4 + (30 - line) / 2,
        area,
    );
    assert!(pixels == expected);
    // wider than its area, it is cut at the area's edges
    let ([left, right], _) = ink(&pixels);
    assert!(left >= 10 && right <= 150);
}
