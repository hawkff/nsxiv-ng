//! Edit mode: an edit of the image as shown, which takes the keys, the pointer, resizes and
//! timers while it lasts, and whose answers become the frontend's requests. A frontend that
//! can edit says so through [`Session::can_edit`], with what only it knows.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use nsxiv_ng_core::viewer::{Effect, Keystroke};
use nsxiv_ng_core::{Point, Size};
use nsxiv_ng_edit::{
    Edit, EditError, Recognition, Recognizer, Response, Rgb, RgbaImage, recognize, save_path,
};
use nsxiv_ng_render::image_view;
use nsxiv_ng_render::text::MapFont;
use nsxiv_ng_render::{Canvas, bar};

use crate::line::{Line, Typed};
use crate::{Answer, Request, Session, one_redraw};

/// What a frontend on which edit mode cannot start says when its key is pressed.
pub(crate) const NO_EDIT_MODE: &str = "this frontend has no edit mode yet";

/// How many names [`save_path`] gives a save, where another file takes each before it is
/// created.
const SAVE_TRIES: usize = 16;

/// The longest wait between the two presses of a double click, in milliseconds, as the
/// pointer's own.
const DOUBLE_CLICK: u32 = 400;

/// How far apart, in logical units, the two presses of a double click may be.
const DOUBLE_CLICK_SLOP: f64 = 4.0;

/// What a frontend that can edit gives the session: what only it knows.
pub struct EditSetup {
    /// The families of annotation text, in order, the first installed serving.
    pub families: Vec<String>,
    /// It shows a text field of its own for an annotation's words, through
    /// [`Request::EditText`].
    pub field: bool,
    /// What recognizes text, barcodes, faces and the foreground, where the system has one.
    pub recognizer: Option<Arc<dyn Recognizer>>,
}

/// A recognition an edit asked for: the edit's number, the job, what, and of which image.
#[derive(Clone, Copy)]
pub(crate) struct Asked {
    number: u64,
    job: u64,
    what: nsxiv_ng_edit::Recognize,
    flattened: bool,
}

/// The edit under way.
pub(crate) struct Editing {
    pub(crate) edit: Edit,
    /// The edit's number, which a recognition's answer names.
    number: u64,
    /// The file the edit is of.
    source: PathBuf,
    /// Answers that flatten the image, waiting for the fonts, in order.
    waiting: VecDeque<Response>,
    /// The frontend's text field is open.
    field: bool,
    /// A press that ended the field, which goes on once the field's text is in.
    press: Option<(Point, u32, Option<u32>)>,
    /// The time and place of the last press, which a second near it soon after makes a double
    /// click.
    last_press: Option<(u32, Point)>,
    /// The left button went down in the edit, and has not come up.
    down: bool,
    /// The canvas changed size while the text field was open, which the edit takes once the
    /// field's text is in.
    resized: bool,
    /// The session's own text line, open, for a frontend without a field.
    line: Option<Line>,
}

impl Session {
    /// The frontend can edit: `x` enters edit mode, with what `setup` gives.
    pub fn can_edit(&mut self, setup: EditSetup) {
        self.edit_setup = Some(setup);
    }

    /// Annotation text's faces from the bytes `map` gives for a font file, a map of the file
    /// rather than a copy, where it gives them.
    pub fn map_fonts(&mut self, map: MapFont) {
        self.text.map_fonts(map);
    }

    /// Whether an edit is under way, which takes the keys.
    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Whether the session's own text line is open, for a frontend without a field: it takes
    /// the keys through [`Session::key`], a key named by one character typing that character.
    /// A frontend passes a key that types a character as that character, then, where the key's
    /// name is another, as X11 names é `eacute`.
    pub fn typing(&self) -> bool {
        self.editing.as_ref().is_some_and(|e| e.line.is_some())
    }

    // Ends the session's text line with its words, as Return does.
    fn end_line(&mut self) -> Vec<Request> {
        match self.editing.as_mut().and_then(|e| e.line.take()) {
            Some(line) => self.text_entered(line.into_text()),
            None => Vec::new(),
        }
    }

