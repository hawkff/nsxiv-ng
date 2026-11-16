//! Edit mode through the session, without a display: `x` enters an edit of the image shown,
//! which takes the keys, the pointer and the canvas until it ends, saves or copies its image,
//! and asks a recognizer on a thread of its own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsxiv_ng_core::config::Config;
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::Options;
use nsxiv_ng_core::viewer::Keystroke;
use nsxiv_ng_core::{Point, Rect, Size};
use nsxiv_ng_edit::{RgbaImage, Word};
use nsxiv_ng_render::{Canvas, canvas};
use nsxiv_ng_session::{Bar, Button, EditSetup, Recognizer, Request, Scroll, Session};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 192;

fn testdata(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(path)
}

// a fresh directory of this test's own
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("nsxiv-ng-edit-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait(wakes: &Receiver<()>) {
    wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
}

/// The requests of the answers that come within two seconds, until one holds `until`.
fn answers(
    session: &mut Session,
    wakes: &Receiver<()>,
    until: impl Fn(&Request) -> bool,
) -> Vec<Request> {
    let mut requests = Vec::new();
    while !requests.iter().any(&until) {
        if wakes.recv_timeout(Duration::from_secs(2)).is_err() {
            break;
        }
        requests.extend(session.answers());
    }
    requests
}

/// A session over `file` in a 256 × 192 window without its bar, the image and the fonts in,
/// which can edit with `recognizer`, in a frontend with a text field of its own.
fn open(
    file: &Path,
    config: &Config,
    recognizer: Option<Arc<dyn Recognizer>>,
) -> (Session, Receiver<()>) {
    open_with(file, config, recognizer, true)
}

/// The same, in a frontend with a text field of its own where `field` says so.
fn open_with(
    file: &Path,
    config: &Config,
    recognizer: Option<Arc<dyn Recognizer>>,
    field: bool,
) -> (Session, Receiver<()>) {
    let (sender, wakes) = mpsc::channel();
    let files = vec![FileEntry::new(file.to_owned())];
    let options = Options {
        no_bar: true,
        ..Options::default()
    };
    let wake = move || {
        let _ = sender.send(());
    };
    let program = PathBuf::from("nsxiv-ng");
    let mut session = Session::new(
        options,
        files,
        config,
        program,
        Vec::new(),
        Bar::Drawn,
        wake,
    );
    session.can_edit(EditSetup {
        families: vec!["DejaVu Sans".to_owned()],
        field,
        recognizer,
    });
    let window = Size {
        width: f64::from(WIDTH),
        height: f64::from(HEIGHT),
    };
    session.resize(window, 1.0);
    session.start();
    while session.viewer().image().is_none() || session.title_height() == 0.0 {
        wait(&wakes);
        session.answers();
    }
    (session, wakes)
}

fn photo() -> PathBuf {
    testdata("fixtures/photo.jpg")
}

fn key(session: &mut Session, name: &str) -> Vec<Request> {
    session.key(&Keystroke::new(name)).expect("a key taken")
}

fn ctrl(session: &mut Session, name: &str) -> Vec<Request> {
    session.key(&Keystroke::ctrl(name)).expect("a key taken")
}

fn p(x: f64, y: f64) -> Point {
    Point { x, y }
}

/// A press at `from`, a drag and a release at `to`, `time` the press's milliseconds.
fn stroke(session: &mut Session, from: Point, to: Point, time: u32) -> Vec<Request> {
    let mut requests = session.press(Button::Left, from, time);
    requests.extend(session.pointer_moved(to));
    requests.extend(session.release(Button::Left, to));
    requests
}

/// The frame the session draws, 256 × 192.
fn frame(session: &mut Session) -> Vec<u32> {
    let mut pixels = vec![0; (WIDTH * HEIGHT) as usize];
    let mut canvas = Canvas::new(&mut pixels, WIDTH, HEIGHT).unwrap();
    session.draw(&mut canvas, 1.0, Default::default());
    pixels
}

const RED: u32 = canvas::rgb(255, 56, 60);

