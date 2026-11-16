//! The golden frames of `testdata/golden/edit/cases.tsv`: each case's annotations flattened
//! over the base, against a reference frame kept in the tree (kind `reference`) within a
//! bound, or against the viewer's own (kind `own`), whose text and emoji are also placed where
//! the reference frames place them. NSXIV_NG_BLESS=1 writes the viewer's own frames.

use std::path::{Path, PathBuf};

use nsxiv_ng_core::Point;
use nsxiv_ng_render::image_view::{Image, Turn};
use nsxiv_ng_render::text::tiny_skia::{IntSize, Pixmap};

use crate::testing::{families, text};
use crate::{Annotation, CensorMode, Edit, FillMode, PALETTE, RgbaImage, Tool};

/// Each reference case: the most of its channels, in percent, that may be more than 16 off
/// the reference frame, the largest mean error of a channel, and the least overlap there
/// must be between the pixels each drawing moved more than 48 off the base. What is left is
/// antialiasing at edges, where CoreGraphics covers a pixel a little differently.
const BOUNDS: &[(&str, f64, f64, f64)] = &[
    ("arrow", 0.6, 0.05, 0.97),
    ("rect-stroke", 0.1, 0.01, 0.99),
    ("rect-strokefill", 0.1, 0.2, 0.98),
    ("rect-fill", 0.1, 0.01, 0.99),
    ("ellipse-stroke", 0.6, 0.2, 0.97),
    ("ellipse-strokefill", 0.6, 0.3, 0.97),
    ("ellipse-fill", 0.3, 0.1, 0.99),
    ("pencil", 0.6, 0.1, 0.97),
    ("marker", 0.6, 0.05, 0.95),
    // the dim, black at 55%, rounds a little differently over every pixel outside the spots
    ("spotlight", 0.1, 0.3, 0.99),
    ("censor-solid", 0.0, 0.01, 1.0),
    // CoreImage's Gaussian against three boxes, both in linear light; a blur moves only a few
    // pixels far from the base, too few for their overlap to tell anything
    ("censor-blur", 0.1, 0.5, 0.0),
];

/// The viewer's own frames: the largest channel error, and the share of channels more than 2
/// off, in percent. Linux on x86-64 and macOS on arm64 draw them alike to the byte; the bound
/// leaves room for a rounding elsewhere.
const OWN_BOUNDS: (u8, f64) = (2, 0.05);

fn testdata(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/golden/edit")
        .join(path)
}

/// A case's annotations, from its column of the cases file.
fn parse(column: &str) -> Vec<Annotation> {
    let tools = [
        Tool::Arrow,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Text,
        Tool::Pencil,
        Tool::Marker,
        Tool::Badge,
        Tool::Stamp,
        Tool::Censor,
        Tool::Spotlight,
    ];
    let point = |xy: &str| {
        let (x, y) = xy.split_once(',').expect("x,y");
        Point {
            x: x.parse().unwrap(),
            y: y.parse().unwrap(),
        }
    };
    (column.split(" ; ").enumerate())
        .map(|(i, spec)| {
            let mut words = spec.split(' ');
            let name = words.next().unwrap();
            let tool = *tools.iter().find(|t| t.name() == name).expect("a tool");
            let mut a = Annotation::new(i as u64 + 1, tool);
            let (points, keys): (Vec<&str>, Vec<&str>) = words.partition(|w| !w.contains('='));
            a.start = point(points[0]);
            a.end = points.get(1).map_or(a.start, |&p| point(p));
            for key in keys {
                let (key, value) = key.split_once('=').unwrap();
                match key {
                    "width" => a.line_width = value.parse().unwrap(),
                    "fill" if value == "fill" => a.fill = FillMode::Fill,
                    "fill" => a.fill = FillMode::StrokeFill,
                    "censor" if value == "blur" => a.censor = CensorMode::Blur,
                    "censor" => a.censor = CensorMode::Solid,
                    "color" => a.color = PALETTE.iter().find(|c| c.0 == value).unwrap().1,
                    "number" => a.number = value.parse().unwrap(),
                    "font" => a.font_size = value.parse().unwrap(),
                    "text" => a.text = value.replace('_', " ").replace("\\n", "\n"),
                    "points" => a.points = value.split('/').map(point).collect(),
                    _ => panic!("no such key: {key}"),
                }
            }
            a
        })
        .collect()
}