    /// Ends the edit without saving, as the viewer's quitting drops it.
    pub fn end_edit(&mut self) -> Vec<Request> {
        match self.editing.take() {
            Some(_) => self.left_edit(),
            None => Vec::new(),
        }
    }

    /// The text field [`Request::EditText`] opened ended with `text`, by Return or Escape, or
    /// for a [`Request::EndText`]. A press that ended it goes on then.
    pub fn text_entered(&mut self, text: String) -> Vec<Request> {
        let canvas = self.canvas();
        let Some(editing) = &mut self.editing else {
            return Vec::new();
        };
        editing.field = false;
        let press = editing.press.take();
        let responses = editing.edit.text_entered(text);
        let resized = std::mem::take(&mut editing.resized);
        if resized {
            editing.edit.resize(canvas);
        }
        let mut requests = self.edit_responses(responses);
        if resized {
            requests.push(Request::Redraw { bar_only: false });
        }
        if let Some((at, time, clicks)) = press {
            requests.extend(self.edit_press(at, time, clicks));
        }
        one_redraw(requests)
    }

    // The canvas changed size from `before` in an edit, which takes the new size; an open text
    // field, which the image would refit under, ends first, as Return would, and the edit takes
    // the size once the field's text is in.
    pub(crate) fn edit_resized(&mut self, before: Size) -> Vec<Request> {
        let canvas = self.canvas();
        // the session's own line ends at once
        if self.typing() && canvas != before {
            let mut requests = self.end_line();
            requests.extend(self.edit_resized(before));
            return one_redraw(requests);
        }
        let Some(editing) = &mut self.editing else {
            return Vec::new();
        };
        if editing.field && (editing.resized || canvas != before) {
            let first = !std::mem::replace(&mut editing.resized, true);
            return if first {
                vec![Request::EndText]
            } else {
                Vec::new()
            };
        }
        editing.edit.resize(canvas);
        vec![Request::Redraw { bar_only: false }]
    }

    /// A key came up: space's, while editing, stops moving a shape being drawn.
    pub fn key_released(&mut self, key: &Keystroke) -> Vec<Request> {
        if let Some(editing) = &mut self.editing
            && key.key == " "
        {
            editing.edit.space(false);
        }
        Vec::new()
    }

    // Effect::EnterEditMode: an edit of the image as shown, where the frontend can edit.
    pub(crate) fn enter_edit(&mut self, requests: &mut Vec<Request>) -> bool {
        let Some(setup) = &self.edit_setup else {
            if !self.quiet {
                requests.push(Request::Warning(NO_EDIT_MODE.to_owned()));
            }
            requests.push(Request::Beep);
            return false;
        };
        // the image shown is the current file's only once no load of it is under way: a
        // reload keeps the old pixels on screen meanwhile
        let loading = self.viewer.awaited().is_some();
        let (Some(source), Some(pyramid), false) =
            (self.viewer.current_entry(), &self.shown, loading)
        else {
            requests.push(Request::Beep);
            return false;
        };
        let source = source.path.clone();
        let (still, opaque) = (&pyramid.levels[0], pyramid.opaque);
        // an animation's frame drawn last, which is on screen whatever frame a timer has moved
        // to since; else the still's full image
        let frame = self.drawn.as_deref().filter(|frame| frame.width > 0);
        let (width, height, rgba, opaque) = match frame {
            Some(frame) => (frame.width, frame.height, &frame.rgba, false),
            None => (still.width, still.height, &still.rgba, opaque),
        };
        let levels = [&rgba[..]];
        let image = image_view::Image {
            width,
            height,
            levels: &levels,
            opaque,
        };
        let families = setup.families.clone();
        let made = Edit::new(
            &image,
            self.turn,
            self.colors.as_ref(),
            self.max_image_bytes,
            source.clone(),
            families,
        );
        match made {
            Ok(edit) => {
                // a drag of the viewer's held across x ends, so the pointer pans nothing after
                self.pointer = Default::default();
                self.edits += 1;
                self.editing = Some(Editing {
                    edit,
                    number: self.edits,
                    source,
                    waiting: VecDeque::new(),
                    field: false,
                    press: None,
                    last_press: None,
                    down: false,
                    resized: false,
                    line: None,
                });
                // the canvas with the edit's status line in place, which shows whether or not
                // the viewer's bar does
                let canvas = self.canvas();
                if let Some(editing) = &mut self.editing {
                    editing.edit.resize(canvas);
                }
                true
            }
            Err(why) => {
                if !self.quiet && matches!(why, EditError::TooLarge { .. }) {
                    requests.push(Request::Warning(why.to_string()));
                }
                requests.push(Request::Beep);
                false
            }
        }
    }