#[test]
fn x_enters_an_edit_that_takes_the_keys_the_pointer_and_the_frame() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    let viewer_frame = frame(&mut session);
    let requests = key(&mut session, "x");
    assert!(session.editing());
    assert_eq!(requests, [Request::Redraw { bar_only: false }]);
    // the photo, 640 × 480, fills the canvas at 40%: a filled rectangle in the edit's red
    for name in ["r", "f", "f"] {
        key(&mut session, name);
    }
    stroke(&mut session, p(50.0, 40.0), p(150.0, 120.0), 0);
    let at = |x: usize, y: usize| y * WIDTH as usize + x;
    let edited = frame(&mut session);
    assert_eq!(edited[at(100, 80)], RED, "the rectangle");
    assert_ne!(viewer_frame[at(100, 80)], RED);
    // the viewer gets no key, no scroll and no zoom meanwhile
    assert!(
        session
            .key(&Keystroke::new("j"))
            .is_some_and(|r| r.is_empty())
    );
    assert_eq!(session.viewer().current(), 0);
    assert_eq!(session.scroll(Scroll::Steps(1)), []);
    assert_eq!(session.zoom_at(2.0, p(10.0, 10.0)), []);
    assert_eq!(session.viewer().viewport().zoom(), 0.4);
    // Control-z takes the rectangle away; q leaves without saving, and q then quits
    ctrl(&mut session, "z");
    assert_ne!(frame(&mut session)[at(100, 80)], RED);
    assert_eq!(
        key(&mut session, "q"),
        [Request::Redraw { bar_only: false }]
    );
    assert!(!session.editing());
    assert_eq!(frame(&mut session), viewer_frame);
    assert!(matches!(key(&mut session, "q")[..], [Request::Quit(_)]));
}

#[test]
fn the_viewers_timers_and_resizes_wait_for_the_edit() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    // a slideshow of one file sets its timer
    key(&mut session, "s");
    let slideshow = session.deadline();
    assert!(slideshow.is_some());
    key(&mut session, "x");
    assert_eq!(session.deadline(), None, "the edit has no message up");
    // a resize goes to the edit alone, and to the viewer once it ends
    let window = Size {
        width: 512.0,
        height: 384.0,
    };
    assert_eq!(
        session.resize(window, 1.0),
        [Request::Redraw { bar_only: false }]
    );
    assert_eq!(session.viewer().viewport().zoom(), 0.4);
    // Control-c's message holds the edit's deadline
    ctrl(&mut session, "c");
    assert!(session.deadline().is_some_and(|at| Some(at) != slideshow));
    key(&mut session, "q");
    assert_eq!(session.viewer().viewport().zoom(), 0.8);
    // the slideshow stopped as the edit began, and stays stopped
    assert_eq!(
        session.deadline(),
        None,
        "the slideshow went on after the edit"
    );
}

/// The physical memory, as the session reads it.
fn memory() -> u64 {
    #[cfg(target_os = "linux")]
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            let line = text.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
            let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
            Some(kib * 1024)
        });
    #[cfg(target_os = "macos")]
    let memory = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok()?.trim().parse().ok());
    memory.unwrap()
}

#[test]
fn an_image_too_large_to_edit_warns_and_beeps() {
    // a cap of 8 MB, which decodes the photo and edits nothing that large
    let config = Config {
        max_image_percent: 8e6 / memory() as f64 * 100.0,
        ..Config::default()
    };
    let (mut session, _wakes) = open(&photo(), &config, None);
    let requests = key(&mut session, "x");
    assert!(!session.editing());
    let [Request::Warning(warning), Request::Beep] = &requests[..] else {
        panic!("{requests:?}");
    };
    assert!(
        warning.starts_with("editing this image needs 10 MB, more than the image cap of"),
        "{warning}"
    );
}

