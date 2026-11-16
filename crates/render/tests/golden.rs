//! Frames checked against the golden frames, reference frames kept in the tree and drawn
//! through Imlib2, the library nsxiv draws with: a 256 × 192 canvas, the bar hidden, the keys
//! of each `frame` row of `testdata/golden/cases.tsv` through the core's Viewer, and every load
//! answered at once.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use nsxiv_ng_core::Size;
use nsxiv_ng_core::config::{CONTRAST_MAX, GAMMA_MAX};
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::Options;
use nsxiv_ng_core::viewer::{Effect, ImageInfo, Keystroke, Viewer, color_value};
use nsxiv_ng_core::viewport::Viewport;
use nsxiv_ng_decode::{Animation, decode};
use nsxiv_ng_render::image_view::{self, Image, Turn};
use nsxiv_ng_render::{Canvas, Colors, canvas};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 192;
const WHITE: u32 = canvas::rgb(255, 255, 255);

/// Each frame case's largest channel error and share of channels off by more than 2, in
/// percent, as measured against Imlib2's frames; none for a frame not asserted.
const BOUNDS: &[(&str, Option<(u8, f64)>)] = &[
    // the error left is Imlib2's 8-bit weights and sums, which it truncates where we round
    ("gradient-down", Some((1, 0.0))),
    ("gradient-down-noaa", Some((0, 0.0))),
    ("gradient-50", Some((1, 0.0))),
    ("gradient-150", Some((1, 0.0))),
    ("gradient-400", Some((1, 0.0))),
    ("gradient-400-noaa", Some((0, 0.0))),
    ("gradient-fill", Some((1, 0.0))),
    ("checker-down", Some((1, 0.0))),
    // the white strip at the right edge that Imlib2 once left is not in this frame
    ("checker-down-noaa", Some((0, 0.0))),
    ("checker-400", Some((3, 0.03))),
    ("checker-400-noaa", Some((0, 0.0))),
    // photo.jpg decodes differently in zune-jpeg and libjpeg-turbo, up to 9 a channel
    // before any scaling; gamma 3.25 steepens the levels near black, so a decoder
    // difference of 2 there becomes 57
    ("photo-down", Some((5, 0.72))),
    ("photo-gamma-up", Some((57, 6.3))),
    ("photo-gamma-down", Some((5, 0.59))),
    ("photo-contrast-up", Some((9, 7.71))),
    ("photo-contrast-down", Some((4, 0.21))),
    ("photo-turned-flipped", Some((4, 0.25))),
    // the reference frame's crop rounds outward, so its destination hangs off the canvas and
    // Imlib2's clipping samples an image some 1% larger than the zoom, drifting 2 pixels
    // across the frame; the viewer keeps the zoom (`a_pan_moves_the_frame_by_whole_pixels`)
    ("photo-150-panned", None),
    ("alpha-down", Some((0, 0.0))),
    ("alpha-checkerboard", Some((1, 0.0))),
    // Imlib2 averages straight alpha, so the clear black around the disk darkens its edge;
    // the viewer averages by alpha (`color_hidden_under_clear_pixels_stays_hidden_when_scaled`)
    ("alpha-50", Some((32, 0.08))),
    ("alpha-200-checkerboard", Some((19, 0.19))),
    ("moving-1", Some((0, 0.0))),
    ("moving-2", Some((0, 0.0))),
    ("moving-3", Some((0, 0.0))),
];

fn testdata(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(path)
}

fn golden(name: &str) -> Vec<u8> {
    let path = testdata(&format!("golden/{name}.png"));
    image::open(&path).unwrap().to_rgba8().into_raw()
}

/// A decoded frame: width, height and straight-alpha RGBA.
type Frame = (u32, u32, Vec<u8>);

struct Case {
    name: String,
    fixture: PathBuf,
    keys: Vec<Keystroke>,
}