    // The edit is gone: the viewer catches up with the canvas, which resizes changed while it
    // took none of them, and its timers held meanwhile fire as they come due.
    fn left_edit(&mut self) -> Vec<Request> {
        // the recognitions that waited for one running are for an edit gone
        self.next_recognition.clear();
        let mut requests = self.carry(vec![Effect::Layout]);
        requests.push(Request::Redraw { bar_only: false });
        one_redraw(requests)
    }

    // A press of the left button in the edit, the `clicks` the frontend's system counted where
    // it counts them; one while the text field is open ends the field first, as Return would,
    // and goes on once its text is in.
    pub(crate) fn edit_press(&mut self, at: Point, time: u32, clicks: Option<u32>) -> Vec<Request> {
        // the session's own line ends at once, and the press goes on
        if self.typing() {
            let mut requests = self.end_line();
            requests.extend(self.edit_press(at, time, clicks));
            return one_redraw(requests);
        }
        let Some(editing) = &mut self.editing else {
            return Vec::new();
        };
        if editing.field {
            editing.press = Some((at, time, clicks));
            return vec![Request::EndText];
        }
        // the system's count where it gives one, as with the user's own double-click speed,
        // else the session's own timing
        let double = match clicks {
            Some(clicks) => clicks >= 2,
            None => editing.last_press.is_some_and(|(last, from)| {
                time.wrapping_sub(last) < DOUBLE_CLICK
                    && (at.x - from.x).hypot(at.y - from.y) <= DOUBLE_CLICK_SLOP
            }),
        };
        // of the session's own, a third press starts again
        editing.last_press = if double { None } else { Some((time, at)) };
        editing.down = true;
        let clicks = if double { 2 } else { 1 };
        let responses = editing.edit.press(at, clicks, &mut self.text);
        self.edit_responses(responses)
    }

    pub(crate) fn edit_drag(&mut self, at: Point) -> Vec<Request> {
        let Some(editing) = self.editing.as_mut().filter(|e| e.down) else {
            return Vec::new();
        };
        let responses = editing.edit.drag(at);
        self.edit_responses(responses)
    }

    pub(crate) fn edit_release(&mut self, at: Point) -> Vec<Request> {
        let Some(editing) = self.editing.as_mut().filter(|e| e.down) else {
            return Vec::new();
        };
        editing.down = false;
        let responses = editing.edit.release(at);
        self.edit_responses(responses)
    }

    pub(crate) fn edit_key(&mut self, key: &Keystroke) -> Option<Vec<Request>> {
        let editing = self.editing.as_mut()?;
        // the session's own line takes every key while it is open
        if let Some(line) = &mut editing.line {
            return Some(match line.key(key) {
                Typed::Changed => vec![Request::Redraw { bar_only: false }],
                Typed::Ended(text) => {
                    editing.line = None;
                    self.text_entered(text)
                }
                Typed::Nothing => Vec::new(),
            });
        }
        let responses = editing.edit.key(key)?;
        Some(one_redraw(self.edit_responses(responses)))
    }

    pub(crate) fn edit_timers(&mut self, now: Instant) -> Vec<Request> {
        let Some(editing) = &mut self.editing else {
            return Vec::new();
        };
        let responses = editing.edit.timers(now);
        self.edit_responses(responses)
    }