#[test]
fn a_text_field_takes_the_words_and_a_press_while_it_is_open_ends_it_first() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    key(&mut session, "x");
    key(&mut session, "t");
    let requests = stroke(&mut session, p(40.0, 100.0), p(40.0, 100.0), 0);
    let Some(Request::EditText {
        rect,
        text,
        font,
        color,
    }) = requests.last()
    else {
        panic!("{requests:?}");
    };
    // the field: from the baseline up, the font at the fit's scale but 11 at least; the fit is
    // of the canvas above the edit's status line
    let fit = (f64::from(WIDTH) / 640.0).min((f64::from(HEIGHT) - session.bar()) / 480.0);
    let field = Rect {
        x: 40.0,
        y: 104.0 - (28.0 * fit + 12.0),
        width: 196.0f64.max(220.0),
        height: 28.0 * fit + 12.0,
    };
    let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(
        near(rect.x, field.x)
            && near(rect.y, field.y)
            && near(rect.width, field.width)
            && near(rect.height, field.height),
        "{rect:?} where {field:?}"
    );
    assert_eq!(
        (text.as_str(), *font, *color),
        ("", (28.0 * fit).max(11.0), [255, 56, 60])
    );
    // a press elsewhere ends the field first, then starts a second text, which asks for its
    // words as it comes up
    assert_eq!(
        session.press(Button::Left, p(40.0, 160.0), 1000),
        [Request::EndText]
    );
    session.text_entered("Hi".to_owned());
    let requests = session.release(Button::Left, p(40.0, 160.0));
    assert!(
        matches!(requests.last(), Some(Request::EditText { rect, .. }) if rect.y > 140.0),
        "{requests:?}"
    );
    session.text_entered(String::new());
    let with_text = frame(&mut session);
    key(&mut session, "u");
    assert_ne!(frame(&mut session), with_text, "the first text shows");
}

/// A session in a frontend without a field, in an edit, a text begun at 40, 100.
fn typing() -> Session {
    let (mut session, _wakes) = open_with(&photo(), &Config::default(), None, false);
    key(&mut session, "x");
    key(&mut session, "t");
    let requests = stroke(&mut session, p(40.0, 100.0), p(40.0, 100.0), 0);
    assert!(
        !requests
            .iter()
            .any(|r| matches!(r, Request::EditText { .. })),
        "{requests:?}"
    );
    assert!(session.typing());
    session
}

/// The frame of a text of `words` at 40, 100, through a frontend's own field.
fn with_field(words: &str) -> Vec<u32> {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    key(&mut session, "x");
    key(&mut session, "t");
    stroke(&mut session, p(40.0, 100.0), p(40.0, 100.0), 0);
    session.text_entered(words.to_owned());
    frame(&mut session)
}

#[test]
fn without_a_field_the_sessions_line_takes_the_words() {
    let mut session = typing();
    let blank = frame(&mut session);
    for name in ["H", "x", "BackSpace", "i", "Left", "é", "End", " "] {
        assert_eq!(
            key(&mut session, name),
            [Request::Redraw { bar_only: false }]
        );
    }
    // the line shows the words; keys with Control do nothing in it
    let shown = frame(&mut session);
    assert_ne!(shown, blank);
    assert_eq!(ctrl(&mut session, "z"), []);
    assert_eq!(frame(&mut session), shown);
    // Return ends it with its words, as a field's would
    key(&mut session, "Return");
    assert!(!session.typing());
    assert!(frame(&mut session) == with_field("Héi "));
    // and the edit has the keys again
    ctrl(&mut session, "z");
    assert!(frame(&mut session) != with_field("Héi "));
}

#[test]
fn escape_keeps_the_lines_words_and_a_line_left_empty_takes_its_text_away() {
    let mut session = typing();
    key(&mut session, "o");
    key(&mut session, "Escape");
    assert!(!session.typing() && session.editing());
    assert!(frame(&mut session) == with_field("o"));
    let mut session = typing();
    key(&mut session, "Return");
    assert!(frame(&mut session) == with_field(""));
}