/// The cases: name, kind and annotations.
fn cases() -> Vec<(String, String, Vec<Annotation>)> {
    let cases = std::fs::read_to_string(testdata("cases.tsv")).unwrap();
    (cases.lines())
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let columns: Vec<_> = line.split('\t').collect();
            (
                columns[0].to_owned(),
                columns[1].to_owned(),
                parse(columns[2]),
            )
        })
        .collect()
}

fn base() -> Pixmap {
    Pixmap::load_png(testdata("base.png")).unwrap()
}

/// An edit of the base, which is opaque, so its premultiplied pixels are its straight ones.
fn edit_of(base: &Pixmap) -> Edit {
    let levels = [base.data()];
    let image = Image {
        width: base.width(),
        height: base.height(),
        levels: &levels,
        opaque: true,
    };
    let source = PathBuf::from("base.png");
    Edit::new(&image, Turn::default(), None, u64::MAX, source, families()).unwrap()
}

fn flattened(annotations: Vec<Annotation>) -> RgbaImage {
    let (mut edit, mut text) = (edit_of(&base()), text());
    edit.annotations = annotations;
    edit.flattened(&mut text)
}

fn straight(pixmap: &Pixmap) -> Vec<u8> {
    pixmap.clone().take_demultiplied()
}

/// The pixels of `frame` more than 48 off the base in some channel.
fn marked(frame: &[u8], base: &[u8]) -> Vec<bool> {
    (frame.chunks(4).zip(base.chunks(4)))
        .map(|(p, b)| (0..3).any(|c| p[c].abs_diff(b[c]) > 48))
        .collect()
}

/// The box of the marked pixels: left, top, right, bottom, the last two past them.
fn ink(marks: &[bool], width: usize) -> [usize; 4] {
    let mut b = [usize::MAX, usize::MAX, 0, 0];
    for (i, _) in marks.iter().enumerate().filter(|(_, m)| **m) {
        let (x, y) = (i % width, i / width);
        b = [b[0].min(x), b[1].min(y), b[2].max(x + 1), b[3].max(y + 1)];
    }
    b
}

/// The golden frame at `path`, which must be `frame`'s size: pixels are compared in pairs,
/// and a smaller one would leave the rest unchecked.
fn golden_of(path: &Path, frame: &RgbaImage) -> Pixmap {
    let golden = Pixmap::load_png(path).unwrap();
    let sizes = (
        (golden.width(), golden.height()),
        (frame.width, frame.height),
    );
    assert_eq!(sizes.0, sizes.1, "{}", path.display());
    golden
}

/// The name of one of the viewer's own frames, which is the system's own where it holds emoji.
fn own(name: &str) -> String {
    if name.starts_with("stamp") {
        format!("{name}-{}.png", std::env::consts::OS)
    } else {
        format!("{name}.png")
    }
}