    // A recognition's answer, for the edit it names; one for an edit that ended, or for
    // another, changes nothing.
    pub(crate) fn recognized(
        &mut self,
        number: u64,
        job: u64,
        answer: Recognition,
    ) -> Vec<Request> {
        self.recognizing = false;
        let mut requests = match self.editing.as_mut().filter(|e| e.number == number) {
            Some(editing) => {
                let responses = editing.edit.recognized(job, answer);
                self.edit_responses(responses)
            }
            None => Vec::new(),
        };
        // those asked for meanwhile, in turn, where their edit is still under way: one that
        // runs, and those that answered at once, as without a recognizer
        while !self.recognizing {
            let Some(asked) = self.next_recognition.pop_front() else {
                break;
            };
            requests.extend(self.recognize(asked));
        }
        requests
    }

    // The answers that waited for the fonts, once they are in.
    pub(crate) fn edit_fonts_in(&mut self) -> Vec<Request> {
        if !self.editing.as_ref().is_some_and(|e| !e.waiting.is_empty()) || !self.fonts_in() {
            return Vec::new();
        }
        let waiting = self
            .editing
            .as_mut()
            .map(|e| std::mem::take(&mut e.waiting));
        let responses = waiting.unwrap_or_default().into_iter().collect();
        self.edit_responses(responses)
    }

    // The fonts have loaded, whether or not one serves, so text can be drawn as it will be.
    fn fonts_in(&mut self) -> bool {
        #[cfg(test)]
        if self.hold_fonts {
            return false;
        }
        !self.text.loading()
    }

    // The edit's answers as the frontend's requests, in order.
    fn edit_responses(&mut self, responses: Vec<Response>) -> Vec<Request> {
        let mut requests = Vec::new();
        let mut queue = VecDeque::from(responses);
        while let Some(response) = queue.pop_front() {
            let field = self.edit_setup.as_ref().is_some_and(|setup| setup.field);
            let fonts = self.fonts_in();
            let Some(editing) = &mut self.editing else {
                break;
            };
            let flattens = matches!(
                response,
                Response::CopyImage
                    | Response::Leave { save: true }
                    | Response::Recognize {
                        flattened: true,
                        ..
                    }
            );
            // nothing is flattened without the fonts its text is drawn in, nor ahead of what
            // waits for them already
            if flattens && (!editing.waiting.is_empty() || !fonts) {
                editing.waiting.push_back(response);
                continue;
            }
            match response {
                Response::Redraw => requests.push(Request::Redraw { bar_only: false }),
                Response::Bar => requests.push(Request::Redraw { bar_only: true }),
                Response::Beep => requests.push(Request::Beep),
                Response::EditText {
                    rect,
                    text,
                    font,
                    color: Rgb(r, g, b),
                } if field => {
                    editing.field = true;
                    requests.push(Request::EditText {
                        rect,
                        text,
                        font,
                        color: [r, g, b],
                    });
                }
                // a frontend without a field of its own gets the session's line
                Response::EditText {
                    rect,
                    text,
                    color: Rgb(r, g, b),
                    ..
                } => {
                    editing.line = Some(Line::new(rect, text, [r, g, b]));
                    requests.push(Request::Redraw { bar_only: false });
                }
                Response::CopyText(text) => requests.push(Request::CopyText(text)),
                Response::CopyImage => {
                    let image = editing.edit.flattened(&mut self.text);
                    match png(&image) {
                        Ok(png) => requests.push(Request::CopyImage(png)),
                        Err(why) => {
                            requests.push(Request::Warning(format!("cannot copy the edit: {why}")));
                            requests.push(Request::Beep);
                        }
                    }
                }
                Response::Leave { save: false } => {
                    self.editing = None;
                    requests.extend(self.left_edit());
                    break;
                }
                Response::Leave { save: true } => {
                    let saved = self.save();
                    requests.extend(self.after_save(saved));
                    // what waited after a save that failed goes on, in the edit kept
                    if self.editing.is_none() {
                        break;
                    }
                }
                Response::Recognize {
                    job,
                    what,
                    flattened,
                } => {
                    let asked = Asked {
                        number: editing.number,
                        job,
                        what,
                        flattened,
                    };
                    requests.extend(self.recognize(asked));
                }
            }
        }
        requests
    }