#[test]
fn a_press_or_a_resize_ends_the_line_first() {
    let mut session = typing();
    key(&mut session, "a");
    // a press elsewhere ends it with its words, then begins a second text, in a line again
    session.press(Button::Left, p(40.0, 160.0), 1000);
    session.release(Button::Left, p(40.0, 160.0));
    assert!(session.typing());
    key(&mut session, "Escape");
    let mut fielded = {
        let (mut session, _wakes) = open(&photo(), &Config::default(), None);
        key(&mut session, "x");
        key(&mut session, "t");
        stroke(&mut session, p(40.0, 100.0), p(40.0, 100.0), 0);
        session.text_entered("a".to_owned());
        session
    };
    assert!(frame(&mut session) == frame(&mut fielded));
    // a resize ends it too, and the edit takes the new size
    let mut session = typing();
    key(&mut session, "a");
    let window = Size {
        width: 512.0,
        height: 384.0,
    };
    session.resize(window, 1.0);
    assert!(!session.typing());
    fielded.resize(window, 1.0);
    assert!(frame(&mut session) == frame(&mut fielded));
}

#[test]
fn a_resize_ends_an_open_text_field_before_the_edit_takes_the_new_size() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    key(&mut session, "x");
    key(&mut session, "t");
    let requests = stroke(&mut session, p(40.0, 100.0), p(40.0, 100.0), 0);
    assert!(matches!(requests.last(), Some(Request::EditText { .. })));
    let fitted = frame(&mut session);
    // the field ends, as Return would, once, and the image stays under it meanwhile
    let window = Size {
        width: 512.0,
        height: 384.0,
    };
    assert_eq!(session.resize(window, 1.0), [Request::EndText]);
    assert_eq!(session.resize(window, 1.0), []);
    assert_eq!(frame(&mut session), fitted);
    // its text is in, and the edit takes the new size
    let requests = session.text_entered("Hi".to_owned());
    assert!(requests.contains(&Request::Redraw { bar_only: false }));
    let refitted = frame(&mut session);
    assert_ne!(refitted, fitted);
    key(&mut session, "u");
    assert_ne!(frame(&mut session), refitted, "the text shows");
    // without a field open, a resize goes to the edit at once
    let window = Size {
        width: f64::from(WIDTH),
        height: f64::from(HEIGHT),
    };
    assert_eq!(
        session.resize(window, 1.0),
        [Request::Redraw { bar_only: false }]
    );
}

#[test]
fn a_double_click_is_the_systems_count_where_it_gives_one_else_the_sessions_timing() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    key(&mut session, "x");
    key(&mut session, "t");
    stroke(&mut session, p(40.0, 100.0), p(40.0, 100.0), 0);
    session.text_entered("Hi".to_owned());
    key(&mut session, "v");
    // a double click on the text opens its field: the second press, by the count
    let field = |requests: &[Request]| {
        requests
            .iter()
            .any(|r| matches!(r, Request::EditText { .. }))
    };
    let double = |session: &mut Session, times: [u32; 2], clicks: [Option<u32>; 2]| {
        let mut requests = Vec::new();
        for (time, count) in times.into_iter().zip(clicks) {
            requests = session.press_counted(Button::Left, p(42.0, 95.0), time, count);
            session.release(Button::Left, p(42.0, 95.0));
        }
        let opened = field(&requests);
        if opened {
            session.text_entered("Hi".to_owned());
        }
        opened
    };
    // the system's count, whatever the time between
    assert!(double(&mut session, [1000, 3000], [Some(1), Some(2)]));
    assert!(!double(&mut session, [5000, 5100], [Some(1), Some(1)]));
    // the session's own timing where the system gives none
    assert!(double(&mut session, [7000, 7100], [None, None]));
    assert!(!double(&mut session, [9000, 9500], [None, None]));
}

#[test]
fn an_edit_shows_its_status_line_with_the_bar_hidden_and_leaving_hides_it_again() {
    // an image the canvas shows at its size, as the viewer and the edit draw it alike
    let (mut session, _wakes) = open(&testdata("fixtures/moving.gif"), &Config::default(), None);
    let view = |session: &Session| session.viewer().viewport().view_size().height;
    assert_eq!((session.bar(), view(&session)), (0.0, f64::from(HEIGHT)));
    // the bar shown, the frame as the edit will lay out the image, then hidden again
    key(&mut session, "b");
    let rows = (WIDTH * HEIGHT - WIDTH * session.bar_rows(HEIGHT, 1.0)) as usize;
    let with_bar = frame(&mut session);
    key(&mut session, "b");
    // an edit takes the canvas above its status line from the start
    key(&mut session, "x");
    assert!(session.editing() && session.bar() > 0.0);
    let edit = frame(&mut session);
    assert!(edit[..rows] == with_bar[..rows]);
    // the status line in the footer, whose width ] and [ change
    key(&mut session, "]");
    assert!(frame(&mut session)[rows..] != edit[rows..]);
    key(&mut session, "[");
    assert!(frame(&mut session)[rows..] == edit[rows..]);
    key(&mut session, "q");
    assert!(!session.editing());
    assert_eq!((session.bar(), view(&session)), (0.0, f64::from(HEIGHT)));
}

