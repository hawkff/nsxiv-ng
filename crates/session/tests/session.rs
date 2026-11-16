//! The whole stack without a display: files decoded on the Loader's workers, keys through the
//! viewer, and frames the session draws, checked against the golden frames, reference frames
//! kept in the tree.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use nsxiv_ng_core::Size;
use nsxiv_ng_core::config::Config;
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::Options;
use nsxiv_ng_core::viewer::Keystroke;
use nsxiv_ng_decode::{Loader, Pyramid};
use nsxiv_ng_render::grid_view::{self, GridColors};
use nsxiv_ng_render::image_view::{self, Turn};
use nsxiv_ng_render::text::Text;
use nsxiv_ng_render::{Canvas, Colors, bar, canvas};
use nsxiv_ng_session::{Bar, Button, EditSetup, Frame, Palette, Request, Scroll, Session};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 192;
const WHITE: u32 = canvas::rgb(255, 255, 255);

/// Each frame case's largest channel error and share of channels off by more than 2, in
/// percent, as measured against Imlib2's frames; none for a frame not asserted. Rows without
/// a comment have the bounds `BOUNDS` in `crates/render/tests/golden.rs` asserts.
const BOUNDS: &[(&str, Option<(u8, f64)>)] = &[
    // gradient.png is 16-bit: the decoder rounds it to 8 bits where Imlib2 keeps the high
    // byte, so these frames are 1 further off than in render's test, which cuts the fixture
    // as Imlib2 does
    ("gradient-down", Some((2, 0.0))),
    ("gradient-down-noaa", Some((1, 0.0))),
    ("gradient-50", Some((2, 0.0))),
    ("gradient-150", Some((2, 0.0))),
    ("gradient-400", Some((2, 0.0))),
    ("gradient-400-noaa", Some((1, 0.0))),
    ("gradient-fill", Some((2, 0.0))),
    ("checker-down", Some((1, 0.0))),
    ("checker-down-noaa", Some((0, 0.0))),
    ("checker-400", Some((3, 0.03))),
    ("checker-400-noaa", Some((0, 0.0))),
    // photo.jpg decodes differently in zune-jpeg and libjpeg-turbo, up to 9 a channel. These
    // frames are scaled from a pyramid level, which averages 2 x 2 blocks before the area
    // average, where Imlib2 averages the full image once; render's test draws level 0 alone,
    // and its `levels_draw_what_level_0_alone_draws` measures that step by itself. Stacked on
    // the decoder's difference, it takes pixels 2 off to 3.
    ("photo-down", Some((6, 7.06))),
    ("photo-gamma-up", Some((71, 11.56))),
    ("photo-gamma-down", Some((6, 4.88))),
    ("photo-contrast-up", Some((11, 18.14))),
    ("photo-contrast-down", Some((5, 2.61))),
    ("photo-turned-flipped", Some((5, 1.57))),
    // reference only, as in render's test; it enlarges, so it reads level 0 and gives
    // render's numbers exactly
    ("photo-150-panned", None),
    ("alpha-down", Some((0, 0.0))),
    ("alpha-checkerboard", Some((1, 0.0))),
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

// a fresh directory of this test's own
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("nsxiv-ng-session-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A session over `files` in a 256 × 192 window, started, and the wakes it sends.
fn open(
    files: &[PathBuf],
    options: Options,
    config: &Config,
) -> (Session, Receiver<()>, Vec<Request>) {
    open_with(files, options, config, Vec::new(), Bar::Drawn)
}

/// [`open`] with the places to look for the key-handler and where the bar shows.
fn open_with(
    files: &[PathBuf],
    options: Options,
    config: &Config,
    key_handlers: Vec<PathBuf>,
    bar: Bar,
) -> (Session, Receiver<()>, Vec<Request>) {
    let (sender, wakes) = mpsc::channel();
    let files = files.iter().cloned().map(FileEntry::new).collect();
    // no file here needs a decoding helper, so none runs
    let program = PathBuf::from("nsxiv-ng");
    let wake = move || {
        let _ = sender.send(());
    };
    let mut session = Session::new(options, files, config, program, key_handlers, bar, wake);
    let window = Size {
        width: f64::from(WIDTH),
        height: f64::from(HEIGHT),
    };
    let mut requests = session.resize(window, 1.0);
    requests.extend(session.start());
    (session, wakes, requests)
}

/// A config that shows the bar at start, which the default does not.
fn shown_bar() -> Config {
    Config {
        show_bar: true,
        ..Config::default()
    }
}

fn hidden_bar() -> Options {
    Options {
        no_bar: true,
        ..Options::default()
    }
}

fn wait(wakes: &Receiver<()>) {
    wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
}

/// Takes in answers until the file the viewer shows has loaded or failed.
fn settle(session: &mut Session, wakes: &Receiver<()>) -> Vec<Request> {
    let mut requests = Vec::new();
    while session.viewer().image().is_none() && session.viewer().error_text().is_none() {
        wait(wakes);
        requests.extend(session.answers());
    }
    requests
}

/// Takes in an animation's progress until its last frame is in.
fn all_frames(session: &mut Session, wakes: &Receiver<()>) {
    while !session.viewer().image().is_some_and(|image| image.complete) {
        wait(wakes);
        session.answers();
    }
}

fn frame(session: &mut Session) -> Vec<u32> {
    let mut pixels = vec![0; (WIDTH * HEIGHT) as usize];
    let mut canvas = Canvas::new(&mut pixels, WIDTH, HEIGHT).unwrap();
    session.draw(&mut canvas, 1.0, Frame::default());
    pixels
}

fn rgba(pixel: u32) -> [u8; 4] {
    let [a, r, g, b] = pixel.to_be_bytes();
    [r, g, b, a]
}

/// The largest channel error and the percentage of channels off by more than 2, of the
/// color channels, as render's golden test counts them.
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
    let path = testdata(&format!("golden/{name}.png"));
    let golden = image::open(&path).unwrap().to_rgba8().into_raw();
    golden
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&[r, g, b, _]| canvas::rgb(r, g, b))
        .collect()
}

/// The `frame` rows of cases.tsv: name, fixture and keys, `C-n` being Ctrl and n.
fn frame_cases() -> Vec<(String, PathBuf, Vec<Keystroke>)> {
    let text = std::fs::read_to_string(testdata("golden/cases.tsv")).unwrap();
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t').collect::<Vec<_>>())
        .filter(|fields| fields[1] == "frame")
        .map(|fields| {
            let keys = fields.get(3).map_or(Vec::new(), |keys| {
                keys.split(' ')
                    .map(|key| match key.strip_prefix("C-") {
                        Some(key) if !key.is_empty() => Keystroke::ctrl(key),
                        _ => Keystroke::new(key),
                    })
                    .collect()
            });
            let fixture = testdata(&format!("fixtures/{}", fields[2]));
            (fields[0].to_owned(), fixture, keys)
        })
        .collect()
}

#[test]
fn frames_match_imlib2_through_the_whole_stack() {
    let cases = frame_cases();
    assert_eq!(cases.len(), BOUNDS.len(), "every frame case has bounds");
    let mut failed = Vec::new();
    for (name, fixture, keys) in &cases {
        let (mut session, wakes, _) = open(
            std::slice::from_ref(fixture),
            hidden_bar(),
            &Config::default(),
        );
        settle(&mut session, &wakes);
        if name.starts_with("moving-") {
            all_frames(&mut session, &wakes);
        }
        for key in keys {
            session
                .key(key)
                .unwrap_or_else(|| panic!("{name}: no binding for {key:?}"));
            settle(&mut session, &wakes);
        }
        assert!(
            session.viewer().image().is_some(),
            "{name}: the fixture loads"
        );
        let (largest, share) = errors(&frame(&mut session), &golden_frame(name));
        println!("{name}: largest error {largest}, {share:.3}% off by more than 2");
        let &(_, bounds) = BOUNDS
            .iter()
            .find(|(case, _)| case == name)
            .unwrap_or_else(|| panic!("no bounds for {name}"));
        if bounds.is_some_and(|(max, max_share)| largest > max || share > max_share) {
            failed.push(name.clone());
        }
    }
    assert!(failed.is_empty(), "past their bounds: {failed:?}");
}

/// The file's pyramid, as a Loader of its own decodes it.
fn pyramid(path: &Path) -> Arc<Pyramid> {
    let (sender, answers) = mpsc::channel();
    let loader = Loader::new(2, 1 << 30, 0, move |loaded| {
        let _ = sender.send(loaded);
    });
    loader.load(1, path.to_owned());
    let loaded = answers.recv_timeout(Duration::from_secs(60)).unwrap();
    loaded.pyramid.unwrap()
}

#[test]
fn the_frame_is_the_image_view_of_the_pyramid_over_the_bar() {
    let (ready, fonts) = mpsc::channel();
    let mut text = Text::new("monospace", 13.0, move || {
        let _ = ready.send(());
    });
    fonts.recv_timeout(Duration::from_secs(60)).unwrap();
    let bar = bar::height(&mut text).expect("a font") as u32;
    let main = HEIGHT - bar;
    for (fixture, keys) in [
        ("photo.jpg", ""),
        ("gradient.png", "="),
        ("checker.png", "= + + +"),
        ("checker.png", "a"),
        ("photo.jpg", "> |"),
        ("photo.jpg", "_ < } } }"),
        ("photo.jpg", "( ( ( ( F"),
        ("alpha.png", "= - -"),
        ("alpha.png", "A"),
        ("alpha.png", "A } ) +"),
    ] {
        let path = testdata(&format!("fixtures/{fixture}"));
        let (mut session, wakes, _) = open(
            std::slice::from_ref(&path),
            Options::default(),
            &shown_bar(),
        );
        // the image in, and the canvas above the bar once the fonts tell its height
        while session.viewer().image().is_none()
            || session.viewer().viewport().view_size().height != f64::from(main)
        {
            wait(&wakes);
            session.answers();
        }
        for key in keys.split_whitespace() {
            session.key(&Keystroke::new(key)).unwrap();
        }
        let frame = frame(&mut session);

        let viewer = session.viewer();
        let pyramid = pyramid(&path);
        let levels: Vec<&[u8]> = pyramid.levels.iter().map(|l| &l.rgba[..]).collect();
        let image = image_view::Image {
            width: pyramid.levels[0].width,
            height: pyramid.levels[0].height,
            levels: &levels,
            opaque: pyramid.opaque,
        };
        let (flip_h, flip_v) = viewer.flips();
        let turn = Turn {
            rotation: viewer.rotation(),
            flip_h,
            flip_v,
        };
        let colors = Colors::new(viewer.gamma_value(), viewer.contrast_value());
        let mut expected = vec![WHITE; (WIDTH * HEIGHT) as usize];
        let area = &mut expected[..(WIDTH * main) as usize];
        let mut area = Canvas::new(area, WIDTH, main).unwrap();
        let view = viewer.viewport();
        image_view::draw(&mut area, &image, view, turn, colors.as_ref(), 1.0, false);
        assert!(
            area.pixels().iter().any(|&p| p != WHITE),
            "{fixture} after {keys:?} shows"
        );
        let mut canvas = Canvas::new(&mut expected, WIDTH, HEIGHT).unwrap();
        let (left, right) = viewer.status_text();
        bar::status(
            &mut canvas,
            &mut text,
            [0, main as i32, WIDTH as i32, bar as i32],
            1.0,
            (&left, &right),
            (canvas::rgb(0xee, 0xee, 0xee), canvas::rgb(0, 0, 0)),
        );
        assert!(frame == expected, "{fixture} after {keys:?}");
    }
}