    // A recognition, one at a time across edits: those asked while another runs wait for it in
    // turn, each one asked answered, but one of a kind that waits already is not
    // asked again, so a key pressed again and again copies or censors twice at most. A waiting
    // one holds no image: its copy is taken when it runs. Without a recognizer the answer is
    // at once that there is none, and no image is copied.
    fn recognize(&mut self, asked: Asked) -> Vec<Request> {
        let Some(editing) = self.editing.as_mut().filter(|e| e.number == asked.number) else {
            return Vec::new();
        };
        let Some(recognizer) = self.edit_setup.as_ref().and_then(|s| s.recognizer.clone()) else {
            let responses = editing.edit.recognized(asked.job, Recognition::Unavailable);
            return self.edit_responses(responses);
        };
        if self.recognizing {
            let kind = |a: &Asked| (a.what, a.flattened);
            if !self
                .next_recognition
                .iter()
                .any(|w| kind(w) == kind(&asked))
            {
                self.next_recognition.push_back(asked);
            } else {
                // dropped, so the edit waits for it no more
                editing.edit.forget(asked.job);
            }
            return Vec::new();
        }
        // a copy of the image for the recognizer's thread, one beyond the copies Edit::new
        // counted against the image cap, which the one recognition at a time bounds
        let image = match asked.flattened {
            true => editing.edit.flattened(&mut self.text),
            false => editing.edit.base_image(),
        };
        self.recognizing = true;
        self.ask(recognizer, asked.number, asked.job, asked.what, image);
        Vec::new()
    }

    // Runs a recognition on a thread of its own, whose answer comes back through the wake.
    fn ask(
        &mut self,
        recognizer: Arc<dyn Recognizer>,
        number: u64,
        job: u64,
        what: nsxiv_ng_edit::Recognize,
        image: RgbaImage,
    ) {
        let (sender, wake) = (self.sender.clone(), Arc::clone(&self.wake));
        let answer = move |answer| {
            let _ = sender.send(Answer::Recognized {
                edit: number,
                job,
                answer,
            });
            wake();
        };
        let spawned = std::thread::Builder::new()
            .name("recognize".to_owned())
            .spawn({
                let answer = answer.clone();
                // a recognizer that unwinds answers too, so the next recognition runs
                move || {
                    let asked = AssertUnwindSafe(|| recognize(Some(&*recognizer), what, &image));
                    let found = std::panic::catch_unwind(asked).unwrap_or(Recognition::Unavailable);
                    // the copy goes before the answer lets the next recognition make its own
                    drop(image);
                    answer(found)
                }
            });
        if spawned.is_err() {
            answer(Recognition::Unavailable);
        }
    }

    // Saves the flattened image as PNG to a file created new beside the source, under the
    // first name nothing holds, asked again where another file took it meanwhile: the path,
    // or the warning where it failed.
    // shortcut: the save runs on the frontend's thread, which blocks the
    // window for the encoding and the write, half a second for a 12 MP photo; save on a
    // thread of its own, with the edit kept until it is done, if that wait shows
    fn save(&mut self) -> Option<Result<PathBuf, String>> {
        let editing = self.editing.as_mut()?;
        let image = editing.edit.flattened(&mut self.text);
        let png = match png(&image) {
            Ok(png) => png,
            Err(why) => return Some(Err(format!("cannot save the edit: {why}"))),
        };
        let mut last = None;
        for _ in 0..SAVE_TRIES {
            let path = save_path(&editing.source);
            let created = OpenOptions::new().write(true).create_new(true).open(&path);
            let mut file = match created {
                Ok(file) => file,
                Err(why) if why.kind() == io::ErrorKind::AlreadyExists => {
                    last = Some(path);
                    continue;
                }
                Err(why) => return Some(Err(format!("{}: {why}", path.display()))),
            };
            if let Err(why) = file.write_all(&png).and_then(|()| file.sync_all()) {
                // no part of an image left behind
                let _ = std::fs::remove_file(&path);
                return Some(Err(format!("{}: {why}", path.display())));
            }
            return Some(Ok(path));
        }
        let path = last.unwrap_or_default();
        Some(Err(format!("{}: every name is taken", path.display())))
    }