#[test]
fn an_edit_with_the_bar_shown_leaves_it_shown() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    key(&mut session, "b");
    let bar = session.bar();
    key(&mut session, "x");
    assert!(session.editing());
    assert_eq!(session.bar(), bar);
    key(&mut session, "q");
    assert!(!session.editing() && session.viewer().bar_visible());
    assert_eq!(session.bar(), bar);
}

#[test]
fn an_animations_edit_is_of_the_frame_drawn_last() {
    let (mut session, wakes) = open(&testdata("fixtures/moving.gif"), &Config::default(), None);
    while !session.viewer().image().is_some_and(|image| image.complete) {
        wait(&wakes);
        session.answers();
    }
    // the bar shown, so the viewer's image and the edit's lie over the same canvas
    key(&mut session, "b");
    let shown = frame(&mut session);
    // the animation's timer moves to the next frame, and x comes before the frame that shows it
    let due = session.deadline().expect("the animation's timer");
    session.timers(due);
    assert_eq!(session.viewer().frame_index(), 1);
    key(&mut session, "x");
    assert!(session.editing());
    // the image's rows, above the bar, whose text is the edit's now
    let rows = (WIDTH * HEIGHT - WIDTH * session.bar_rows(HEIGHT, 1.0)) as usize;
    assert!(frame(&mut session)[..rows] == shown[..rows]);
}

#[test]
fn no_edit_starts_while_the_current_file_loads_again() {
    let (mut session, wakes) = open(&photo(), &Config::default(), None);
    // r loads the file again, and the old pixels stay on screen meanwhile
    key(&mut session, "r");
    assert_eq!(key(&mut session, "x"), [Request::Beep]);
    assert!(!session.editing());
    while session.viewer().awaited().is_some() {
        wait(&wakes);
        session.answers();
    }
    key(&mut session, "x");
    assert!(session.editing());
}

#[test]
fn a_drag_held_across_x_pans_nothing_after_the_edit() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    // zoomed in, so a drag pans
    key(&mut session, "+");
    key(&mut session, "+");
    let pan = |session: &Session| session.viewer().viewport().image_rect();
    session.press(Button::Left, p(100.0, 100.0), 0);
    session.pointer_moved(p(120.0, 100.0));
    let before = pan(&session);
    key(&mut session, "x");
    key(&mut session, "q");
    session.pointer_moved(p(180.0, 100.0));
    assert_eq!(pan(&session), before);
}

#[test]
fn a_save_writes_the_edit_beside_its_file_and_shows_it_next() {
    let dir = scratch("save");
    let source = dir.join("photo.jpg");
    std::fs::copy(photo(), &source).unwrap();
    let (mut session, wakes) = open(&source, &Config::default(), None);
    key(&mut session, "x");
    for name in ["r", "f", "f"] {
        key(&mut session, name);
    }
    stroke(&mut session, p(50.0, 40.0), p(150.0, 120.0), 0);
    let requests = key(&mut session, "w");
    assert!(!session.editing(), "{requests:?}");
    let saved = dir.join("photo-edit.png");
    // its pixels said to be sRGB
    let bytes = std::fs::read(&saved).unwrap();
    assert!(bytes.windows(4).any(|w| w == b"sRGB"));
    let image = image::open(&saved).unwrap().to_rgba8();
    assert_eq!(image.dimensions(), (640, 480));
    // the rectangle, at the image's own size
    assert_eq!(image.get_pixel(250, 200).0, [255, 56, 60, 255]);
    let entry = session.viewer().current_entry().unwrap();
    assert_eq!(entry.path, saved);
    // a second save takes the next name
    while session.viewer().image().is_none() {
        wait(&wakes);
        session.answers();
    }
    key(&mut session, "x");
    ctrl(&mut session, "s");
    assert!(dir.join("photo-edit-edit.png").exists());
}