#[test]
fn an_answer_after_the_viewer_moved_on_changes_nothing() {
    let dir = scratch("moved-on");
    // a FIFO: its decode waits in open until the test opens the other end
    let slow = dir.join("slow.png");
    let made = std::process::Command::new("mkfifo")
        .arg(&slow)
        .status()
        .unwrap();
    assert!(made.success());
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg"), slow.clone()],
        hidden_bar(),
        &Config::default(),
    );
    // the fonts' wake and the photo's, neither answer taken in yet
    wait(&wakes);
    wait(&wakes);
    session.key(&Keystroke::new("j")).unwrap();
    session.answers();
    let viewer = session.viewer();
    assert!(
        viewer.current() == 1 && viewer.image().is_none() && viewer.error_text().is_none(),
        "the photo's answer came after the viewer moved on"
    );
    assert!(
        frame(&mut session).iter().all(|&p| p == WHITE),
        "and nothing of it shows"
    );
    // the slow file opens now, and a FIFO is no image file
    drop(std::fs::OpenOptions::new().write(true).open(&slow).unwrap());
    let requests = settle(&mut session, &wakes);
    assert_eq!(
        session.viewer().error_text(),
        Some("could not load: slow.png")
    );
    let prefix = format!("{}: ", slow.display());
    assert!(
        requests
            .iter()
            .any(|r| matches!(r, Request::Warning(w) if w.starts_with(&prefix))),
        "the decoder's reason goes to the frontend: {requests:?}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_image_on_screen_keeps_its_own_turn_while_the_next_file_decodes() {
    let dir = scratch("turn");
    let slow = dir.join("slow.png");
    let made = std::process::Command::new("mkfifo")
        .arg(&slow)
        .status()
        .unwrap();
    assert!(made.success());
    let gradient = testdata("fixtures/gradient.png");
    let files = [
        testdata("fixtures/photo.jpg"),
        gradient.clone(),
        slow.clone(),
    ];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    session.key(&Keystroke::new(">")).unwrap();
    frame(&mut session);
    // the gradient taken in and never drawn, then the slow file, which waits in open
    session.key(&Keystroke::new("j")).unwrap();
    settle(&mut session, &wakes);
    session.key(&Keystroke::new("j")).unwrap();
    session.answers();
    let viewer = session.viewer();
    assert!(viewer.current() == 2 && viewer.image().is_none());
    let drawn = frame(&mut session);

    let viewer = session.viewer();
    let pyramid = pyramid(&gradient);
    let levels: Vec<&[u8]> = pyramid.levels.iter().map(|l| &l.rgba[..]).collect();
    let image = image_view::Image {
        width: pyramid.levels[0].width,
        height: pyramid.levels[0].height,
        levels: &levels,
        opaque: pyramid.opaque,
    };
    let mut expected = vec![WHITE; (WIDTH * HEIGHT) as usize];
    let mut canvas = Canvas::new(&mut expected, WIDTH, HEIGHT).unwrap();
    let view = viewer.viewport();
    image_view::draw(&mut canvas, &image, view, Turn::default(), None, 1.0, false);
    assert!(
        drawn == expected,
        "the gradient unturned, not with the photo's turn"
    );
    drop(std::fs::OpenOptions::new().write(true).open(&slow).unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

// A PNG's chunk: length, type, data and CRC-32.
fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut crc = !0u32;
    for &byte in kind.iter().chain(data) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    chunk
}

#[test]
fn the_error_text_says_why_a_file_did_not_load() {
    let dir = scratch("errors");
    // a header of 16 x 2000000000 RGB, some 170 GB decoded with its levels, in rows short
    // enough for the decoder's own limit, and no pixels
    let big = dir.join("big.png");
    let mut header = [16u32.to_be_bytes(), 2_000_000_000u32.to_be_bytes()].concat();
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let png = [
        b"\x89PNG\r\n\x1a\n".to_vec(),
        chunk(b"IHDR", &header),
        chunk(b"IDAT", &[]),
        chunk(b"IEND", &[]),
    ]
    .concat();
    std::fs::write(&big, png).unwrap();
    let bad = dir.join("bad.png");
    std::fs::write(&bad, b"not an image").unwrap();
    let warnings = |requests: &[Request]| {
        requests
            .iter()
            .filter_map(|r| match r {
                Request::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    // a cap of a hundred-thousandth of memory: far below the image on any machine, and above
    // both files' bytes and the decoder's row buffers
    let config = Config {
        max_image_percent: 1e-3,
        ..Config::default()
    };
    let (mut session, wakes, _) = open(&[big, bad.clone()], hidden_bar(), &config);
    let requests = settle(&mut session, &wakes);
    assert_eq!(
        session.viewer().error_text(),
        Some("too large: 16x2000000000")
    );
    assert_eq!(
        warnings(&requests),
        Vec::<String>::new(),
        "the screen says why"
    );
    session.key(&Keystroke::new("j")).unwrap();
    let requests = settle(&mut session, &wakes);
    assert_eq!(
        session.viewer().error_text(),
        Some("could not load: bad.png")
    );
    let prefix = format!("{}: ", bad.display());
    assert!(
        matches!(&warnings(&requests)[..], [w] if w.starts_with(&prefix)),
        "{requests:?}"
    );
    // the text shows once the fonts are in
    while frame(&mut session).iter().all(|&p| p == WHITE) {
        wait(&wakes);
        session.answers();
    }

    let quiet = Options {
        quiet: true,
        ..hidden_bar()
    };
    let (mut session, wakes, _) = open(std::slice::from_ref(&bad), quiet, &config);
    let requests = settle(&mut session, &wakes);
    assert_eq!(warnings(&requests), Vec::<String>::new(), "-q keeps quiet");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_bar_takes_the_bottom_rows_once_the_fonts_are_in() {
    let (mut session, wakes, mut requests) = open(
        &[testdata("fixtures/photo.jpg")],
        Options::default(),
        &shown_bar(),
    );
    let photo = Size {
        width: 640.0,
        height: 480.0,
    };
    // the window fits the photo with no bar until the fonts tell its height
    let bar = loop {
        let fitted = requests.iter().find_map(|r| match r {
            Request::FitWindow { image, bar } if *bar > 0.0 => Some((*image, *bar)),
            _ => None,
        });
        if let Some((image, bar)) = fitted {
            assert_eq!(image, photo);
            break bar;
        }
        wait(&wakes);
        requests = session.answers();
    };
    settle(&mut session, &wakes);
    assert_eq!(
        session.viewer().viewport().view_size().height,
        f64::from(HEIGHT) - bar,
        "the image gets the rows above the bar"
    );
    let pixels = frame(&mut session);
    let rows: Vec<&[u32]> = pixels.chunks(WIDTH as usize).collect();
    let gray = canvas::rgb(0xee, 0xee, 0xee);
    let top = HEIGHT as usize - bar as usize;
    assert!(rows[top..].iter().all(|row| row[0] == gray));
    assert!(
        rows[top..]
            .iter()
            .flat_map(|row| row.iter())
            .any(|&p| p != gray),
        "with the status in it"
    );
    assert!(
        rows[..top].iter().all(|row| row[0] == WHITE),
        "and above it the background beside the photo, fitted to the rows left"
    );
}

fn grid_options() -> Options {
    Options {
        thumbnail_mode: true,
        // nothing is written to the user's thumbnail cache
        private_mode: true,
        ..hidden_bar()
    }
}

/// Takes in answers until the grid's frame changes from `before`, and gives the new frame.
fn changed(session: &mut Session, wakes: &Receiver<()>, before: &[u32]) -> Vec<u32> {
    loop {
        wait(wakes);
        session.answers();
        let now = frame(session);
        if now != before {
            return now;
        }
    }
}

/// Each grid case's largest channel error and share of channels off by more than 2, in
/// percent: the grid drawn with the session's thumbnail against the same grid drawn with the
/// reference thumbnail. Both are scaled into the 128-pixel cell. Ours covers a side of 160,
/// from the cache's 256-pixel folder, where the reference took 320, and for the photo
/// zune-jpeg's difference from libjpeg-turbo comes along.
const GRID_BOUNDS: &[(&str, (u8, f64))] = &[
    ("photo-thumbnail", (5, 0.97)),
    ("moving-thumbnail", (0, 0.0)),
];

#[test]
fn the_grid_matches_when_using_reference_thumbnails() {
    let text = std::fs::read_to_string(testdata("golden/cases.tsv")).unwrap();
    let cases: Vec<(String, String)> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t').collect::<Vec<_>>())
        .filter(|fields| fields[1] == "thumbnail")
        .map(|fields| (fields[0].to_owned(), fields[2].to_owned()))
        .collect();
    assert_eq!(cases.len(), GRID_BOUNDS.len(), "every grid case has bounds");
    let mut failed = Vec::new();
    for (name, fixture) in cases {
        let path = testdata(&format!("fixtures/{fixture}"));
        let (mut session, wakes, _) = open(&[path], grid_options(), &Config::default());
        let placeholder = frame(&mut session);
        let drawn = changed(&mut session, &wakes, &placeholder);

        let golden = image::open(testdata(&format!("golden/{name}.png")))
            .unwrap()
            .to_rgba8();
        let levels = [golden.as_raw().as_slice()];
        let thumbnail = image_view::Image {
            width: golden.width(),
            height: golden.height(),
            levels: &levels,
            opaque: false,
        };
        let mut expected = vec![WHITE; (WIDTH * HEIGHT) as usize];
        let mut canvas = Canvas::new(&mut expected, WIDTH, HEIGHT).unwrap();
        let colors = GridColors {
            placeholder: canvas::rgb(0xee, 0xee, 0xee),
            selection: canvas::rgb(0x34, 0x78, 0xf6),
            mark: canvas::rgb(0xff, 0xcc, 0x00),
        };
        grid_view::draw(&mut canvas, session.viewer(), 1.0, colors, |_| {
            Some(thumbnail)
        });
        let (largest, share) = errors(&drawn, &expected);
        println!("{name}: largest error {largest}, {share:.3}% off by more than 2");
        let &(_, (max, max_share)) = GRID_BOUNDS.iter().find(|(case, _)| *case == name).unwrap();
        if largest > max || share > max_share {
            failed.push(name);
        }
    }
    assert!(failed.is_empty(), "past their bounds: {failed:?}");
}

fn solid_png(path: &Path, rgb: [u8; 3]) {
    image::RgbImage::from_pixel(64, 64, image::Rgb(rgb))
        .save(path)
        .unwrap();
}

#[test]
fn an_invalidated_file_gets_a_new_thumbnail() {
    let dir = scratch("invalidated");
    let path = dir.join("solid.png");
    solid_png(&path, [255, 0, 0]);
    let (mut session, wakes, _) = open(
        std::slice::from_ref(&path),
        grid_options(),
        &Config::default(),
    );
    let placeholder = frame(&mut session);
    let red = changed(&mut session, &wakes, &placeholder);
    assert!(red.contains(&canvas::rgb(255, 0, 0)));
    solid_png(&path, [0, 0, 255]);
    let requests = session.key(&Keystroke::new("R")).unwrap();
    assert_eq!(requests.last(), Some(&Request::Redraw { bar_only: false }));
    assert_eq!(
        frame(&mut session),
        placeholder,
        "the old thumbnail goes at once"
    );
    let blue = changed(&mut session, &wakes, &placeholder);
    assert!(blue.contains(&canvas::rgb(0, 0, 255)) && !blue.contains(&canvas::rgb(255, 0, 0)));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Takes in answers until the grid shows no placeholder, which is `placeholder`, the bar's
/// background, a color none of the thumbnails has: every thumbnail in view is in. False where
/// a minute passed first. Each look draws a clear frame, whose cells are kept under its
/// background alone, so no frame drawn over the session's background copies them.
fn thumbnails_in(session: &mut Session, wakes: &Receiver<()>, placeholder: [u8; 3]) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let placeholder = canvas::rgb(placeholder[0], placeholder[1], placeholder[2]);
    let clear = Frame {
        clear: true,
        ..Frame::default()
    };
    loop {
        while wakes.try_recv().is_ok() {
            session.answers();
        }
        let mut pixels = vec![0; (WIDTH * HEIGHT) as usize];
        let mut canvas = Canvas::new(&mut pixels, WIDTH, HEIGHT).unwrap();
        session.draw(&mut canvas, 1.0, clear);
        if !pixels.contains(&placeholder) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        if wakes.recv_timeout(Duration::from_millis(100)).is_ok() {
            session.answers();
        }
    }
}

/// The colors of the grid's kept-cells test, the bar's background `placeholder`.
fn palette(background: [u8; 3], placeholder: [u8; 3]) -> Palette {
    Palette {
        background,
        foreground: [240, 240, 240],
        bar_background: placeholder,
        selection: [200, 50, 50],
        mark: [50, 200, 50],
        placeholder,
        error: [240, 240, 240],
    }
}

/// The placeholders of the kept-cells test before its palette step and from it on.
const PLACEHOLDERS: [[u8; 3]; 2] = [[1, 254, 3], [3, 250, 7]];

/// What the grid's kept-cells test does at a step: to the session, and to the files.
type Step = (&'static str, fn(&mut Session, &Path));

#[test]
fn a_grid_drawn_from_its_kept_cells_is_the_grid_drawn_afresh() {
    // a thumbnail with alpha, which the background under it shows through, among others
    let dir = scratch("kept-cells");
    let names = [
        "alpha.png",
        "photo.jpg",
        "checker.png",
        "gradient.png",
        "jpeg-420.jpg",
    ];
    let files: Vec<PathBuf> = names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            std::fs::copy(testdata(&format!("fixtures/{name}")), &path).unwrap();
            path
        })
        .collect();
    let steps: [Step; 6] = [
        ("drawn again", |_, _| {}),
        ("scrolled by half a cell", |session, _| {
            session.scroll(Scroll::Distance { dx: 0.0, dy: 74.0 });
        }),
        ("scrolled by a row", |session, _| {
            session.scroll(Scroll::Distance { dx: 0.0, dy: 148.0 });
        }),
        ("the grid zoomed out", |session, _| {
            session.key(&Keystroke::new("-"));
        }),
        ("another palette", |session, _| {
            session.set_palette(palette([10, 20, 30], PLACEHOLDERS[1]));
        }),
        ("a thumbnail made anew", |session, dir| {
            // the same size, so only the thumbnail tells the cell kept from the new one
            let first = dir.join("alpha.png");
            let mut image = image::open(testdata("fixtures/alpha.png"))
                .unwrap()
                .to_rgba8();
            image
                .pixels_mut()
                .for_each(|p| p.0[..3].iter_mut().for_each(|c| *c = 255 - *c));
            image.save(&first).unwrap();
            session.key(&Keystroke::new("R"));
        }),
    ];
    // the steps from the palette's on show the second placeholder
    let placeholder = |at: usize| PLACEHOLDERS[usize::from(at >= 4)];
    let open = || {
        let (mut session, wakes, _) = open(&files, grid_options(), &Config::default());
        session.set_palette(palette([255, 255, 255], PLACEHOLDERS[0]));
        (session, wakes)
    };
    let (mut kept, wakes) = open();
    assert!(
        thumbnails_in(&mut kept, &wakes, PLACEHOLDERS[0]),
        "thumbnails in"
    );
    frame(&mut kept);
    for (at, (what, step)) in steps.iter().enumerate() {
        step(&mut kept, &dir);
        let loaded = thumbnails_in(&mut kept, &wakes, placeholder(at));
        assert!(loaded, "thumbnails in, {what}");
        // the first frame keeps what it draws anew, the second copies all it kept
        let frames = [frame(&mut kept), frame(&mut kept)];
        // the same steps in a session that never drew a frame over its background, so draws
        // every thumbnail
        let (mut fresh, fresh_wakes) = open();
        assert!(thumbnails_in(&mut fresh, &fresh_wakes, PLACEHOLDERS[0]));
        for (step_at, (_, step)) in steps[..=at].iter().enumerate() {
            step(&mut fresh, &dir);
            let loaded = thumbnails_in(&mut fresh, &fresh_wakes, placeholder(step_at));
            assert!(loaded, "thumbnails in afresh, {what}");
        }
        let afresh = frame(&mut fresh);
        assert!(frames.iter().all(|f| *f == afresh), "{what}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_fonts_coming_in_leave_a_grid_without_its_bar_where_it_was_scrolled() {
    // five cells in a column, scrolled a row down before the session took the fonts in, which
    // it does among its answers
    let names = [
        "alpha.png",
        "photo.jpg",
        "checker.png",
        "gradient.png",
        "jpeg-420.jpg",
    ];
    let files = names.map(|name| testdata(&format!("fixtures/{name}")));
    let (mut session, wakes, _) = open(&files, grid_options(), &Config::default());
    session.scroll(Scroll::Distance { dx: 0.0, dy: 148.0 });
    let scrolled = session.viewer().grid().scroll_y();
    assert!(scrolled > 0.0);
    while session.title_height() == 0.0 {
        wait(&wakes);
        session.answers();
    }
    assert_eq!(session.viewer().grid().scroll_y(), scrolled);
}

/// A frame drawn over the session's last one, which the canvas holds.
const KEPT: Frame = Frame {
    clear: false,
    whole: false,
    kept: true,
};

/// A canvas pixel no frame of an opaque image gives, as its alpha is not 255.
const SENTINEL: u32 = 0x0102_0304;

/// The photo at `scale`, in a session over the whole window, at 100% and then zoomed by
/// `factor` about the window's middle, the image's pixels in.
fn photo_at(scale: f64, factor: f64) -> (Session, u32, u32) {
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg")],
        Options::default(),
        &Config::default(),
    );
    settle(&mut session, &wakes);
    let window = Size {
        width: f64::from(WIDTH),
        height: f64::from(HEIGHT),
    };
    session.resize(window, scale);
    session.key(&Keystroke::new("=")).unwrap();
    session.zoom_at(factor, at(128.0, 96.0));
    let (w, h) = (
        (f64::from(WIDTH) * scale) as u32,
        (f64::from(HEIGHT) * scale) as u32,
    );
    (session, w, h)
}

fn draw_frame(
    session: &mut Session,
    pixels: &mut [u32],
    (w, h): (u32, u32),
    frame: Frame,
    scale: f64,
) {
    let mut canvas = Canvas::new(pixels, w, h).unwrap();
    session.draw(&mut canvas, scale, frame);
}

/// Whether a frame drawn over a copy of `last` with the sentinel over its middle moved it: some
/// sentinel pixel is left, where a frame drawn whole leaves none.
fn moved(session: &mut Session, last: &[u32], size: (u32, u32), scale: f64) -> bool {
    let mut marked = last.to_vec();
    let (w, h) = (size.0 as usize, size.1 as usize);
    for row in h / 3..h / 2 {
        marked[row * w + w / 3..row * w + w / 2].fill(SENTINEL);
    }
    draw_frame(session, &mut marked, size, KEPT, scale);
    marked.contains(&SENTINEL)
}

/// The pans of the moved-frame tests, in logical units, either way, both ways and far.
const PANS: [(f64, f64); 7] = [
    (10.0, 0.0),
    (-10.0, 0.0),
    (0.0, 7.0),
    (0.0, -7.0),
    (6.0, -5.0),
    (-3.0, 9.0),
    (-37.0, 21.0),
];

#[test]
fn a_pan_drawn_over_the_last_frame_is_the_frame_drawn_whole() {
    // whole zooms, uneven ones, the 5.5 whose precision a sample at the edge changes, and
    // fifty a pinch might give, at both scales
    let mut zooms = vec![2.0, 3.0, 4.0, 1.5, 2.37, 1.0, 5.5];
    zooms.extend((0..50).map(|i| 1.01 + f64::from(i) * (4.0 - 1.01) / 49.0));
    let (mut pans, mut moves) = (0, 0);
    for scale in [1.0, 2.0] {
        for &zoom in &zooms {
            // one session draws over its frames, another, given the same steps, tells
            // whether they move
            let (mut session, w, h) = photo_at(scale, zoom);
            let (mut told, _, _) = photo_at(scale, zoom);
            let size = (w, h);
            let mut kept = vec![0; (w * h) as usize];
            draw_frame(&mut session, &mut kept, size, Frame::default(), scale);
            let mut last = kept.clone();
            draw_frame(&mut told, &mut last, size, Frame::default(), scale);
            // then to a corner, where the image's edge meets the window's, and a pixel each way
            // from it, where a sample on a pixel leaves view or comes in
            let pixel = 1.0 / scale;
            let edge = [
                (-1e5, 1e5),
                (pixel, 0.0),
                (-pixel, 0.0),
                (0.0, -pixel),
                (0.0, pixel),
                (pixel, -pixel),
                (-pixel, pixel),
            ];
            for (dx, dy) in PANS.into_iter().chain(edge) {
                session.scroll(Scroll::Distance { dx, dy });
                told.scroll(Scroll::Distance { dx, dy });
                draw_frame(&mut session, &mut kept, size, KEPT, scale);
                let mut whole = vec![0; (w * h) as usize];
                draw_frame(&mut session, &mut whole, size, Frame::default(), scale);
                assert!(
                    kept == whole,
                    "scale {scale}, zoom {zoom}, a pan of ({dx}, {dy})"
                );
                pans += 1;
                if moved(&mut told, &last, size, scale) {
                    moves += 1;
                }
                last = whole;
            }
        }
    }
    println!("{moves} of {pans} pans moved the frame before");
    // the copies at 100% and the whole zooms at least
    assert!(moves >= 2 * 4 * PANS.len(), "{moves} of {pans} pans moved");
}

#[test]
fn a_frame_drawn_over_the_last_moves_it_only_where_the_pan_alone_changed() {
    let (mut session, w, h) = photo_at(1.0, 2.0);
    let size = (w, h);
    let mut last = vec![0; (w * h) as usize];
    draw_frame(&mut session, &mut last, size, Frame::default(), 1.0);
    // a pan moves the frame before, as the session tells beforehand
    session.scroll(Scroll::Distance { dx: 10.0, dy: 7.0 });
    assert!(session.would_move(size, 1.0), "a pan, told");
    assert!(moved(&mut session, &last, size, 1.0), "a pan");
    draw_frame(&mut session, &mut last, size, Frame::default(), 1.0);
    // a frame drawn without the promise is drawn whole
    session.scroll(Scroll::Distance { dx: 10.0, dy: 7.0 });
    let mut marked = last.clone();
    marked[w as usize * 50 + 50] = SENTINEL;
    draw_frame(&mut session, &mut marked, size, Frame::default(), 1.0);
    assert!(!marked.contains(&SENTINEL), "no promise");
    draw_frame(&mut session, &mut last, size, Frame::default(), 1.0);
    // another zoom, a little one so the frame before would move but little, and a reduced
    // image, whose taps a crop moves, are drawn whole
    session.zoom_at(1.02, at(128.0, 96.0));
    assert!(!session.would_move(size, 1.0), "another zoom, told");
    assert!(!moved(&mut session, &last, size, 1.0), "another zoom");
    draw_frame(&mut session, &mut last, size, Frame::default(), 1.0);
    let (mut reduced, _, _) = photo_at(1.0, 0.75);
    draw_frame(&mut reduced, &mut last, size, Frame::default(), 1.0);
    reduced.scroll(Scroll::Distance { dx: 10.0, dy: 7.0 });
    assert!(!moved(&mut reduced, &last, size, 1.0), "a reduced image");
    // an image with alpha is drawn whole
    let (mut alpha, wakes, _) = open(
        &[testdata("fixtures/alpha.png")],
        Options::default(),
        &Config::default(),
    );
    settle(&mut alpha, &wakes);
    alpha.key(&Keystroke::new("=")).unwrap();
    alpha.zoom_at(8.0, at(128.0, 96.0));
    draw_frame(&mut alpha, &mut last, size, Frame::default(), 1.0);
    alpha.scroll(Scroll::Distance { dx: 10.0, dy: 7.0 });
    assert!(!moved(&mut alpha, &last, size, 1.0), "an image with alpha");
}

const MS: Duration = Duration::from_millis(1);

#[test]
fn the_slideshow_advances_once_its_deadline_has_passed_and_not_before() {
    let options = Options {
        slideshow: Some(2.0),
        ..hidden_bar()
    };
    let files = [
        testdata("fixtures/photo.jpg"),
        testdata("fixtures/checker.png"),
    ];
    let (mut session, wakes, _) = open(&files, options, &Config::default());
    settle(&mut session, &wakes);
    let due = session.deadline().expect("the slideshow's countdown");
    assert_eq!(session.timers(due - MS), []);
    assert_eq!(session.viewer().current(), 0);
    session.timers(due);
    assert_eq!(session.viewer().current(), 1, "the next image");
    settle(&mut session, &wakes);
    let due = session
        .deadline()
        .expect("a countdown for the image that showed");
    session.key(&Keystroke::new("s")).unwrap();
    assert_eq!(session.deadline(), None, "s stops the slideshow");
    session.timers(due + Duration::from_secs(60));
    assert_eq!(
        session.viewer().current(),
        1,
        "a canceled timer never fires"
    );
}

#[test]
fn a_reload_after_a_change_waits_out_its_quiet_time() {
    let dir = scratch("reload");
    let path = dir.join("solid.png");
    solid_png(&path, [255, 0, 0]);
    let (mut session, wakes, requests) = open(
        std::slice::from_ref(&path),
        hidden_bar(),
        &Config::default(),
    );
    assert!(requests.contains(&Request::Watch(Some(path.clone()))));
    settle(&mut session, &wakes);
    assert_eq!(session.file_changed(), []);
    let due = session.deadline().expect("the quiet time");
    session.timers(due - MS);
    assert_eq!(
        session.viewer().awaited(),
        None,
        "no reload within the quiet time"
    );
    let requests = session.timers(due);
    assert!(
        session.viewer().awaited().is_some(),
        "a reload once it has passed"
    );
    assert!(
        requests.contains(&Request::Watch(Some(path))),
        "the new file is watched"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_animation_plays_through_its_frames_by_deadlines() {
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/moving.gif")],
        hidden_bar(),
        &Config::default(),
    );
    settle(&mut session, &wakes);
    all_frames(&mut session, &wakes);
    for (index, name) in [(1, "moving-2"), (2, "moving-3"), (0, "moving-1")] {
        let due = session.deadline().expect("the next frame's time");
        session.timers(due - MS);
        assert_ne!(session.viewer().frame_index(), index, "not before its time");
        session.timers(due);
        assert_eq!(session.viewer().frame_index(), index);
        assert_eq!(
            errors(&frame(&mut session), &golden_frame(name)),
            (0, 0.0),
            "{name}"
        );
    }
}

#[test]
fn a_file_whose_name_has_a_newline_is_not_passed_to_the_key_handler() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("key-handler-newline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (kept, odd) = (dir.join("a.png"), dir.join("keep.png\nvictim.png"));
    for file in [&kept, &odd] {
        solid_png(file, [0, 128, 0]);
    }
    let out = dir.join("out");
    let script = dir.join("key-handler");
    let text = format!("#!/bin/sh\ncat > '{}'\n", out.display());
    std::fs::write(&script, text).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let warning = Request::Warning(format!(
        "{}: not passed to the key-handler, as its name has a newline",
        odd.display()
    ));
    let run = |files: &[PathBuf], quiet: bool| {
        let options = Options {
            quiet,
            ..hidden_bar()
        };
        let handlers = vec![script.clone()];
        let (mut session, wakes, _) =
            open_with(files, options, &Config::default(), handlers, Bar::Drawn);
        settle(&mut session, &wakes);
        // every file marked
        session.key(&Keystroke::ctrl("m")).unwrap();
        session.key(&Keystroke::ctrl("x")).unwrap();
        session.key(&Keystroke::new("t")).unwrap()
    };
    // among others it is left out, with a warning, and the others go
    let requests = run(&[kept.clone(), odd.clone()], false);
    assert_eq!(requests, [warning.clone(), Request::RunKeyHandler]);
    let quiet = run(&[kept.clone(), odd.clone()], true);
    assert_eq!(quiet, [Request::RunKeyHandler], "no warning under -q");
    // alone, the handler does not run, and the screen stays the viewer's
    assert_eq!(run(std::slice::from_ref(&odd), false), [warning]);
    assert!(!out.exists(), "no handler ran yet");
    let (mut session, wakes, _) = open_with(
        &[kept.clone(), odd.clone()],
        hidden_bar(),
        &Config::default(),
        vec![script.clone()],
        Bar::Drawn,
    );
    settle(&mut session, &wakes);
    session.key(&Keystroke::ctrl("m")).unwrap();
    session.key(&Keystroke::ctrl("x")).unwrap();
    session.key(&Keystroke::new("t")).unwrap();
    session.run_key_handler();
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!("{}\n", kept.display()),
        "the handler reads the other file alone"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_key_handler_gets_the_key_and_the_marked_files() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("key-handler-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let files: Vec<PathBuf> = ["a.png", "b.png", "c.png"]
        .map(|name| dir.join(name))
        .to_vec();
    for file in &files {
        solid_png(file, [0, 128, 0]);
    }
    let out = dir.join("out");
    // it writes its key and its stdin down, and deletes c.png
    let script = dir.join("key-handler");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" > '{out}'\ncat >> '{out}'\nrm '{c}'\n",
            out = out.display(),
            c = files[2].display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // the first place holds none, so the second serves
    let handlers = vec![dir.join("missing/key-handler"), script];
    let (mut session, wakes, _) = open_with(
        &files,
        hidden_bar(),
        &Config::default(),
        handlers,
        Bar::Drawn,
    );
    settle(&mut session, &wakes);
    for key in ["m", "j", "m"] {
        session.key(&Keystroke::new(key)).unwrap();
    }
    session.key(&Keystroke::ctrl("x")).unwrap();
    let requests = session.key(&Keystroke::new("y")).unwrap();
    assert_eq!(requests, [Request::RunKeyHandler]);
    session.run_key_handler();
    let fed = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        fed,
        format!("y\n{}\n{}\n", files[0].display(), files[2].display()),
        "the key, then the marked files"
    );
    let left: Vec<&PathBuf> = session.viewer().files().iter().map(|f| &f.path).collect();
    assert_eq!(
        left,
        [&files[0], &files[1]],
        "the deleted file left the list"
    );

    let none = vec![dir.join("none")];
    let (mut session, wakes, _) = open_with(
        &files[..1],
        hidden_bar(),
        &Config::default(),
        none,
        Bar::Drawn,
    );
    settle(&mut session, &wakes);
    session.key(&Keystroke::ctrl("x")).unwrap();
    let requests = session.key(&Keystroke::new("y")).unwrap();
    let warning = format!(
        "no executable key-handler at {}",
        dir.join("none").display()
    );
    assert_eq!(
        requests,
        [Request::Warning(warning)],
        "a warning, and no run"
    );

    // one whose interpreter is missing cannot be started
    let broken = dir.join("broken");
    std::fs::write(&broken, "#!/nonexistent/sh\n").unwrap();
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    for quiet in [false, true] {
        let options = Options {
            quiet,
            ..hidden_bar()
        };
        let handlers = vec![broken.clone()];
        let (mut session, wakes, _) = open_with(
            &files[..1],
            options.clone(),
            &Config::default(),
            handlers,
            Bar::Drawn,
        );
        settle(&mut session, &wakes);
        session.key(&Keystroke::ctrl("x")).unwrap();
        let requests = session.key(&Keystroke::new("y")).unwrap();
        assert_eq!(requests, [Request::RunKeyHandler]);
        let requests = session.run_key_handler();
        if quiet {
            assert_eq!(requests, [], "-q keeps quiet");
        } else {
            let prefix = format!("{}: key-handler failed: ", broken.display());
            assert!(
                matches!(&requests[..], [Request::Warning(w)] if w.starts_with(&prefix)),
                "{requests:?}"
            );
        }
        let none = vec![dir.join("none")];
        let (mut session, wakes, _) =
            open_with(&files[..1], options, &Config::default(), none, Bar::Drawn);
        settle(&mut session, &wakes);
        session.key(&Keystroke::ctrl("x")).unwrap();
        let requests = session.key(&Keystroke::new("y")).unwrap();
        assert_eq!(requests.is_empty(), quiet, "no request at all under -q");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn removing_the_last_file_in_the_grid_quits() {
    let dir = scratch("last");
    let path = dir.join("solid.png");
    solid_png(&path, [255, 0, 0]);
    let (mut session, wakes, _) = open(
        std::slice::from_ref(&path),
        grid_options(),
        &Config::default(),
    );
    let placeholder = frame(&mut session);
    changed(&mut session, &wakes, &placeholder);
    let requests = session.key(&Keystroke::new("D")).unwrap();
    assert!(requests.contains(&Request::Quit(vec![])), "{requests:?}");
    // a frame and answers after it, as a frontend may still draw before it exits
    frame(&mut session);
    session.answers();
    std::fs::remove_dir_all(dir).unwrap();
}

fn at(x: f64, y: f64) -> nsxiv_ng_core::Point {
    nsxiv_ng_core::Point { x, y }
}

// a click of the left button: a press and a release in one place
fn click(session: &mut Session, x: f64, y: f64, time: u32) {
    session.press(Button::Left, at(x, y), time);
    session.release(Button::Left, at(x, y));
}

#[test]
fn a_drag_pans_and_its_release_does_not_navigate() {
    let files = [
        testdata("fixtures/photo.jpg"),
        testdata("fixtures/checker.png"),
    ];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    session.key(&Keystroke::new("=")).unwrap();
    let before = session.viewer().viewport().image_rect();
    session.press(Button::Left, at(128.0, 96.0), 0);
    session.pointer_moved(at(100.0, 80.0));
    session.pointer_moved(at(250.0, 80.0));
    // released in the zone that would go on
    session.release(Button::Left, at(250.0, 80.0));
    let after = session.viewer().viewport().image_rect();
    assert_eq!(
        (after.x - before.x, after.y - before.y),
        (122.0, 16.0),
        "the image follows the pointer, y upward"
    );
    assert_eq!(session.viewer().current(), 0, "a drag does not navigate");
}

#[test]
fn a_click_navigates_at_either_side() {
    let files: Vec<PathBuf> = ["photo.jpg", "checker.png", "gradient.png"]
        .map(|name| testdata(&format!("fixtures/{name}")))
        .to_vec();
    let options = Options {
        start_at: 2,
        ..hidden_bar()
    };
    let (mut session, wakes, _) = open(&files, options, &Config::default());
    settle(&mut session, &wakes);
    // the zones are a third of the width, 84.48 of 256, at each side
    click(&mut session, 128.0, 96.0, 0);
    assert_eq!(session.viewer().current(), 1, "nothing between them");
    click(&mut session, 84.0, 96.0, 1000);
    assert_eq!(session.viewer().current(), 0, "back on the left");
    click(&mut session, 172.0, 96.0, 2000);
    assert_eq!(session.viewer().current(), 1, "on at the right");
}

fn grid_session(files: usize) -> (Session, Receiver<()>, Vec<PathBuf>) {
    let all: Vec<PathBuf> = ["photo.jpg", "checker.png", "gradient.png"]
        .map(|name| testdata(&format!("fixtures/{name}")))
        .to_vec();
    let files = all[..files].to_vec();
    let (session, wakes, _) = open(&files, grid_options(), &Config::default());
    (session, wakes, files)
}

#[test]
fn two_clicks_on_a_cell_within_the_time_open_it() {
    // one column of 148-unit cells, from x 54: cell 0 above y 148, cell 1 below
    for (first, second, opens) in [
        (1000, 1399, true),
        (u32::MAX - 100, 200, true),
        (1000, 1400, false),
    ] {
        // inside cell 0, and in the zone where a release in the image view would go on
        let (mut session, _wakes, _) = grid_session(3);
        click(&mut session, 190.0, 70.0, first);
        click(&mut session, 190.0, 70.0, second);
        let viewer = session.viewer();
        assert_eq!(
            (viewer.thumbnail_mode(), viewer.current()),
            (!opens, 0),
            "clicks at {first} and {second}"
        );
    }
    let (mut session, _wakes, _) = grid_session(3);
    click(&mut session, 128.0, 170.0, 1000);
    assert_eq!(session.viewer().current(), 1, "a click selects");
    // the selection scrolled cell 1 into view, which leaves cell 0 above y 44
    assert_eq!(session.viewer().grid().scroll_y(), 104.0);
    click(&mut session, 128.0, 20.0, 1100);
    assert!(
        session.viewer().thumbnail_mode() && session.viewer().current() == 0,
        "a click on another cell only selects"
    );
    click(&mut session, 20.0, 20.0, 1200);
    assert_eq!(session.viewer().current(), 0, "beside the cells, nothing");
}

#[test]
fn the_right_button_switches_mode_or_marks() {
    let (mut session, _wakes, _) = grid_session(3);
    session.press(Button::Right, at(128.0, 170.0), 0);
    let viewer = session.viewer();
    assert!(
        viewer.files()[1].marked && viewer.current() == 0,
        "the grid marks without moving the selection"
    );
    session.release(Button::Right, at(128.0, 170.0));
    session.key(&Keystroke::new("Return")).unwrap();
    assert!(!session.viewer().thumbnail_mode());
    session.press(Button::Right, at(10.0, 10.0), 10);
    assert!(
        session.viewer().thumbnail_mode(),
        "the image view switches to the grid"
    );
}

#[test]
fn the_wheel_and_the_touchpad_scroll_the_grid_or_zoom_and_pan() {
    let (mut session, _wakes, _) = grid_session(3);
    session.scroll(Scroll::Steps(1));
    let third = 148.0 / 3.0;
    assert_eq!(
        session.viewer().grid().scroll_y(),
        third,
        "a third of a cell a step"
    );
    session.scroll(Scroll::Distance { dx: 0.0, dy: 10.0 });
    assert_eq!(session.viewer().grid().scroll_y(), third + 10.0);
    session.scroll(Scroll::WheelDistance(7.0));
    assert_eq!(
        session.viewer().grid().scroll_y(),
        third + 17.0,
        "a wheel without steps scrolls the grid by its distance"
    );

    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg")],
        hidden_bar(),
        &Config::default(),
    );
    settle(&mut session, &wakes);
    session.key(&Keystroke::new("=")).unwrap();
    session.scroll(Scroll::Steps(-1));
    assert_eq!(session.viewer().viewport().zoom(), 1.5, "up zooms in");
    session.scroll(Scroll::Steps(2));
    assert_eq!(
        session.viewer().viewport().zoom(),
        0.75,
        "down zooms out, a level a step"
    );
    session.scroll(Scroll::WheelDistance(30.0));
    assert_eq!(
        session.viewer().viewport().zoom(),
        0.5,
        "a wheel without steps zooms one level"
    );
    session.scroll(Scroll::Steps(i32::MIN));
    assert_eq!(
        session.viewer().viewport().zoom(),
        8.0,
        "as far as the levels go"
    );
    session.scroll(Scroll::Steps(i32::MAX));
    assert_eq!(session.viewer().viewport().zoom(), 0.125);
    session.key(&Keystroke::new("=")).unwrap();
    let before = session.viewer().viewport().image_rect();
    session.scroll(Scroll::Distance { dx: 10.0, dy: 5.0 });
    let after = session.viewer().viewport().image_rect();
    assert_eq!(
        (after.x - before.x, after.y - before.y),
        (-10.0, 5.0),
        "a touchpad pans"
    );
}

#[test]
fn a_zoom_at_the_pointer_keeps_the_point_under_it_within_the_bounds() {
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg")],
        Options::default(),
        &Config::default(),
    );
    settle(&mut session, &wakes);
    session.key(&Keystroke::new("=")).unwrap();
    let canvas = session.viewer().viewport().view_size().height;
    // where the image's point under `at` is on screen, y down from the canvas's top
    let under = |session: &Session, image: (f64, f64)| {
        let viewport = session.viewer().viewport();
        let rect = viewport.image_rect();
        let zoom = viewport.zoom();
        (rect.x + image.0 * zoom, canvas - (rect.y + image.1 * zoom))
    };
    let image_at = |session: &Session, at: (f64, f64)| {
        let viewport = session.viewer().viewport();
        let rect = viewport.image_rect();
        let zoom = viewport.zoom();
        ((at.0 - rect.x) / zoom, (canvas - at.1 - rect.y) / zoom)
    };
    let pointer = (60.0, 50.0);
    for (factor, zoom) in [(1.5, 1.5), (1.1, 1.5 * 1.1), (1e9, 8.0)] {
        let point = image_at(&session, pointer);
        let requests = session.zoom_at(factor, at(pointer.0, pointer.1));
        assert!(!requests.is_empty(), "a zoom by {factor} redraws");
        assert_eq!(session.viewer().viewport().zoom(), zoom, "by {factor}");
        let (x, y) = under(&session, point);
        assert!(
            (x - pointer.0).abs() < 1.0 && (y - pointer.1).abs() < 1.0,
            "the point stays under the pointer by {factor}, at {x}, {y}"
        );
    }
    // the bottom bound, where the image no longer fills the canvas and the clamp centers it
    session.zoom_at(1e-9, at(pointer.0, pointer.1));
    let viewport = session.viewer().viewport();
    let rect = viewport.image_rect();
    let view = viewport.view_size();
    assert_eq!(viewport.zoom(), 0.125 / 4.0);
    assert_eq!(
        (rect.x, rect.y),
        (
            (view.width - rect.width) / 2.0,
            (view.height - rect.height) / 2.0
        )
    );
    let before = (viewport.zoom(), rect);
    for factor in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(session.zoom_at(factor, at(10.0, 10.0)).is_empty());
        let viewport = session.viewer().viewport();
        assert_eq!(
            (viewport.zoom(), viewport.image_rect()),
            before,
            "by {factor}"
        );
    }

    let (mut session, _wakes, _) = grid_session(3);
    let scroll = session.viewer().grid().scroll_y();
    assert!(session.zoom_at(2.0, at(128.0, 96.0)).is_empty());
    assert_eq!(
        session.viewer().grid().scroll_y(),
        scroll,
        "nothing in the grid"
    );
    assert!(session.viewer().thumbnail_mode());
}

#[test]
fn a_palette_set_colors_the_whole_frame_after_it() {
    let options = Options {
        thumbnail_mode: true,
        private_mode: true,
        ..Options::default()
    };
    let files = [
        testdata("fixtures/photo.jpg"),
        testdata("fixtures/checker.png"),
    ];
    let (mut session, wakes, _) = open(&files, options, &shown_bar());
    while !session.frame_is_final() || session.bar() == 0.0 {
        wait(&wakes);
        session.answers();
    }
    // a mark, for its color
    session.key(&Keystroke::new("m")).unwrap();
    let palette = Palette {
        background: [1, 2, 3],
        foreground: [240, 120, 60],
        bar_background: [7, 8, 9],
        selection: [10, 11, 12],
        mark: [13, 14, 15],
        placeholder: [16, 17, 18],
        error: [19, 20, 21],
    };
    assert_eq!(
        session.set_palette(palette),
        vec![Request::Redraw { bar_only: false }]
    );
    let pixels = frame(&mut session);
    let rgb = |[r, g, b]: [u8; 3]| canvas::rgb(r, g, b);
    assert_eq!(
        pixels[0],
        rgb(palette.background),
        "the background beside the cells"
    );
    let bottom = pixels[pixels.len() - WIDTH as usize];
    assert_eq!(bottom, rgb(palette.bar_background), "the bar");
    assert!(
        pixels.contains(&rgb(palette.selection)),
        "the selection's frame"
    );
    assert!(pixels.contains(&rgb(palette.mark)), "the mark");
    let bar = &pixels[pixels.len() - WIDTH as usize * session.bar() as usize..];
    assert!(bar.contains(&rgb(palette.foreground)), "the bar's text");
}

#[test]
fn a_press_on_the_bar_does_not_reach_the_canvas() {
    let files: Vec<PathBuf> = ["photo.jpg", "checker.png"]
        .map(|name| testdata(&format!("fixtures/{name}")))
        .to_vec();
    let options = Options {
        start_at: 2,
        ..Options::default()
    };
    let (mut session, wakes, _) = open(&files, options, &shown_bar());
    settle(&mut session, &wakes);
    // the fonts tell the bar's height
    while session.viewer().viewport().view_size().height == f64::from(HEIGHT) {
        wait(&wakes);
        session.answers();
    }
    click(&mut session, 10.0, 190.0, 0);
    assert_eq!(
        session.viewer().current(),
        1,
        "a click on the bar does not navigate"
    );
    click(&mut session, 10.0, 20.0, 1000);
    assert_eq!(session.viewer().current(), 0);
}

#[test]
fn a_bar_the_frontend_shows_takes_its_room() {
    let options = Options::default();
    let files = [testdata("fixtures/photo.jpg")];
    let (mut session, wakes, _) =
        open_with(&files, options, &shown_bar(), vec![], Bar::Shown(16.0));
    settle(&mut session, &wakes);
    let height = |session: &Session| session.viewer().viewport().view_size().height;
    assert_eq!(height(&session), 176.0, "one row of 16 for the bar");
    session.key(&Keystroke::new("b")).unwrap();
    assert_eq!(
        height(&session),
        192.0,
        "the bar hidden, with no call from the frontend"
    );
    session.key(&Keystroke::new("b")).unwrap();
    assert_eq!(height(&session), 176.0);
    // the canvas drawn is the part above the bar, and the session draws no bar into it
    let mut pixels = vec![0; (WIDTH * 176) as usize];
    let mut canvas = Canvas::new(&mut pixels, WIDTH, 176).unwrap();
    session.draw(&mut canvas, 1.0, Frame::default());
    assert!(!pixels.contains(&canvas::rgb(0xee, 0xee, 0xee)));
}

#[test]
fn a_redraw_says_whether_the_bar_alone_changed() {
    let files = [
        testdata("fixtures/photo.jpg"),
        testdata("fixtures/checker.png"),
    ];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    let redraw = |requests: &[Request]| {
        requests.iter().find_map(|r| match r {
            Request::Redraw { bar_only } => Some(*bar_only),
            _ => None,
        })
    };
    // m marks, and moves on, whose image comes once it decodes
    let requests = session.key(&Keystroke::new("m")).unwrap();
    assert_eq!(
        redraw(&requests),
        Some(true),
        "a mark changes the bar alone: {requests:?}"
    );
    let requests = settle(&mut session, &wakes);
    assert_eq!(redraw(&requests), Some(false), "the image that came");
    let requests = session.key(&Keystroke::new("Return")).unwrap();
    assert_eq!(redraw(&requests), Some(false));
    let requests = session.key(&Keystroke::new("m")).unwrap();
    assert_eq!(redraw(&requests), Some(false), "marks show in the grid");
}

fn drawn(session: &mut Session, size: (u32, u32), frame: Frame) -> (Vec<u32>, bool) {
    let mut pixels = vec![0x1234_5678; (size.0 * size.1) as usize];
    let mut canvas = Canvas::new(&mut pixels, size.0, size.1).unwrap();
    let opaque = session.draw(&mut canvas, 1.0, frame);
    (pixels, opaque)
}

#[test]
fn a_frame_may_leave_the_background_clear_or_show_the_image_whole() {
    let clear = Frame {
        clear: true,
        ..Frame::default()
    };
    // the gradient fits 256 x 160, with bands above and below
    let gradient = testdata("fixtures/gradient.png");
    let (mut session, wakes, _) = open(
        std::slice::from_ref(&gradient),
        hidden_bar(),
        &Config::default(),
    );
    settle(&mut session, &wakes);
    let (pixels, opaque) = drawn(&mut session, (WIDTH, HEIGHT), clear);
    assert!(!opaque);
    assert!(
        pixels[..WIDTH as usize * 16].iter().all(|&p| p == 0),
        "clear above"
    );
    assert!(
        pixels[WIDTH as usize * 100] >> 24 == 0xff,
        "the image opaque"
    );
    let (pixels, opaque) = drawn(&mut session, (WIDTH, HEIGHT), Frame::default());
    assert!(opaque && pixels[0] == WHITE, "the background, opaque");
    // whole: the image alone at the canvas's size, here its own
    let whole = Frame {
        clear: true,
        whole: true,
        ..Frame::default()
    };
    let (pixels, opaque) = drawn(&mut session, (320, 200), whole);
    let decoded = nsxiv_ng_decode::decode(&gradient, 1 << 30).unwrap();
    let expected: Vec<u32> = decoded
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&[r, g, b, _]| canvas::rgb(r, g, b))
        .collect();
    assert!(
        opaque && pixels == expected,
        "the image alone, pixel for pixel"
    );

    // the photo covers the frame
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg")],
        hidden_bar(),
        &Config::default(),
    );
    settle(&mut session, &wakes);
    assert!(
        drawn(&mut session, (WIDTH, HEIGHT), clear).1,
        "an opaque image that covers"
    );
    // one with alpha does not, even filling the frame
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/alpha.png")],
        hidden_bar(),
        &Config::default(),
    );
    settle(&mut session, &wakes);
    session.key(&Keystroke::new("F")).unwrap();
    assert!(
        !drawn(&mut session, (WIDTH, HEIGHT), clear).1,
        "an image with alpha"
    );
    let (mut session, _wakes, _) = grid_session(1);
    let (pixels, opaque) = drawn(&mut session, (WIDTH, HEIGHT), clear);
    assert!(!opaque && pixels[0] == 0, "the grid on a clear canvas");
    assert!(drawn(&mut session, (WIDTH, HEIGHT), Frame::default()).1);
}

#[test]
fn a_frame_at_another_scale_leaves_the_thumbnails_as_they_are() {
    let (mut session, wakes, _) = grid_session(1);
    let placeholder = frame(&mut session);
    let shown = changed(&mut session, &wakes, &placeholder);
    for scale in [0.5, 2.0] {
        let mut pixels = vec![0; (WIDTH * HEIGHT) as usize];
        let mut canvas = Canvas::new(&mut pixels, WIDTH, HEIGHT).unwrap();
        session.draw(&mut canvas, scale, Frame::default());
    }
    assert_eq!(frame(&mut session), shown, "the thumbnail stays");
}

#[test]
fn a_clear_frame_stays_clear_around_the_image_while_the_next_file_decodes() {
    let dir = scratch("clear-while-decoding");
    // a FIFO: its decode waits in open until the test opens the other end
    let slow = dir.join("slow.png");
    let made = std::process::Command::new("mkfifo")
        .arg(&slow)
        .status()
        .unwrap();
    assert!(made.success());
    let files = [testdata("fixtures/gradient.png"), slow.clone()];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    session.key(&Keystroke::new("j")).unwrap();
    assert!(session.viewer().image().is_none() && session.viewer().error_text().is_none());
    let clear = Frame {
        clear: true,
        ..Frame::default()
    };
    // the gradient, still on screen, fits 256 x 160 with bands above and below
    let (pixels, opaque) = drawn(&mut session, (WIDTH, HEIGHT), clear);
    assert!(!opaque);
    assert!(
        pixels[..WIDTH as usize * 16].iter().all(|&p| p == 0),
        "clear above"
    );
    assert!(pixels[WIDTH as usize * 100] >> 24 == 0xff, "the old image");
    drop(std::fs::OpenOptions::new().write(true).open(&slow).unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_title_bar_is_the_status_bar_s_height_in_its_font_and_colors() {
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg")],
        hidden_bar(),
        &Config::default(),
    );
    while session.title_height() == 0.0 {
        wait(&wakes);
        session.answers();
    }
    let (ready, fonts) = mpsc::channel();
    let mut text = Text::new("monospace", 13.0, move || {
        let _ = ready.send(());
    });
    fonts.recv_timeout(Duration::from_secs(60)).unwrap();
    let height = bar::height(&mut text).unwrap();
    assert_eq!(session.title_height(), height, "the line of Ag and 8");
    let rows = height as u32 * 2;
    let mut drawn = vec![0; (WIDTH * rows) as usize];
    let mut canvas = Canvas::new(&mut drawn, WIDTH, rows).unwrap();
    session.draw_title(&mut canvas, "photo.jpg", 2.0);
    let mut expected = vec![0; (WIDTH * rows) as usize];
    let mut canvas = Canvas::new(&mut expected, WIDTH, rows).unwrap();
    let colors = (canvas::rgb(0xee, 0xee, 0xee), canvas::rgb(0, 0, 0));
    bar::title(&mut canvas, &mut text, "photo.jpg", 2.0, colors);
    assert!(drawn == expected);
    assert!(drawn.contains(&canvas::rgb(0, 0, 0)), "with text in it");
}

fn forgets(requests: &[Request]) -> Vec<Vec<PathBuf>> {
    requests
        .iter()
        .filter_map(|r| match r {
            Request::Forget(paths) => Some(paths.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_changed_or_invalidated_file_is_told() {
    let dir = scratch("forget");
    let [a, b] = ["a.png", "b.png"].map(|name| dir.join(name));
    solid_png(&a, [255, 0, 0]);
    solid_png(&b, [0, 255, 0]);
    let files = [a.clone(), b.clone()];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    // the shown file rewritten: the watch, its quiet time, the reload
    solid_png(&a, [0, 0, 255]);
    session.file_changed();
    let due = session.deadline().unwrap();
    let requests = session.timers(due);
    assert_eq!(forgets(&requests), [vec![a.clone()]], "{requests:?}");
    // as a frontend that keeps frames: one drawn while the reload decodes shows the old image
    let _ = frame(&mut session);
    let mut requests = Vec::new();
    // a reload keeps the image on screen until its answer is in
    while session.viewer().awaited().is_some() {
        wait(&wakes);
        requests.extend(session.answers());
    }
    let last = |wanted: fn(&Request) -> bool| requests.iter().rposition(wanted);
    let forget = last(|r| matches!(r, Request::Forget(paths) if paths.len() == 1));
    let redraw = last(|r| matches!(r, Request::Redraw { bar_only: false }));
    assert!(
        forget.is_some() && forget < redraw,
        "told again before the frame of the new image: {requests:?}"
    );
    // rewritten while another shows, and found changed on the way back
    session.key(&Keystroke::new("j")).unwrap();
    settle(&mut session, &wakes);
    // another size, as a write within the clock's tick may keep the times
    image::RgbImage::from_pixel(32, 32, image::Rgb([255, 255, 0]))
        .save(&a)
        .unwrap();
    let mut requests = session.key(&Keystroke::new("k")).unwrap();
    requests.extend(settle(&mut session, &wakes));
    assert_eq!(forgets(&requests), [vec![a.clone()]], "{requests:?}");
    session.key(&Keystroke::new("j")).unwrap();
    let mut requests = settle(&mut session, &wakes);
    requests.extend(session.key(&Keystroke::new("k")).unwrap());
    requests.extend(settle(&mut session, &wakes));
    assert_eq!(
        forgets(&requests),
        [] as [Vec<PathBuf>; 0],
        "unchanged files: none"
    );
    // a file known before that can no longer be read
    session.key(&Keystroke::new("j")).unwrap();
    settle(&mut session, &wakes);
    std::fs::remove_file(&a).unwrap();
    let mut requests = session.key(&Keystroke::new("k")).unwrap();
    requests.extend(settle(&mut session, &wakes));
    assert_eq!(forgets(&requests), [vec![a.clone()]], "{requests:?}");
    solid_png(&a, [255, 0, 0]);
    // an invalidation in the grid
    session.key(&Keystroke::new("Return")).unwrap();
    let requests = session.key(&Keystroke::new("R")).unwrap();
    assert_eq!(forgets(&requests), [vec![a, b]]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A session over `files` that wants the stills of the neighbors.
fn open_for_stills(files: &[PathBuf]) -> (Session, Receiver<()>) {
    let (sender, wakes) = mpsc::channel();
    let files = files.iter().cloned().map(FileEntry::new).collect();
    let wake = move || {
        let _ = sender.send(());
    };
    let program = PathBuf::from("nsxiv-ng");
    let config = Config::default();
    let mut session = Session::new(
        hidden_bar(),
        files,
        &config,
        program,
        vec![],
        Bar::Drawn,
        wake,
    );
    session.want_stills();
    let window = Size {
        width: f64::from(WIDTH),
        height: f64::from(HEIGHT),
    };
    session.resize(window, 1.0);
    session.start();
    (session, wakes)
}

/// Takes in answers until the still of `path` is in, adding their requests to `told`, which
/// holds those of the calls before, and checks that a request named it.
fn still_in(session: &mut Session, wakes: &Receiver<()>, path: &Path, told: &mut Vec<Request>) {
    loop {
        told.extend(session.answers());
        if session.still(path).is_some() {
            assert!(told.contains(&Request::Still(path.to_owned())), "{told:?}");
            return;
        }
        wait(wakes);
    }
}

#[test]
fn a_still_drawn_ahead_is_the_frame_drawn_once_the_viewer_moves_there() {
    let [checker, photo, alpha] =
        ["checker.png", "photo.jpg", "alpha.png"].map(|name| testdata(&format!("fixtures/{name}")));
    let (mut session, wakes) = open_for_stills(&[checker, photo.clone(), alpha.clone()]);
    let mut told = settle(&mut session, &wakes);
    // a setting that lasts from file to file
    told.extend(session.key(&Keystroke::new("}")).unwrap());
    let frames = [
        Frame::default(),
        Frame {
            clear: true,
            ..Frame::default()
        },
        Frame {
            clear: true,
            whole: true,
            ..Frame::default()
        },
    ];
    // the photo is larger than the canvas, and alpha.png has alpha
    for next in [photo, alpha] {
        still_in(&mut session, &wakes, &next, &mut told);
        let still = session.still(&next).expect("a still");
        let ahead: Vec<(Vec<u32>, bool)> = frames
            .iter()
            .map(|&frame| {
                let mut pixels = vec![0x1234_5678; (WIDTH * HEIGHT) as usize];
                let mut canvas = Canvas::new(&mut pixels, WIDTH, HEIGHT).unwrap();
                let opaque = still.draw(&mut canvas, 1.0, frame);
                (pixels, opaque)
            })
            .collect();
        told = session.key(&Keystroke::new("j")).unwrap();
        told.extend(settle(&mut session, &wakes));
        for (&frame, ahead) in frames.iter().zip(&ahead) {
            assert!(
                drawn(&mut session, (WIDTH, HEIGHT), frame) == *ahead,
                "{next:?} with {frame:?}"
            );
        }
        assert!(session.still(&next).is_none(), "the viewer is there now");
    }
}

#[test]
fn a_file_not_decoded_ahead_gives_no_still() {
    let files: Vec<PathBuf> = ["photo.jpg", "checker.png", "alpha.png"]
        .map(|name| testdata(&format!("fixtures/{name}")))
        .to_vec();
    let (mut session, wakes) = open_for_stills(&files);
    settle(&mut session, &wakes);
    // no neighbor of the first file, so never decoded ahead
    assert!(session.still(&files[2]).is_none());
    // and a session that wants no stills is told of none
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    let requests = settle(&mut session, &wakes);
    assert!(!requests.iter().any(|r| matches!(r, Request::Still(_))));
    assert!(session.still(&files[1]).is_none());
}

#[test]
fn a_still_is_given_only_for_a_neighbor_now() {
    let files: Vec<PathBuf> = ["checker.png", "photo.jpg", "alpha.png", "gradient.png"]
        .map(|name| testdata(&format!("fixtures/{name}")))
        .to_vec();
    let (mut session, wakes) = open_for_stills(&files);
    let mut told = settle(&mut session, &wakes);
    still_in(&mut session, &wakes, &files[1], &mut told);
    // into the grid and on to the last file, which loads nothing
    for key in ["Return", "j", "j", "j"] {
        session.key(&Keystroke::new(key)).unwrap();
    }
    assert_eq!(session.viewer().current(), 3);
    assert!(
        session.still(&files[1]).is_none(),
        "no neighbor of the last file"
    );
}

fn redraws(requests: &[Request]) -> Vec<bool> {
    requests
        .iter()
        .filter_map(|r| match r {
            Request::Redraw { bar_only } => Some(*bar_only),
            _ => None,
        })
        .collect()
}

#[test]
fn thumbnails_that_wait_give_one_redraw_and_a_key_s_comes_at_once() {
    let files: Vec<PathBuf> = ["photo.jpg", "checker.png", "gradient.png"]
        .map(|name| testdata(&format!("fixtures/{name}")))
        .to_vec();
    // 176 high above the bar's row: three cells and the fonts, each with a wake
    let open_waiting = || {
        let (mut session, wakes, _) = open_with(
            &files,
            grid_options(),
            &Config::default(),
            vec![],
            Bar::Shown(16.0),
        );
        session.wait_for_thumbnails(Duration::from_secs(60));
        (session, wakes)
    };
    let (mut session, wakes) = open_waiting();
    let mut first_due = None;
    for _ in 0..4 {
        wait(&wakes);
        let requests = session.answers();
        assert_eq!(
            redraws(&requests),
            [] as [bool; 0],
            "no frame while they come"
        );
        first_due = first_due.or(session.deadline());
        assert_eq!(
            session.deadline(),
            first_due,
            "the wait from the first one in"
        );
    }
    let due = session.deadline().expect("a frame due");
    assert_eq!(session.timers(due - MS), []);
    assert_eq!(session.timers(due), [Request::Redraw { bar_only: false }]);
    assert_eq!(session.deadline(), None);

    let (mut session, wakes) = open_waiting();
    while session.deadline().is_none() {
        wait(&wakes);
        session.answers();
    }
    let requests = session.key(&Keystroke::new("j")).unwrap();
    assert_eq!(redraws(&requests), [false], "a key's frame comes at once");
    assert_eq!(
        session.deadline(),
        None,
        "and shows the thumbnails that waited"
    );
}

#[test]
fn the_fonts_lay_out_again_only_a_bar_the_session_draws() {
    let grid = Options {
        thumbnail_mode: true,
        private_mode: true,
        ..Options::default()
    };
    let height = |session: &Session| session.viewer().viewport().view_size().height;
    // with no files, the fonts' wake is the only one
    for (options, bar, after, redraw) in [
        (grid.clone(), Bar::Shown(16.0), 176.0, false),
        // "no images", the text of what went wrong
        (Options::default(), Bar::Shown(16.0), 176.0, true),
    ] {
        let (mut session, wakes, _) = open_with(&[], options, &shown_bar(), vec![], bar);
        assert_eq!(height(&session), 176.0);
        wait(&wakes);
        let requests = session.answers();
        assert_eq!(height(&session), after, "no layout under {bar:?}");
        assert_eq!(
            redraws(&requests),
            if redraw { vec![false] } else { vec![] },
            "a redraw only where the fonts' text shows: {requests:?}"
        );
    }
    let (mut session, wakes, _) = open_with(&[], grid, &shown_bar(), vec![], Bar::Drawn);
    wait(&wakes);
    session.answers();
    let bar = session.title_height();
    assert!(bar > 0.0);
    assert_eq!(
        height(&session),
        f64::from(HEIGHT) - bar,
        "a layout for the bar drawn"
    );
}

#[test]
fn a_bar_font_no_installed_family_has_is_told_once_unless_quiet() {
    // the family used instead is the first monospaced one installed
    let named = "bar_font: no installed font family is named \"No Such Family\"; ";
    for (family, quiet, told) in [
        ("No Such Family", false, 1),
        ("No Such Family", true, 0),
        ("dejavu sans mono", false, 0),
    ] {
        let config = Config {
            bar_font: family.to_owned(),
            ..Config::default()
        };
        let options = Options {
            quiet,
            ..hidden_bar()
        };
        // what resize and start gave counts too, as the fonts may be in by then
        let (mut session, wakes, mut requests) =
            open(&[testdata("fixtures/gradient.png")], options, &config);
        requests.extend(settle(&mut session, &wakes));
        while session.title_height() == 0.0 {
            wait(&wakes);
            requests.extend(session.answers());
        }
        // and none more after
        requests.extend(session.answers());
        let warnings: Vec<&Request> = requests
            .iter()
            .filter(|r| matches!(r, Request::Warning(w) if w.starts_with("bar_font")))
            .collect();
        assert_eq!(warnings.len(), told, "{family} {quiet}: {requests:?}");
        if let [Request::Warning(warning)] = warnings[..] {
            assert!(
                warning.starts_with(named) && warning.ends_with(" is used instead"),
                "{warning}"
            );
        }
    }
}

#[test]
fn edit_mode_on_a_frontend_that_cannot_edit_warns_unless_quiet_and_beeps() {
    for quiet in [false, true] {
        let options = Options {
            quiet,
            ..hidden_bar()
        };
        let (mut session, wakes, _) = open(
            &[testdata("fixtures/photo.jpg")],
            options,
            &Config::default(),
        );
        settle(&mut session, &wakes);
        let requests = session.key(&Keystroke::new("x")).unwrap();
        let warning = "this frontend has no edit mode yet";
        let expected = if quiet {
            vec![Request::Beep]
        } else {
            vec![Request::Warning(warning.to_owned()), Request::Beep]
        };
        assert_eq!(requests, expected);
    }
}

#[test]
fn the_bar_drawn_alone_is_the_bar_of_the_whole_frame() {
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg")],
        Options::default(),
        &shown_bar(),
    );
    settle(&mut session, &wakes);
    while session.title_height() == 0.0 {
        wait(&wakes);
        session.answers();
    }
    let rows = session.bar_rows(HEIGHT, 1.0);
    assert!(rows > 0);
    let mut kept = frame(&mut session);
    // a mark changes the bar alone, which a frontend redraws over the frame it keeps
    let requests = session.key(&Keystroke::new("m")).unwrap();
    assert_eq!(requests.last(), Some(&Request::Redraw { bar_only: true }));
    let top = ((HEIGHT - rows) * WIDTH) as usize;
    let mut canvas = Canvas::new(&mut kept[top..], WIDTH, rows).unwrap();
    session.draw_bar(&mut canvas, 1.0);
    assert!(
        kept == frame(&mut session),
        "the same frame as a whole redraw"
    );
    let bar = Bar::Shown(16.0);
    let (session, _, _) = open_with(&[], hidden_bar(), &Config::default(), vec![], bar);
    assert_eq!(
        session.bar_rows(HEIGHT, 1.0),
        0,
        "none where the frontend shows it"
    );
}

/// What a frontend that keeps its frame shows after the requests: the bar redrawn over it
/// alone where only the bar changed, else a whole frame.
fn kept_after(session: &mut Session, kept: &mut Vec<u32>, requests: &[Request]) {
    for request in requests {
        match request {
            Request::Redraw { bar_only: true } => {
                let rows = session.bar_rows(HEIGHT, 1.0);
                let top = ((HEIGHT - rows) * WIDTH) as usize;
                let mut canvas = Canvas::new(&mut kept[top..], WIDTH, rows).unwrap();
                session.draw_bar(&mut canvas, 1.0);
            }
            Request::Redraw { bar_only: false } => *kept = frame(session),
            _ => {}
        }
    }
}

#[test]
fn a_frame_kept_with_the_bar_redrawn_alone_is_a_whole_redraw() {
    let dir = scratch("kept");
    let bad = dir.join("bad.png");
    std::fs::write(&bad, b"not an image").unwrap();
    let slow = dir.join("slow.png");
    let made = std::process::Command::new("mkfifo")
        .arg(&slow)
        .status()
        .unwrap();
    assert!(made.success());
    for (first, keys) in [
        // the text of what went wrong, then a file that decodes, which waits in open
        (bad, vec!["j"]),
        // a photo, then a gamma step while the next file decodes
        (testdata("fixtures/photo.jpg"), vec!["j", "}"]),
    ] {
        let files = [first, slow.clone()];
        let (mut session, wakes, _) = open(&files, Options::default(), &Config::default());
        settle(&mut session, &wakes);
        while session.title_height() == 0.0 {
            wait(&wakes);
            session.answers();
        }
        let mut kept = frame(&mut session);
        for key in keys {
            let requests = session.key(&Keystroke::new(key)).unwrap();
            kept_after(&mut session, &mut kept, &requests);
            assert!(kept == frame(&mut session), "{files:?} after {key}");
        }
        assert!(
            session.viewer().awaited().is_some(),
            "the next file still decodes"
        );
    }
    // the slow file's decodes end, and the folder goes; opened to read too, which waits for no
    // reader, as a decode that never started would leave none
    let ends = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slow);
    drop(ends.unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_frame_is_final_once_the_fonts_are_in_and_an_image_its_error_or_the_grid_shows() {
    let photo = testdata("fixtures/photo.jpg");
    let missing = PathBuf::from("/nonexistent/missing.png");
    let cases = [
        (photo.clone(), Options::default(), "an image"),
        (missing, Options::default(), "the text of what went wrong"),
        (photo, grid_options(), "the grid"),
    ];
    for (file, options, what) in cases {
        let (mut session, wakes, _) = open(&[file], options, &Config::default());
        if what != "the grid" {
            assert!(
                !session.frame_is_final(),
                "{what}: nothing shows before it decodes"
            );
        }
        while !session.frame_is_final() {
            wait(&wakes);
            session.answers();
        }
        let viewer = session.viewer();
        let shows =
            viewer.thumbnail_mode() || viewer.image().is_some() || viewer.error_text().is_some();
        assert!(shows, "{what}");
        // the bar's and a title bar's rows are known by then, with a font installed as here,
        // though the fonts may have come before the answers took them in
        assert!(session.title_height() > 0.0, "{what}");
        session.answers();
        assert!(session.frame_is_final(), "{what}: and stays final");
    }
}

#[test]
fn with_a_bar_the_session_draws_the_answers_that_make_the_frame_final_ask_for_a_redraw() {
    // a frontend shows its window with the first redraw once the frame is final; with the bar
    // hidden only the fonts' layout asks for it, where the photo decodes first
    let (mut session, wakes, _) = open(
        &[testdata("fixtures/photo.jpg")],
        hidden_bar(),
        &Config::default(),
    );
    loop {
        wait(&wakes);
        let requests = session.answers();
        if session.frame_is_final() {
            assert!(
                requests.contains(&Request::Redraw { bar_only: false }),
                "{requests:?}"
            );
            break;
        }
    }
}

#[test]
fn fonts_the_answers_have_yet_to_take_in_leave_the_frame_not_final() {
    // one wake for the photo and one for the fonts; the second waits, not taken in. Where the
    // first answers took the photo in and not the fonts, the fonts are in but the rows are not,
    // so the frame is not final yet; where they took both in, there is nothing to tell
    for _ in 0..20 {
        let (mut session, wakes, _) = open(
            &[testdata("fixtures/orientation-1.jpg")],
            Options::default(),
            &Config::default(),
        );
        wait(&wakes);
        session.answers();
        let fonts_taken = session.title_height() > 0.0;
        wait(&wakes);
        if session.viewer().image().is_some() && !fonts_taken {
            assert!(
                !session.frame_is_final(),
                "final before the bar's rows are known"
            );
            session.answers();
            assert!(session.frame_is_final());
        }
    }
}

/// The names of the files in the session's list, in order.
fn names(session: &Session) -> Vec<String> {
    (session.viewer().files().iter())
        .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn fixture(name: &str) -> PathBuf {
    testdata(&format!("fixtures/{name}"))
}

#[test]
fn a_file_opened_while_the_viewer_runs_comes_after_the_current_one_and_shows() {
    let files = [fixture("gradient.png"), fixture("checker.png")];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    // m marks the first and goes on to the second
    session.key(&Keystroke::new("m")).unwrap();
    settle(&mut session, &wakes);
    session.open_after_current(vec![fixture("alpha.png")]);
    assert_eq!(
        names(&session),
        ["gradient.png", "checker.png", "alpha.png"]
    );
    assert_eq!(session.viewer().current(), 2);
    assert!(session.viewer().files()[0].marked, "the marks stay");
    settle(&mut session, &wakes);
    assert_eq!(
        shown_size(&session),
        Some((160.0, 160.0)),
        "alpha.png's image shows"
    );
}

/// The size of the image the viewer shows, which tells the fixtures apart.
fn shown_size(session: &Session) -> Option<(f64, f64)> {
    let image = session.viewer().image()?;
    Some((image.size.width, image.size.height))
}

/// A session over no files, as one a frontend made before files were opened in it, given
/// `opened`, which then shows the first of them.
fn opened_into_an_empty_list(opened: &[PathBuf]) {
    let (mut session, wakes, _) = open(&[], hidden_bar(), &Config::default());
    session.open_after_current(opened.to_vec());
    let paths: Vec<_> = (session.viewer().files().iter())
        .map(|f| f.path.clone())
        .collect();
    assert_eq!(paths, opened, "in their order");
    assert_eq!(session.viewer().current(), 0);
    settle(&mut session, &wakes);
    assert_eq!(
        shown_size(&session),
        Some((160.0, 160.0)),
        "the first one's image"
    );
}

#[test]
fn a_file_opened_into_an_empty_list_shows() {
    opened_into_an_empty_list(&[fixture("alpha.png")]);
}

#[test]
fn several_files_opened_into_an_empty_list_come_in_their_order_and_the_first_shows() {
    opened_into_an_empty_list(&[
        fixture("alpha.png"),
        fixture("photo.jpg"),
        fixture("checker.png"),
    ]);
}

#[test]
fn several_files_opened_come_in_their_order_and_the_first_shows() {
    let files = [fixture("gradient.png"), fixture("checker.png")];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    let opened = [
        fixture("alpha.png"),
        fixture("photo.jpg"),
        fixture("alpha.png"),
    ];
    session.open_after_current(opened.to_vec());
    assert_eq!(
        names(&session),
        ["gradient.png", "alpha.png", "photo.jpg", "checker.png"]
    );
    assert_eq!(session.viewer().current(), 1);
}

#[test]
fn a_file_opened_that_the_list_holds_goes_in_no_second_time_and_shows_where_it_is_first() {
    let files = [
        fixture("gradient.png"),
        fixture("checker.png"),
        fixture("alpha.png"),
    ];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    settle(&mut session, &wakes);
    session.open_after_current(vec![fixture("alpha.png"), fixture("photo.jpg")]);
    assert_eq!(
        names(&session),
        ["gradient.png", "photo.jpg", "checker.png", "alpha.png"]
    );
    assert_eq!(
        session.viewer().current(),
        3,
        "alpha.png shows where it was"
    );
    // the current file opened again leaves the list as it is
    session.open_after_current(vec![fixture("alpha.png")]);
    assert_eq!(names(&session).len(), 4);
    assert_eq!(session.viewer().current(), 3);
}

#[test]
fn files_opened_in_an_edit_come_next_and_the_edit_goes_on() {
    let files = [fixture("gradient.png"), fixture("checker.png")];
    let (mut session, wakes, _) = open(&files, hidden_bar(), &Config::default());
    session.can_edit(EditSetup {
        families: vec!["DejaVu Sans".to_owned()],
        field: true,
        recognizer: None,
    });
    settle(&mut session, &wakes);
    while session.title_height() == 0.0 {
        wait(&wakes);
        session.answers();
    }
    session.key(&Keystroke::new("x")).unwrap();
    assert!(session.editing());
    session.open_after_current(vec![fixture("alpha.png"), fixture("photo.jpg")]);
    assert!(session.editing(), "the edit goes on");
    assert_eq!(
        session.viewer().current(),
        0,
        "the file edited stays current"
    );
    assert_eq!(
        names(&session),
        ["gradient.png", "alpha.png", "photo.jpg", "checker.png"]
    );
    // once the edit ends they come next
    session.key(&Keystroke::new("Escape")).unwrap();
    assert!(!session.editing());
    session.key(&Keystroke::new("j")).unwrap();
    assert_eq!(
        session.viewer().current_entry().unwrap().path,
        fixture("alpha.png")
    );
}