    // After a save: the edit ends and the viewer shows the saved file next, or it stays rather
    // than lose work, with the reason and a beep.
    fn after_save(&mut self, saved: Option<Result<PathBuf, String>>) -> Vec<Request> {
        match saved {
            Some(Ok(path)) => {
                self.editing = None;
                let mut requests = self.left_edit();
                let effects = self.viewer.insert_after_current(path);
                requests.extend(self.carry(effects));
                one_redraw(requests)
            }
            Some(Err(why)) => vec![Request::Warning(why), Request::Beep],
            None => Vec::new(),
        }
    }

    /// Draws the edit's frame above the bar, and the edit's status in the bar.
    pub(crate) fn draw_edit(&mut self, canvas: &mut Canvas, scale: f64) -> bool {
        let rows = self.bar_rows(canvas.height(), scale);
        let Some(editing) = &mut self.editing else {
            return false;
        };
        let width = canvas.width();
        canvas.pixels_mut().fill(self.background);
        let main = canvas.height() - rows;
        let [_, r, g, b] = self.grid_colors.selection.to_be_bytes();
        {
            let pixels = &mut canvas.pixels_mut()[..width as usize * main as usize];
            if let Some(mut area) = Canvas::new(pixels, width, main) {
                let background = self.background;
                editing
                    .edit
                    .draw(&mut area, scale, &mut self.text, background, Rgb(r, g, b));
                if let Some(line) = &editing.line {
                    let bar = (self.bar_background, self.foreground);
                    line.draw(&mut area, &mut self.text, scale, bar);
                }
            }
        }
        if rows > 0 {
            let (left, right) = editing.edit.status();
            bar::status(
                canvas,
                &mut self.text,
                [0, main as i32, width as i32, rows as i32],
                scale,
                (&left, &right),
                (self.bar_background, self.foreground),
            );
        }
        true
    }

    /// The edit's status in the bar alone, as [`Session::draw_bar`] draws the viewer's.
    pub(crate) fn draw_edit_bar(&mut self, canvas: &mut Canvas, scale: f64) -> bool {
        let Some(editing) = &self.editing else {
            return false;
        };
        let (width, height) = (canvas.width() as i32, canvas.height() as i32);
        let (left, right) = editing.edit.status();
        bar::status(
            canvas,
            &mut self.text,
            [0, 0, width, height],
            scale,
            (&left, &right),
            (self.bar_background, self.foreground),
        );
        true
    }
}