#[test]
fn annotations_draw_as_their_golden_frames() {
    let base = straight(&base());
    let bless = std::env::var_os("NSXIV_NG_BLESS").is_some();
    let mut failed = Vec::new();
    for (name, kind, annotations) in cases() {
        let frame = flattened(annotations);
        if kind == "own" {
            let path = testdata(&own(&name));
            let size = IntSize::from_wh(frame.width, frame.height).unwrap();
            let premultiplied = (frame.rgba.chunks(4))
                .flat_map(|p| {
                    let a = u32::from(p[3]);
                    [0, 1, 2]
                        .map(|c| ((u32::from(p[c]) * a + 127) / 255) as u8)
                        .into_iter()
                        .chain([p[3]])
                })
                .collect();
            if bless {
                Pixmap::from_vec(premultiplied, size)
                    .unwrap()
                    .save_png(&path)
                    .unwrap();
                continue;
            }
            let golden = straight(&golden_of(&path, &frame));
            let errors: Vec<u8> = (frame.rgba.iter().zip(&golden))
                .map(|(a, b)| a.abs_diff(*b))
                .collect();
            let largest = errors.iter().copied().max().unwrap_or(0);
            let off =
                errors.iter().filter(|&&e| e > 2).count() as f64 * 100.0 / errors.len() as f64;
            println!("{name}: largest {largest}, {off:.3}% off by more than 2");
            if largest > OWN_BOUNDS.0 || off > OWN_BOUNDS.1 {
                failed.push(format!("{name}: largest {largest}, {off:.3}%"));
            }
            continue;
        }
        let golden = straight(&golden_of(&testdata(&format!("{name}.png")), &frame));
        let channels = (frame.rgba.chunks(4).zip(golden.chunks(4)))
            .flat_map(|(a, b)| (0..3).map(move |c| a[c].abs_diff(b[c])));
        let (count, off, sum) = channels.fold((0, 0, 0), |(n, off, sum), e| {
            (n + 1, off + usize::from(e > 16), sum + u64::from(e))
        });
        let (off, mean) = (off as f64 * 100.0 / count as f64, sum as f64 / count as f64);
        let (ours, theirs) = (marked(&frame.rgba, &base), marked(&golden, &base));
        let both = ours.iter().zip(&theirs).filter(|(a, b)| **a && **b).count();
        let either = ours.iter().zip(&theirs).filter(|(a, b)| **a || **b).count();
        let overlap = if either == 0 {
            1.0
        } else {
            both as f64 / either as f64
        };
        println!("{name}: {off:.3}% off by more than 16, mean {mean:.3}, overlap {overlap:.3}");
        let &(_, most, largest_mean, least) =
            (BOUNDS.iter().find(|b| b.0 == name)).expect("a bound");
        if off > most || mean > largest_mean || overlap < least {
            failed.push(format!(
                "{name}: {off:.3}%, mean {mean:.3}, overlap {overlap:.3}"
            ));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Where the reference frames, drawn with CoreText, ink text, badges and stamps, measured once
/// from those frames: the box of the pixels more than 48 off the base, and how
/// far ours may be. DejaVu Sans is wider than San Francisco, so the width holds to a quarter
/// and the edges to 3 pixels. An emoji font's art is its own, and CoreText puts Apple Color
/// Emoji 0.12 em lower and further right than its bitmaps' bearings say, for a reason not
/// found: the stamps' edges hold to 0.15 em of their size, 28 × 2.4 and 10 × 2.4 pixels.
const PLACED: &[(&str, [usize; 4], usize)] = &[
    ("text", [25, 35, 150, 71], 3),
    ("badge", [42, 42, 214, 150], 3),
    ("stamp", [96, 65, 148, 132], 10),
    ("stamps", [10, 29, 238, 165], 4),
];

#[test]
fn text_badges_and_stamps_are_placed_where_the_reference_frames_place_them() {
    let base = straight(&base());
    let width = 256;
    let cases = cases();
    for &(name, theirs, tolerance) in PLACED {
        let annotations = cases.iter().find(|c| c.0 == name).unwrap().2.clone();
        let ours = ink(&marked(&flattened(annotations).rgba, &base), width);
        println!("{name}: ours {ours:?}, the reference's {theirs:?}");
        let near = |a: usize, b: usize| a.abs_diff(b) <= tolerance;
        let (our_width, their_width) = ((ours[2] - ours[0]) as f64, (theirs[2] - theirs[0]) as f64);
        assert!(
            near(ours[0], theirs[0])
                && near(ours[1], theirs[1])
                && near(ours[3], theirs[3])
                && (our_width / their_width - 1.0).abs() <= 0.25,
            "{name}: ours {ours:?}, the reference's {theirs:?}"
        );
    }
}