#[test]
fn a_save_that_fails_warns_beeps_and_stays_in_the_edit() {
    // a name of 254 bytes, whose edit's name is longer than a file's name may be
    let dir = scratch("save-fails");
    let stem = "p".repeat(250);
    let source = dir.join(format!("{stem}.jpg"));
    std::fs::copy(photo(), &source).unwrap();
    let (mut session, _wakes) = open(&source, &Config::default(), None);
    key(&mut session, "x");
    let requests = key(&mut session, "w");
    let [Request::Warning(warning), Request::Beep] = &requests[..] else {
        panic!("{requests:?}");
    };
    let path = dir.join(format!("{stem}-edit.png")).display().to_string();
    assert!(warning.starts_with(&path), "{warning}");
    assert!(session.editing());
}

#[test]
fn a_copy_puts_the_edits_image_on_the_pasteboard_as_png() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    key(&mut session, "x");
    let requests = ctrl(&mut session, "c");
    let Some(Request::CopyImage(png)) = requests.first() else {
        panic!("{requests:?}");
    };
    let image = image::load_from_memory(png).unwrap();
    assert_eq!((image.width(), image.height()), (640, 480));
    assert!(session.editing());
}

/// A recognizer whose text comes once the test lets it: each request gives the test a gate of
/// its own, which it opens to have that one answered.
struct Late {
    asked: Mutex<Sender<Sender<()>>>,
}

impl Recognizer for Late {
    fn text(&self, image: &RgbaImage) -> Option<(String, Vec<Word>)> {
        let (open, gate) = mpsc::channel();
        let _ = self.asked.lock().unwrap().send(open);
        gate.recv().ok()?;
        Some((format!("{}x{}", image.width, image.height), Vec::new()))
    }
    fn barcodes(&self, _: &RgbaImage) -> Vec<String> {
        let (open, gate) = mpsc::channel();
        let _ = self.asked.lock().unwrap().send(open);
        match gate.recv() {
            Ok(()) => vec!["code".to_owned()],
            Err(_) => Vec::new(),
        }
    }
    fn faces(&self, _: &RgbaImage) -> Vec<Rect> {
        Vec::new()
    }
    fn foreground(&self, _: &RgbaImage) -> Option<Vec<u8>> {
        None
    }
}

/// A slow recognizer, and the gates of the requests it takes, one by one as they come.
fn late() -> (Arc<dyn Recognizer>, Receiver<Sender<()>>) {
    let (asked, gates) = mpsc::channel();
    let late = Late {
        asked: Mutex::new(asked),
    };
    (Arc::new(late), gates)
}

/// The gate of the next request the recognizer takes, within two seconds.
fn next_gate(gates: &Receiver<Sender<()>>) -> Option<Sender<()>> {
    gates.recv_timeout(Duration::from_secs(2)).ok()
}

#[test]
fn a_recognition_answers_late_through_the_wake_for_its_own_edit_alone() {
    let (recognizer, gates) = late();
    let (mut session, wakes) = open(&photo(), &Config::default(), Some(recognizer));
    key(&mut session, "x");
    assert_eq!(key(&mut session, "o"), []);
    let first = next_gate(&gates).unwrap();
    // the edit ends, and the next asks for its own under the same job's number, which waits
    // for the one running
    key(&mut session, "q");
    key(&mut session, "x");
    key(&mut session, "o");
    // the first edit's answer changes nothing
    first.send(()).unwrap();
    let requests = answers(&mut session, &wakes, |r| matches!(r, Request::CopyText(_)));
    assert!(
        !requests.iter().any(|r| matches!(r, Request::CopyText(_))),
        "{requests:?}"
    );
    // the second edit's comes through the wake to the pasteboard
    let second = next_gate(&gates).unwrap();
    second.send(()).unwrap();
    let copied = Request::CopyText("640x480".to_owned());
    let requests = answers(&mut session, &wakes, |r| *r == copied);
    assert!(requests.contains(&copied), "{requests:?}");
}