/// The image as PNG, 8 bits a channel with alpha, its pixels said to be sRGB, as they are.
/// Deflate at level 2: a 12 MP photo in half a second, where the default level takes five,
/// for a file some 15% larger.
fn png(image: &RgbaImage) -> Result<Vec<u8>, png::EncodingError> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_deflate_compression(png::DeflateCompression::Level(2));
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    encoder.write_header()?.write_image_data(&image.rgba)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    use nsxiv_ng_core::Size;
    use nsxiv_ng_core::config::Config;
    use nsxiv_ng_core::file_list::FileEntry;
    use nsxiv_ng_core::options::Options;

    use super::*;
    use crate::Bar;

    /// An edit of a copy of the photo named `name` in a folder of the test's own, which the
    /// fonts are in for, and the folder.
    fn edit_of(test: &str, name: &str) -> (Session, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join(name);
        let photo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/photo.jpg");
        std::fs::copy(photo, &source).unwrap();
        let (sender, wakes) = mpsc::channel();
        let wake = move || {
            let _ = sender.send(());
        };
        let files = vec![FileEntry::new(source)];
        let program = PathBuf::from("nsxiv-ng");
        let (options, config) = (Options::default(), Config::default());
        let mut session = Session::new(
            options,
            files,
            &config,
            program,
            Vec::new(),
            Bar::Drawn,
            wake,
        );
        session.can_edit(EditSetup {
            families: vec!["DejaVu Sans".to_owned()],
            field: true,
            recognizer: None,
        });
        session.resize(
            Size {
                width: 256.0,
                height: 192.0,
            },
            1.0,
        );
        session.start();
        while session.viewer().image().is_none() || session.text.loading() {
            wakes.recv_timeout(Duration::from_secs(60)).unwrap();
            session.answers();
        }
        session.key(&Keystroke::new("x"));
        assert!(session.editing());
        (session, dir)
    }

    /// A recognizer whose text waits for a gate it hands out, one a request.
    struct Gated(std::sync::Mutex<mpsc::Sender<mpsc::Sender<()>>>);

    impl Recognizer for Gated {
        fn text(&self, _: &RgbaImage) -> Option<(String, Vec<nsxiv_ng_edit::Word>)> {
            let (open, gate) = mpsc::channel();
            let _ = self.0.lock().unwrap().send(open);
            gate.recv().ok()?;
            Some(("text".to_owned(), Vec::new()))
        }
        fn barcodes(&self, _: &RgbaImage) -> Vec<String> {
            Vec::new()
        }
        fn faces(&self, _: &RgbaImage) -> Vec<nsxiv_ng_core::Rect> {
            Vec::new()
        }
        fn foreground(&self, _: &RgbaImage) -> Option<Vec<u8>> {
            None
        }
    }

    #[test]
    fn a_recognition_dropped_for_one_of_its_kind_waits_no_more() {
        let (mut session, dir) = edit_of("dropped", "photo.jpg");
        let (asked, gates) = mpsc::channel();
        session.edit_setup.as_mut().unwrap().recognizer =
            Some(Arc::new(Gated(std::sync::Mutex::new(asked))));
        // o three times: one runs, one waits, and the third is dropped
        for _ in 0..3 {
            session.key(&Keystroke::new("o"));
        }
        let copied = |session: &mut Session| {
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                let requests = session.answers();
                if requests.iter().any(|r| matches!(r, Request::CopyText(_))) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        for _ in 0..2 {
            let gate = gates.recv_timeout(Duration::from_secs(10)).unwrap();
            gate.send(()).unwrap();
            assert!(copied(&mut session), "an answer");
        }
        let waiting = session
            .editing
            .as_ref()
            .unwrap()
            .edit
            .recognitions_waiting();
        assert_eq!(waiting, 0, "jobs the edit still waits for");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nothing_is_flattened_before_the_fonts_are_in() {
        let (mut session, dir) = edit_of("fonts-in", "photo.jpg");
        // a copy and a save asked while the fonts load wait for them, in order
        session.hold_fonts = true;
        let mut requests = session.key(&Keystroke::ctrl("c")).unwrap();
        requests.extend(session.key(&Keystroke::new("w")).unwrap());
        assert!(
            !requests.iter().any(|r| matches!(r, Request::CopyImage(_))),
            "{requests:?}"
        );
        assert!(session.editing() && !dir.join("photo-edit.png").exists());
        session.hold_fonts = false;
        let requests = session.answers();
        assert!(
            matches!(requests.first(), Some(Request::CopyImage(_))),
            "{requests:?}"
        );
        assert!(!session.editing() && dir.join("photo-edit.png").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn what_waited_after_a_save_that_failed_goes_on() {
        // a name whose edit's name is longer than a file's name may be
        let name = format!("{}.jpg", "p".repeat(250));
        let (mut session, dir) = edit_of("save-fails", &name);
        session.hold_fonts = true;
        session.key(&Keystroke::new("w"));
        session.key(&Keystroke::ctrl("c"));
        session.hold_fonts = false;
        let requests = session.answers();
        let warned = requests
            .iter()
            .position(|r| matches!(r, Request::Warning(_)));
        let copied = requests
            .iter()
            .position(|r| matches!(r, Request::CopyImage(_)));
        assert!(warned.is_some() && copied > warned, "{requests:?}");
        assert!(session.editing());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