/// The `frame` rows of cases.tsv, keys as nsxiv names them, `C-n` being Ctrl and n.
fn frame_cases() -> Vec<Case> {
    let text = std::fs::read_to_string(testdata("golden/cases.tsv")).unwrap();
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            (fields[1] == "frame").then(|| Case {
                name: fields[0].to_owned(),
                fixture: testdata(&format!("fixtures/{}", fields[2])),
                keys: fields
                    .get(3)
                    .map(|keys| {
                        keys.split(' ')
                            .map(|key| match key.strip_prefix("C-") {
                                Some(key) if !key.is_empty() => Keystroke::ctrl(key),
                                _ => Keystroke::new(key),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// The fixture's frames, every one for an animation, and their delays.
fn decoded(path: &Path) -> (Vec<Frame>, Vec<f64>) {
    let mut first = decode(path, 1 << 30).unwrap();
    // Imlib2's PNG loader keeps the high byte of a 16-bit sample, where ours rounds, so a
    // 16-bit fixture is cut as Imlib2 cuts it and the frames judge the drawing alone
    if let Ok(image::DynamicImage::ImageRgb16(deep)) = image::open(path) {
        first.rgba = deep
            .pixels()
            .flat_map(|p| [p[0], p[1], p[2], u16::MAX].map(|c| (c >> 8) as u8))
            .collect();
    }
    let still = || {
        (
            vec![(first.width, first.height, first.rgba.clone())],
            vec![0.1],
        )
    };
    if !first.may_animate {
        return still();
    }
    let (sender, receiver) = mpsc::channel();
    let animation = Animation::start(
        path.to_owned(),
        256 << 20,
        1 << 30,
        move |_, delays, complete| {
            sender.send((delays.to_vec(), complete)).unwrap();
        },
    );
    let mut delays = Vec::new();
    // a file of one frame gets no call, and its decoder's end closes the channel
    while let Ok((more, complete)) = receiver.recv_timeout(Duration::from_secs(30)) {
        delays.extend(more);
        if complete {
            break;
        }
    }
    if delays.len() < 2 {
        return still();
    }
    let frames = (0..delays.len())
        .map(|i| {
            let frame = animation.frame(i).unwrap();
            (frame.width, frame.height, frame.rgba.clone())
        })
        .collect();
    (frames, delays)
}

/// Runs the effects, answering each load with the image.
fn answer(viewer: &mut Viewer, effects: Vec<Effect>, info: &ImageInfo) {
    for effect in effects {
        if let Effect::Load { request, .. } = effect {
            let more = viewer.loaded(request, Ok(info.clone()));
            answer(viewer, more, info);
        }
    }
}

/// The viewer after the case's keys, and the fixture's frames.
fn replay(case: &Case) -> (Viewer, Vec<Frame>) {
    let (frames, delays) = decoded(&case.fixture);
    let info = ImageInfo {
        size: Size {
            width: f64::from(frames[0].0),
            height: f64::from(frames[0].1),
        },
        delays,
        complete: true,
    };
    let options = Options {
        no_bar: true,
        ..Options::default()
    };
    let mut viewer = Viewer::new(options, vec![FileEntry::new(case.fixture.clone())]);
    viewer.resized(Size {
        width: f64::from(WIDTH),
        height: f64::from(HEIGHT),
    });
    let effects = viewer.start();
    answer(&mut viewer, effects, &info);
    for key in &case.keys {
        let effects = viewer
            .key(key)
            .unwrap_or_else(|| panic!("{}: no binding for {key:?}", case.name));
        answer(&mut viewer, effects, &info);
    }
    (viewer, frames)
}

/// Level k + 1 of a pyramid from level k: each pixel the alpha-weighted average of its
/// block of up to 2 × 2, as the pyramid contract builds it.
fn halved((width, height, rgba): &Frame) -> Frame {
    let (w, h) = (width.div_ceil(2), height.div_ceil(2));
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (mut alpha, mut colors, mut count) = (0, [0; 3], 0);
            for (sx, sy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (sx, sy) = (2 * x + sx, 2 * y + sy);
                if sx < *width && sy < *height {
                    let at = ((sy * width + sx) * 4) as usize;
                    let a = u32::from(rgba[at + 3]);
                    for (c, total) in colors.iter_mut().enumerate() {
                        *total += u32::from(rgba[at + c]) * a;
                    }
                    alpha += a;
                    count += 1;
                }
            }
            for total in colors {
                // clear blocks get no color
                out.push((total + alpha / 2).checked_div(alpha).unwrap_or(0) as u8);
            }
            out.push(((alpha + count / 2) / count) as u8);
        }
    }
    (w, h, out)
}

fn pyramid(level0: &Frame) -> Vec<Frame> {
    let mut levels = vec![level0.clone()];
    while let Some(last) = levels.last().filter(|l| l.0 > 1 || l.1 > 1) {
        let next = halved(last);
        levels.push(next);
    }
    levels
}

/// The canvas the viewer shows, its image from `levels`, level 0 first.
fn draw(viewer: &Viewer, levels: &[Frame], background: u32) -> Vec<u32> {
    let (flip_h, flip_v) = viewer.flips();
    let turn = Turn {
        rotation: viewer.rotation(),
        flip_h,
        flip_v,
    };
    let colors = Colors::new(viewer.gamma_value(), viewer.contrast_value());
    draw_view(
        viewer.viewport(),
        turn,
        colors.as_ref(),
        levels,
        background,
        (WIDTH, HEIGHT),
    )
}

/// A canvas of `size` with the image from `levels` as `view` shows it.
fn draw_view(
    view: &Viewport,
    turn: Turn,
    colors: Option<&Colors>,
    levels: &[Frame],
    background: u32,
    (width, height): (u32, u32),
) -> Vec<u32> {
    let slices: Vec<&[u8]> = levels.iter().map(|l| l.2.as_slice()).collect();
    let image = Image {
        width: levels[0].0,
        height: levels[0].1,
        levels: &slices,
        opaque: levels[0].2.iter().skip(3).step_by(4).all(|&a| a == 255),
    };
    let mut pixels = vec![background; (width * height) as usize];
    let mut canvas = Canvas::new(&mut pixels, width, height).unwrap();
    image_view::draw(&mut canvas, &image, view, turn, colors, 1.0, false);
    pixels
}

/// A viewport of `view` showing an image of `image` at `zoom`, centered.
fn viewport(view: (u32, u32), image: (u32, u32), zoom: f64) -> Viewport {
    let mut v = Viewport::default();
    v.set_view_size(Size {
        width: f64::from(view.0),
        height: f64::from(view.1),
    });
    v.set_image_size(
        Size {
            width: f64::from(image.0),
            height: f64::from(image.1),
        },
        true,
    );
    v.set_zoom(zoom, None);
    v
}

fn rgba(pixel: u32) -> [u8; 4] {
    let [a, r, g, b] = pixel.to_be_bytes();
    [r, g, b, a]
}

/// The largest channel error and the percentage of channels off by more than 2, of the
/// color channels of opaque frames.
fn errors(frame: &[u32], expected: &[u32]) -> (u8, f64) {
    let (mut largest, mut off) = (0, 0);
    for (&a, &b) in frame.iter().zip(expected) {
        for (a, b) in rgba(a).into_iter().zip(rgba(b)).take(3) {
            let error = a.abs_diff(b);
            largest = largest.max(error);
            off += usize::from(error > 2);
        }
    }
    (largest, off as f64 * 100.0 / (frame.len() * 3) as f64)
}

fn golden_frame(name: &str) -> Vec<u32> {
    golden(name)
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&[r, g, b, _]| canvas::rgb(r, g, b))
        .collect()
}

#[test]
fn frames_match_imlib2_within_their_bounds() {
    let cases = frame_cases();
    assert_eq!(cases.len(), BOUNDS.len(), "every frame case has bounds");
    let mut failed = Vec::new();
    for case in &cases {
        let (viewer, frames) = replay(case);
        let frame = draw(
            &viewer,
            std::slice::from_ref(&frames[viewer.frame_index()]),
            WHITE,
        );
        let (largest, share) = errors(&frame, &golden_frame(&case.name));
        println!(
            "{}: largest error {largest}, {share:.3}% off by more than 2",
            case.name
        );
        let &(_, bounds) = BOUNDS
            .iter()
            .find(|(name, _)| *name == case.name)
            .unwrap_or_else(|| panic!("no bounds for {}", case.name));
        if bounds.is_some_and(|(max, max_share)| largest > max || share > max_share) {
            failed.push(case.name.clone());
        }
    }
    assert!(failed.is_empty(), "past their bounds: {failed:?}");
}

#[test]
fn levels_draw_what_level_0_alone_draws() {
    let mut failed = Vec::new();
    for case in frame_cases() {
        let (viewer, frames) = replay(&case);
        // a color table steepens any difference, and maps the frame drawn without it
        // (`the_color_table_maps_the_frame_drawn_without_it`)
        if viewer.gamma() != 0 || viewer.contrast() != 0 {
            continue;
        }
        let level0 = &frames[viewer.frame_index()];
        let alone = draw(&viewer, std::slice::from_ref(level0), WHITE);
        let with_levels = draw(&viewer, &pyramid(level0), WHITE);
        let (largest, share) = errors(&with_levels, &alone);
        println!(
            "{} with levels: largest error {largest}, {share:.3}% off by more than 2",
            case.name
        );
        if largest > 5 || share > 1.37 {
            failed.push(case.name.clone());
        }
    }
    assert!(failed.is_empty(), "past their bounds: {failed:?}");
}

/// Pixels whose colors gamma and contrast change: color hidden under clear pixels, partial
/// alpha, and opaque ones, 30 wide so the checkerboard's white and gray both lie under them.
fn alpha_fixture() -> (u32, u32, Vec<u8>) {
    let pixels: [[u8; 4]; 6] = [
        [250, 10, 120, 0],
        [200, 100, 50, 1],
        [30, 200, 90, 77],
        [123, 45, 210, 128],
        [255, 255, 255, 200],
        [12, 34, 56, 255],
    ];
    let row: Vec<u8> = (0..30).flat_map(|i| pixels[i % 6]).collect();
    (30, 2, row.repeat(2))
}

#[test]
fn gamma_and_contrast_keep_alpha_as_decoded() {
    let (width, height, rgba) = alpha_fixture();
    let image = Image {
        width,
        height,
        levels: &[&rgba],
        opaque: false,
    };
    let mut view = Viewport::default();
    view.set_view_size(Size {
        width: f64::from(width),
        height: f64::from(height),
    });
    view.set_image_size(
        Size {
            width: f64::from(width),
            height: f64::from(height),
        },
        true,
    );
    let premultiplied = |[r, g, b, a]: [u8; 4]| {
        let times = |c: u8| (u32::from(c) * u32::from(a) + 127) / 255;
        u32::from(a) << 24 | times(r) << 16 | times(g) << 8 | times(b)
    };
    let gray = canvas::rgb(0xbf, 0xbf, 0xbf);
    let colors = [
        Colors::new(color_value(8, GAMMA_MAX), 1.0).unwrap(),
        Colors::new(color_value(-8, GAMMA_MAX), 1.0).unwrap(),
        Colors::new(1.0, color_value(8, CONTRAST_MAX)).unwrap(),
        Colors::new(1.0, color_value(-8, CONTRAST_MAX)).unwrap(),
    ];
    for colors in &colors {
        let mut mapped = rgba.clone();
        colors.apply(&mut mapped);
        // over the checkerboard, a solid background and a clear one
        for (checkerboard, background) in [
            (true, WHITE),
            (false, canvas::rgb(0x33, 0x66, 0x99)),
            (false, 0),
        ] {
            view.alpha_layer = checkerboard;
            let mut pixels = vec![background; (width * height) as usize];
            let mut canvas = Canvas::new(&mut pixels, width, height).unwrap();
            image_view::draw(
                &mut canvas,
                &image,
                &view,
                Turn::default(),
                Some(colors),
                1.0,
                false,
            );
            for (i, (&pixel, &decoded)) in pixels.iter().zip(mapped.as_chunks::<4>().0).enumerate()
            {
                let under = if checkerboard && (12..24).contains(&(i % 30)) {
                    gray
                } else {
                    background
                };
                assert_eq!(
                    pixel,
                    canvas::over(premultiplied(decoded), under),
                    "pixel {i}"
                );
            }
        }
    }
}

#[test]
fn a_pan_moves_the_frame_by_whole_pixels() {
    // the zoom stays under a pan: the frame at 150% panned is the same part of the whole
    // image at 150% on a canvas it fills
    let case = frame_cases()
        .into_iter()
        .find(|c| c.name == "photo-150-panned")
        .unwrap();
    let (viewer, frames) = replay(&case);
    let panned = draw(&viewer, &frames, WHITE);
    let (width, height) = (frames[0].0 * 3 / 2, frames[0].1 * 3 / 2);
    let view = viewport((width, height), (frames[0].0, frames[0].1), 1.5);
    let whole = draw_view(
        &view,
        Turn::default(),
        None,
        &frames,
        WHITE,
        (width, height),
    );
    let r = viewer.viewport().image_rect();
    let (left, top) = (
        -r.x.round() as usize,
        (r.max_y() - f64::from(HEIGHT)).round() as usize,
    );
    let part: Vec<u32> = (0..HEIGHT as usize)
        .flat_map(|y| {
            let at = (top + y) * width as usize + left;
            whole[at..at + WIDTH as usize].to_vec()
        })
        .collect();
    let (largest, _) = errors(&panned, &part);
    assert!(largest <= 1, "largest error {largest}");
}

#[test]
fn color_hidden_under_clear_pixels_stays_hidden_when_scaled() {
    // an opaque green square in clear red, drawn over blue: red never shows
    let (side, inner) = (32u32, 8..24u32);
    let pixels: Vec<u8> = (0..side * side)
        .flat_map(|i| {
            let (x, y) = (i % side, i / side);
            if inner.contains(&x) && inner.contains(&y) {
                [0, 160, 0, 255]
            } else {
                [255, 0, 0, 0]
            }
        })
        .collect();
    let level0 = (side, side, pixels);
    for zoom in [0.3, 0.5, 2.5] {
        for levels in [vec![level0.clone()], pyramid(&level0)] {
            let view = viewport((40, 40), (side, side), zoom);
            let frame = draw_view(
                &view,
                Turn::default(),
                None,
                &levels,
                canvas::rgb(0, 0, 255),
                (40, 40),
            );
            assert!(
                frame.iter().all(|&p| rgba(p)[0] == 0),
                "zoom {zoom}, {} levels",
                levels.len()
            );
            assert!(
                frame.iter().any(|&p| rgba(p)[1] > 100),
                "zoom {zoom}: the square shows"
            );
        }
    }
}

#[test]
fn the_color_table_maps_the_frame_drawn_without_it() {
    // for an opaque image, gamma and contrast map the scaled pixels, as Imlib2 maps them
    // while it blends; each frame is one the image covers, as the table would map the
    // background too
    for (fixture, keys) in [
        ("photo.jpg", ""),
        ("photo.jpg", "= + l l d"),
        ("photo.jpg", "> | F"),
        ("gradient.png", "F"),
        ("checker.png", "= + + +"),
    ] {
        let name = format!("{fixture} after {keys:?}");
        let case = Case {
            name: name.clone(),
            fixture: testdata(&format!("fixtures/{fixture}")),
            keys: keys.split_whitespace().map(Keystroke::new).collect(),
        };
        let (viewer, frames) = replay(&case);
        let plain = draw(&viewer, &frames, WHITE);
        for (gamma, contrast) in [(8, 0), (-8, 0), (0, 8), (0, -8)] {
            let colors = Colors::new(
                color_value(gamma, GAMMA_MAX),
                color_value(contrast, CONTRAST_MAX),
            )
            .unwrap();
            let (flip_h, flip_v) = viewer.flips();
            let turn = Turn {
                rotation: viewer.rotation(),
                flip_h,
                flip_v,
            };
            let frame = draw_view(
                viewer.viewport(),
                turn,
                Some(&colors),
                &frames,
                WHITE,
                (WIDTH, HEIGHT),
            );
            let mut expected: Vec<u8> = plain.iter().flat_map(|&p| rgba(p)).collect();
            colors.apply(&mut expected);
            let frame: Vec<u8> = frame.iter().flat_map(|&p| rgba(p)).collect();
            assert!(
                frame == expected,
                "{name} at gamma {gamma}, contrast {contrast}"
            );
        }
    }
}

#[test]
fn gamma_and_contrast_match_imlib2_to_the_level() {
    // eight steps each way, as the golden frames took them, on the photo they drew without
    let plain = golden("photo-down");
    for (name, steps, gamma) in [
        ("photo-gamma-up", 8, true),
        ("photo-gamma-down", -8, true),
        ("photo-contrast-up", 8, false),
        ("photo-contrast-down", -8, false),
    ] {
        let colors = if gamma {
            Colors::new(color_value(steps, GAMMA_MAX), 1.0)
        } else {
            Colors::new(1.0, color_value(steps, CONTRAST_MAX))
        };
        let mut frame = plain.clone();
        colors.unwrap().apply(&mut frame);
        assert!(frame == golden(name), "{name}");
    }
    assert!(Colors::new(1.0, 1.0).is_none());
}