#[test]
fn one_recognition_runs_at_a_time_and_each_kind_asked_waits_its_turn_once() {
    let (recognizer, gates) = late();
    let (mut session, wakes) = open(&photo(), &Config::default(), Some(recognizer));
    key(&mut session, "x");
    // o three times, then Q: one o runs, one o and the Q wait, and the third o adds nothing
    for _ in 0..3 {
        key(&mut session, "o");
    }
    key(&mut session, "Q");
    let first = next_gate(&gates).unwrap();
    assert!(next_gate(&gates).is_none(), "a second ran beside the first");
    // each answer comes in its turn, and the next one runs once it is in
    let text = Request::CopyText("640x480".to_owned());
    let codes = Request::CopyText("code".to_owned());
    let mut answered = |gate: Sender<()>, copied: &Request| {
        gate.send(()).unwrap();
        let requests = answers(&mut session, &wakes, |r| r == copied);
        assert!(requests.contains(copied), "{requests:?}");
    };
    answered(first, &text);
    answered(next_gate(&gates).unwrap(), &text);
    answered(next_gate(&gates).unwrap(), &codes);
    assert!(next_gate(&gates).is_none(), "the third o was asked again");
}

#[test]
fn leaving_an_edit_drops_the_recognitions_that_wait() {
    let (recognizer, gates) = late();
    let (mut session, wakes) = open(&photo(), &Config::default(), Some(recognizer));
    key(&mut session, "x");
    key(&mut session, "o");
    key(&mut session, "Q");
    let running = next_gate(&gates).unwrap();
    // the edit ends, and another begins before the first's answer is in
    key(&mut session, "q");
    key(&mut session, "x");
    running.send(()).unwrap();
    let requests = answers(&mut session, &wakes, |r| matches!(r, Request::CopyText(_)));
    assert!(
        !requests.iter().any(|r| matches!(r, Request::CopyText(_))),
        "{requests:?}"
    );
    assert!(
        next_gate(&gates).is_none(),
        "the waiting Q ran for an edit gone"
    );
}

/// A recognizer whose first call panics, and whose next ones read "words".
struct PanicsOnce(AtomicBool);

impl Recognizer for PanicsOnce {
    fn text(&self, _: &RgbaImage) -> Option<(String, Vec<Word>)> {
        assert!(
            self.0.swap(true, Ordering::SeqCst),
            "the recognizer's first call fails"
        );
        Some(("words".to_owned(), Vec::new()))
    }
    fn barcodes(&self, _: &RgbaImage) -> Vec<String> {
        Vec::new()
    }
    fn faces(&self, _: &RgbaImage) -> Vec<Rect> {
        Vec::new()
    }
    fn foreground(&self, _: &RgbaImage) -> Option<Vec<u8>> {
        None
    }
}

#[test]
fn a_recognizer_that_panics_answers_unavailable_and_the_next_recognition_runs() {
    let recognizer = Arc::new(PanicsOnce(AtomicBool::new(false)));
    let (mut session, wakes) = open(&photo(), &Config::default(), Some(recognizer));
    key(&mut session, "x");
    key(&mut session, "o");
    let requests = answers(&mut session, &wakes, |r| *r == Request::Beep);
    assert!(requests.contains(&Request::Beep), "{requests:?}");
    key(&mut session, "o");
    let copied = Request::CopyText("words".to_owned());
    let requests = answers(&mut session, &wakes, |r| *r == copied);
    assert!(requests.contains(&copied), "{requests:?}");
}

#[test]
fn without_a_recognizer_a_recognition_beeps_at_once() {
    let (mut session, _wakes) = open(&photo(), &Config::default(), None);
    key(&mut session, "x");
    let requests = key(&mut session, "Q");
    assert!(requests.contains(&Request::Beep), "{requests:?}");
}
