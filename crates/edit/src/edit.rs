//! The edit session: the annotations, undo and redo, the badge counter, the keys, the status
//! line, the recognitions' answers and the pointer, over the image as the viewer showed it.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nsxiv_ng_core::viewer::Keystroke;
use nsxiv_ng_core::viewport::Viewport;
use nsxiv_ng_core::{Point, Rect, Size};
use nsxiv_ng_render::image_view::{self, Image, Turn};
use nsxiv_ng_render::text::Text;
use nsxiv_ng_render::text::tiny_skia::{IntSize, Pixmap};
use nsxiv_ng_render::{Canvas, Colors};

use crate::annotation::distance;
use crate::draw::Drawn;
use crate::{
    Annotation, CensorMode, EMOJIS, FillMode, PALETTE, Recognition, Recognize, Rgb, RgbaImage,
    Tool, find_pii,
};

/// How long a message stands in for the status line.
const FLASH: Duration = Duration::from_secs(3);

/// The copies of the image, each its size at most, that an edit holds at its worst moment,
/// a blur made while the image is flattened: the base, the pieces its censors keep (the base's
/// bytes between them), the piece being made, the blur's working values of a piece of the
/// whole image (four copies, 16 bytes a pixel) and the flattened image. Beside them the blur
/// holds a line of working values, a row or a column, 16 bytes a pixel of the longer side,
/// counted as it is: [`LINE`]. Not counted, since the canvas bounds them instead: the frame,
/// the base as fitted to the window, the pieces scaled to where they show (the frame's bytes
/// between them) and a censor's piece while it moves.
// shortcut: the blur's working values are a whole piece at a time; blur in bands of rows if
// editing very large images on machines of little memory matters
const COPIES: u64 = 8;

/// The bytes a pixel of the blur's line of working values, beside [`COPIES`].
const LINE: u64 = 16;

/// What the frontend or the session is to do for the edit, in the order given.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    /// Draw the frame again, the status line included.
    Redraw,
    /// Only the status line changed.
    Bar,
    Beep,
    /// Leave edit mode; with `save`, once the flattened image is saved, and the edit stays
    /// where it cannot be.
    Leave {
        save: bool,
    },
    /// Show a text field over `rect`, in canvas logical units, holding `text`, in bold
    /// sans-serif at `font` logical pixels in `color`; answer with [`Edit::text_entered`].
    EditText {
        rect: Rect,
        text: String,
        font: f64,
        color: Rgb,
    },
    /// Put the flattened image on the pasteboard.
    CopyImage,
    /// Put this text on the pasteboard.
    CopyText(String),
    /// Recognize `what` in the flattened image, or in the base, and answer with
    /// [`Edit::recognized`] and `job`.
    Recognize {
        job: u64,
        what: Recognize,
        flattened: bool,
    },
}

/// Why an image cannot be edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// Its copies would need `needed` bytes, more than the image cap; `u64::MAX` where the
    /// count overflows.
    TooLarge {
        needed: u64,
        cap: u64,
    },
    Empty,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let megabytes = |bytes: u64| bytes.div_ceil(1 << 20);
        match self {
            EditError::TooLarge { needed, cap } => write!(
                f,
                "editing this image needs {} MB, more than the image cap of {} MB",
                megabytes(*needed),
                megabytes(*cap)
            ),
            EditError::Empty => write!(f, "an empty image cannot be edited"),
        }
    }
}

/// What a recognition was asked for, and so what its answer becomes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Asked {
    /// `o`: the text, copied.
    Copy,
    Barcodes,
    Faces,
    /// `P`: the words that look like personal data, censored.
    Pii,
    Foreground,
}

pub struct Edit {
    /// The image as shown, premultiplied RGBA: what is annotated and saved.
    pub(crate) base: Pixmap,
    /// Counts the changes to the base, which the censor cache keys by.
    pub(crate) generation: u64,
    source: PathBuf,
    pub(crate) families: Vec<String>,
    pub(crate) annotations: Vec<Annotation>,
    redo: Vec<Annotation>,
    tool: Tool,
    palette: usize,
    line_width: f64,
    fill: FillMode,
    censor: CensorMode,
    emoji: usize,
    font_size: f64,
    badge: u32,
    pub(crate) selection: Option<u64>,
    next_id: u64,
    /// The annotation the pointer is drawing.
    pub(crate) drawing: Option<Annotation>,
    drag_start: Point,
    last_drag: Point,
    /// The selection as it was when the pointer took it, and the handle it took.
    taken: Option<Annotation>,
    handle: Option<usize>,
    space: bool,
    /// The canvas in logical units, which the image fits in.
    view: Size,
    /// The annotation whose text a frontend's field is editing.
    editing: Option<u64>,
    flash: Option<(String, Instant)>,
    job: u64,
    /// The recognitions asked for and not answered yet, each answered in its turn.
    asked: Vec<(u64, Asked)>,
    pub(crate) drawn: Drawn,
}

impl Edit {
    /// An edit of `image` as the viewer shows it, turned and flipped by `turn` and with
    /// `colors`' gamma and contrast, alpha kept; `source` names the file it came from.
    /// Annotation text is in the first installed of `families`. It refuses an image whose
    /// copies would pass `cap` bytes.
    pub fn new(
        image: &Image,
        turn: Turn,
        colors: Option<&Colors>,
        cap: u64,
        source: PathBuf,
        families: Vec<String>,
    ) -> Result<Edit, EditError> {
        let (width, height) = if turn.rotation % 180 == 90 {
            (image.height, image.width)
        } else {
            (image.width, image.height)
        };
        // a size whose bytes overflow is too large whatever the cap
        let pixels = u64::from(width).checked_mul(u64::from(height));
        let line = LINE * u64::from(width.max(height));
        let needed = (pixels.and_then(|pixels| pixels.checked_mul(COPIES * 4)))
            .and_then(|copies| copies.checked_add(line));
        let count = pixels.and_then(|pixels| usize::try_from(pixels).ok());
        let fits = needed.is_some_and(|needed| needed <= cap);
        let Some(count) = count.filter(|_| fits) else {
            let needed = needed.unwrap_or(u64::MAX);
            return Err(EditError::TooLarge { needed, cap });
        };
        let size = IntSize::from_wh(width, height).ok_or(EditError::Empty)?;
        // the viewer's own drawing turns and colors it, onto a clear canvas, which keeps alpha
        let mut pixels = vec![0; count];
        let mut canvas = Canvas::new(&mut pixels, width, height).ok_or(EditError::Empty)?;
        image_view::draw(
            &mut canvas,
            image,
            &Viewport::default(),
            turn,
            colors,
            1.0,
            true,
        );
        let rgba = (pixels.iter())
            .flat_map(|p| {
                let [b, g, r, a] = p.to_le_bytes();
                [r, g, b, a]
            })
            .collect();
        let base = Pixmap::from_vec(rgba, size).ok_or(EditError::Empty)?;
        Ok(Edit {
            base,
            generation: 0,
            source,
            families,
            annotations: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Arrow,
            palette: 0,
            line_width: 4.0,
            fill: FillMode::Stroke,
            censor: CensorMode::Pixelate,
            emoji: 0,
            font_size: 28.0,
            badge: 1,
            selection: None,
            next_id: 1,
            drawing: None,
            drag_start: Point::default(),
            last_drag: Point::default(),
            taken: None,
            handle: None,
            space: false,
            view: Size::default(),
            editing: None,
            flash: None,
            job: 0,
            asked: Vec::new(),
            drawn: Drawn::default(),
        })
    }

    pub fn annotations(&self) -> &[Annotation] {
        &self.annotations
    }

    pub fn selection(&self) -> Option<u64> {
        self.selection
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    /// The base as straight RGBA, for a recognition that reads it.
    pub fn base_image(&self) -> RgbaImage {
        RgbaImage::from_pixmap(self.base.clone())
    }

    /// The canvas the image fits in, in logical units, which the pointer's points are in. A
    /// size that is not finite changes nothing, as the viewport keeps its own.
    pub fn resize(&mut self, view: Size) {
        if view.width.is_finite() && view.height.is_finite() {
            self.view = view;
        }
    }

    /// The scale the image shows at: the window's fit, never above 1.
    pub(crate) fn fit(&self) -> f64 {
        let (w, h) = (f64::from(self.base.width()), f64::from(self.base.height()));
        let fit = (self.view.width / w).min(self.view.height / h).min(1.0);
        if fit > 0.0 { fit } else { 1.0 }
    }

    /// Where the image's top left corner shows, centered, in logical units.
    pub(crate) fn origin(&self) -> Point {
        let k = self.fit();
        Point {
            x: (self.view.width - f64::from(self.base.width()) * k) / 2.0,
            y: (self.view.height - f64::from(self.base.height()) * k) / 2.0,
        }
    }

    fn image_point(&self, at: Point) -> Point {
        let (o, k) = (self.origin(), self.fit());
        Point {
            x: (at.x - o.x) / k,
            y: (at.y - o.y) / k,
        }
    }

    fn view_point(&self, p: Point) -> Point {
        let (o, k) = (self.origin(), self.fit());
        Point {
            x: p.x * k + o.x,
            y: p.y * k + o.y,
        }
    }

    /// The annotations the pointer is drawing or dragging.
    pub(crate) fn moving(&self) -> Vec<u64> {
        let dragged = (self.taken.as_ref()).and(self.selection);
        self.drawing.iter().map(|a| a.id).chain(dragged).collect()
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selection?;
        self.annotations.iter().position(|a| a.id == id)
    }

    fn flash(&mut self, message: String) {
        self.flash = Some((message, Instant::now() + FLASH));
    }

    // MARK: the pointer

    /// The left button went down at `at`, in canvas logical units; `clicks` is 2 for a
    /// double click. Select takes a handle of the selection, else the annotation on top that
    /// `at` hits, and a double click on text or a stamp opens its text; the other tools start
    /// drawing. A point that lands nowhere finite on the image changes nothing.
    pub fn press(&mut self, at: Point, clicks: u32, text: &mut Text) -> Vec<Response> {
        let p = self.image_point(at);
        if !finite(p) {
            return Vec::new();
        }
        (self.drag_start, self.last_drag) = (p, p);
        if self.tool == Tool::Select {
            if let Some(i) = self.selected_index() {
                let slop = 12.0 / self.fit();
                let handles = self.annotations[i].handles();
                if let Some(handle) = handles.iter().position(|&h| distance(h, p) < slop) {
                    self.handle = Some(handle);
                    self.taken = Some(self.annotations[i].clone());
                    return Vec::new();
                }
            }
            self.handle = None;
            let families = &self.families;
            let hit = (self.annotations.iter()).rposition(|a| a.hit(p, text, families));
            let mut out = vec![Response::Redraw];
            match hit {
                Some(i) => {
                    self.selection = Some(self.annotations[i].id);
                    self.taken = Some(self.annotations[i].clone());
                    if clicks >= 2 && matches!(self.annotations[i].tool, Tool::Text | Tool::Stamp) {
                        // a double click on a stamp opens its emoji as text
                        out.push(self.begin_text(i));
                    }
                }
                None => (self.selection, self.taken) = (None, None),
            }
            return out;
        }
        let mut a = Annotation {
            start: p,
            end: p,
            color: PALETTE[self.palette].1,
            line_width: self.line_width,
            fill: self.fill,
            censor: self.censor,
            font_size: self.font_size,
            ..Annotation::new(self.new_id(), self.tool)
        };
        match self.tool {
            Tool::Pencil | Tool::Marker => a.points = vec![p],
            Tool::Badge => a.number = self.badge,
            Tool::Stamp => a.text = EMOJIS[self.emoji].to_owned(),
            _ => {}
        }
        self.drawing = Some(a);
        vec![Response::Redraw]
    }

    /// The pointer moved to `at` with the button down: the selection moves or its handle
    /// drags; a shape being drawn grows, or moves while space is held; a path goes on. A
    /// point that lands nowhere finite on the image changes nothing.
    pub fn drag(&mut self, at: Point) -> Vec<Response> {
        let p = self.image_point(at);
        if !finite(p) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.tool == Tool::Select {
            if let (Some(i), Some(taken)) = (self.selected_index(), &self.taken) {
                self.annotations[i] = match self.handle {
                    Some(handle) => taken.resized(handle, p),
                    None => taken.moved(Point {
                        x: p.x - self.drag_start.x,
                        y: p.y - self.drag_start.y,
                    }),
                };
                out.push(Response::Redraw);
            }
        } else if let Some(d) = &mut self.drawing {
            let path = matches!(d.tool, Tool::Pencil | Tool::Marker);
            if self.space && !path {
                *d = d.moved(Point {
                    x: p.x - self.last_drag.x,
                    y: p.y - self.last_drag.y,
                });
            } else if path {
                d.points.push(p);
            } else {
                d.end = p;
            }
            out.push(Response::Redraw);
        }
        self.last_drag = p;
        out
    }

    /// The button came up at `at`: what was drawn is added, unless it is a stray click (less
    /// than 3 pixels) of a tool that draws a shape, or a path of one point. A badge takes the
    /// next number, text asks for its words, and a shape's end stays on the image. A point
    /// that lands nowhere finite on the image ends the gesture where the pointer was last. A
    /// selection let go asks for a frame, which draws a censor moved at full quality.
    pub fn release(&mut self, at: Point) -> Vec<Response> {
        if self.tool == Tool::Select {
            self.handle = None;
            return match self.taken.take() {
                Some(_) => vec![Response::Redraw],
                None => Vec::new(),
            };
        }
        let Some(mut d) = self.drawing.take() else {
            return Vec::new();
        };
        let p = self.image_point(at);
        let p = if finite(p) { p } else { self.last_drag };
        match d.tool {
            Tool::Badge => self.badge += 1,
            Tool::Stamp => {}
            Tool::Text => {
                self.annotations.push(d);
                self.redo.clear();
                return vec![
                    Response::Redraw,
                    self.begin_text(self.annotations.len() - 1),
                ];
            }
            Tool::Pencil | Tool::Marker if d.points.len() < 2 => return vec![Response::Redraw],
            Tool::Pencil | Tool::Marker => {}
            _ if distance(p, self.drag_start) <= 3.0 => return vec![Response::Redraw],
            _ => {
                let (w, h) = (f64::from(self.base.width()), f64::from(self.base.height()));
                d.end = Point {
                    x: d.end.x.clamp(0.0, w),
                    y: d.end.y.clamp(0.0, h),
                };
            }
        }
        self.annotations.push(d);
        self.redo.clear();
        self.selection = None;
        vec![Response::Redraw]
    }

    /// Space went down or came up: held, it moves a shape being drawn instead of growing it.
    pub fn space(&mut self, held: bool) {
        self.space = held;
    }

    /// Asks for the text of annotation `i`: a field from the baseline up, as wide as the canvas
    /// leaves but 220 at least, in the text's color.
    fn begin_text(&mut self, i: usize) -> Response {
        let a = &self.annotations[i];
        self.editing = Some(a.id);
        let (at, k) = (self.view_point(a.start), self.fit());
        let height = a.font_size * k + 12.0;
        Response::EditText {
            rect: Rect {
                x: at.x,
                y: at.y + 4.0 - height,
                width: (self.view.width - at.x - 20.0).max(220.0),
                height,
            },
            text: a.text.clone(),
            font: (a.font_size * k).max(11.0),
            color: a.color,
        }
    }

    /// The text a frontend's field ended with, Return or Escape alike; text left empty goes.
    pub fn text_entered(&mut self, text: String) -> Vec<Response> {
        let Some(id) = self.editing.take() else {
            return Vec::new();
        };
        if let Some(i) = self.annotations.iter().position(|a| a.id == id) {
            if text.is_empty() && self.annotations[i].tool == Tool::Text {
                self.annotations.remove(i);
                // a selection of it, and a copy the pointer holds, go with it
                if self.selection == Some(id) {
                    self.selection = None;
                }
                if self.taken.as_ref().is_some_and(|taken| taken.id == id) {
                    (self.taken, self.handle) = (None, None);
                }
            } else {
                self.annotations[i].text = text;
            }
        }
        vec![Response::Redraw]
    }

    // MARK: keys

    /// A key, under the edit keys; ctrl-z, ctrl-Z, ctrl-s and ctrl-c are macOS's Command
    /// keys, which a frontend maps from its own. None for a key with Control that is
    /// not the edit's, which a frontend may take for itself.
    pub fn key(&mut self, key: &Keystroke) -> Option<Vec<Response>> {
        // while a field edits text, it has the keys
        if self.editing.is_some() {
            return Some(Vec::new());
        }
        if key.ctrl {
            match key.key.as_str() {
                "z" => self.undo(),
                "Z" => self.redo(),
                "s" => return Some(vec![Response::Leave { save: true }]),
                "c" => {
                    self.flash("copied to clipboard".to_owned());
                    return Some(vec![Response::CopyImage, Response::Bar]);
                }
                _ => return None,
            }
            return Some(vec![Response::Redraw]);
        }
        let tool = |tool| Some(tool);
        let chosen = match key.key.as_str() {
            "v" => tool(Tool::Select),
            "a" => tool(Tool::Arrow),
            "r" => tool(Tool::Rect),
            "e" => tool(Tool::Ellipse),
            "t" => tool(Tool::Text),
            "p" => tool(Tool::Pencil),
            "m" => tool(Tool::Marker),
            "n" => tool(Tool::Badge),
            "s" => tool(Tool::Stamp),
            "c" => tool(Tool::Censor),
            "h" => tool(Tool::Spotlight),
            _ => None,
        };
        if let Some(chosen) = chosen {
            // what the pointer was drawing goes, and a selection it took is let go
            (self.tool, self.drawing, self.taken, self.handle) = (chosen, None, None, None);
            return Some(vec![Response::Redraw]);
        }
        match key.key.as_str() {
            " " => {
                self.space = true;
                return Some(Vec::new());
            }
            "f" => {
                self.fill = match self.fill {
                    FillMode::Stroke => FillMode::StrokeFill,
                    FillMode::StrokeFill => FillMode::Fill,
                    FillMode::Fill => FillMode::Stroke,
                }
            }
            "C" => {
                self.censor = match self.censor {
                    CensorMode::Pixelate => CensorMode::Blur,
                    CensorMode::Blur => CensorMode::Solid,
                    CensorMode::Solid => CensorMode::Pixelate,
                }
            }
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" => {
                self.palette = usize::from(key.key.as_bytes()[0] - b'1');
                if let Some(i) = self.selected_index() {
                    self.annotations[i].color = PALETTE[self.palette].1;
                }
                // and the copy the pointer holds, which a drag puts back
                if let Some(taken) = &mut self.taken {
                    taken.color = PALETTE[self.palette].1;
                }
            }
            "[" => self.line_width = (self.line_width - 1.0).max(1.0),
            "]" => self.line_width = (self.line_width + 1.0).min(24.0),
            "{" => self.font_size = (self.font_size - 4.0).max(10.0),
            "}" => self.font_size = (self.font_size + 4.0).min(120.0),
            "<" => self.emoji = (self.emoji + EMOJIS.len() - 1) % EMOJIS.len(),
            ">" => self.emoji = (self.emoji + 1) % EMOJIS.len(),
            "u" => self.undo(),
            "U" => self.redo(),
            // OCR reads the flattened image, PII and faces the base
            "o" => return Some(self.ask(Asked::Copy, Recognize::Text, true)),
            "Q" => return Some(self.ask(Asked::Barcodes, Recognize::Barcodes, true)),
            "F" => return Some(self.ask(Asked::Faces, Recognize::Faces, false)),
            "P" => return Some(self.ask(Asked::Pii, Recognize::Text, false)),
            "B" => {
                self.flash("removing background…".to_owned());
                let mut out = self.ask(Asked::Foreground, Recognize::Foreground, false);
                out.push(Response::Bar);
                return Some(out);
            }
            // invert and background removal change the base, which undo leaves
            "I" => self.invert(),
            "D" | "BackSpace" | "Delete" => self.delete_selected(),
            "w" => return Some(vec![Response::Leave { save: true }]),
            // q, and Escape when nothing is selected, leave without asking, dropping every
            // annotation
            "q" => return Some(vec![Response::Leave { save: false }]),
            "Escape" if self.selection.is_some() => self.selection = None,
            "Escape" => return Some(vec![Response::Leave { save: false }]),
            _ => return Some(Vec::new()),
        }
        Some(vec![Response::Redraw])
    }

    fn undo(&mut self) {
        let Some(last) = self.annotations.pop() else {
            return;
        };
        if last.tool == Tool::Badge {
            self.badge = (self.badge - 1).max(1);
        }
        self.redo.push(last);
        self.selection = None;
    }

    fn redo(&mut self) {
        let Some(a) = self.redo.pop() else {
            return;
        };
        if a.tool == Tool::Badge {
            self.badge += 1;
        }
        self.annotations.push(a);
    }

    fn delete_selected(&mut self) {
        let Some(i) = self.selected_index() else {
            return;
        };
        // deleting any badge lowers the counter, so a number can come twice
        if self.annotations[i].tool == Tool::Badge {
            self.badge = (self.badge - 1).max(1);
        }
        self.annotations.remove(i);
        self.redo.clear();
        self.selection = None;
    }

    fn invert(&mut self) {
        for p in self.base.data_mut().as_chunks_mut::<4>().0 {
            for c in 0..3 {
                p[c] = p[3] - p[c];
            }
        }
        self.generation += 1;
    }

    fn ask(&mut self, asked: Asked, what: Recognize, flattened: bool) -> Vec<Response> {
        self.job += 1;
        self.asked.push((self.job, asked));
        vec![Response::Recognize {
            job: self.job,
            what,
            flattened,
        }]
    }

    /// Forgets recognition `job`, asked for and never to be answered, as one a frontend drops
    /// for another of its kind that waits already: no beep, no message.
    pub fn forget(&mut self, job: u64) {
        self.asked.retain(|&(asked, _)| asked != job);
    }

    /// How many recognitions were asked for and are neither answered nor forgotten.
    pub fn recognitions_waiting(&self) -> usize {
        self.asked.len()
    }

    /// The answer to recognition `job`, each one asked for taken as it comes; one answered
    /// already, or never asked for, changes nothing.
    pub fn recognized(&mut self, job: u64, answer: Recognition) -> Vec<Response> {
        let Some(i) = self.asked.iter().position(|&(asked, _)| asked == job) else {
            return Vec::new();
        };
        let (_, asked) = self.asked.remove(i);
        let (message, out) = match (asked, answer) {
            (_, Recognition::Unavailable) => (
                "this system recognizes nothing in images".to_owned(),
                vec![Response::Beep],
            ),
            (Asked::Copy, Recognition::Text(text, _)) if !text.is_empty() => (
                format!("OCR: {} chars copied", text.chars().count()),
                vec![Response::CopyText(text)],
            ),
            (Asked::Barcodes, Recognition::Barcodes(payloads)) if !payloads.is_empty() => (
                format!("QR: copied {} payload(s)", payloads.len()),
                vec![Response::CopyText(payloads.join("\n"))],
            ),
            (Asked::Faces, Recognition::Faces(rects)) if !rects.is_empty() => {
                self.censor_all(&rects);
                (format!("censored {} face(s)", rects.len()), Vec::new())
            }
            (Asked::Pii, Recognition::Text(_, words)) => {
                let rects = find_pii(&words);
                if rects.is_empty() {
                    return vec![Response::Beep];
                }
                self.censor_all(&rects);
                (
                    format!("censored {} PII region(s)", rects.len()),
                    Vec::new(),
                )
            }
            (Asked::Foreground, Recognition::Foreground(Some(mask)))
                if mask.len() == self.base.data().len() / 4 =>
            {
                self.cut_out(&mask);
                ("background removed".to_owned(), Vec::new())
            }
            (Asked::Foreground, Recognition::Foreground(_)) => {
                // words that say why on macOS 13 as well, where Vision has no foreground
                // request
                self.flash("background removal needs macOS 14+ / no subject found".to_owned());
                return vec![Response::Bar];
            }
            _ => return vec![Response::Beep],
        };
        self.flash(message);
        let mut out = out;
        out.push(Response::Redraw);
        out
    }

    /// Censors each rectangle with the censor mode chosen, one annotation, and one undo step,
    /// a rectangle.
    fn censor_all(&mut self, rects: &[Rect]) {
        for r in rects {
            let a = Annotation {
                start: Point { x: r.x, y: r.y },
                end: Point {
                    x: r.max_x(),
                    y: r.max_y(),
                },
                censor: self.censor,
                ..Annotation::new(self.new_id(), Tool::Censor)
            };
            self.annotations.push(a);
        }
        self.redo.clear();
    }

    /// Keeps of the base what the foreground's coverage covers.
    fn cut_out(&mut self, mask: &[u8]) {
        for (p, &m) in self
            .base
            .data_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(mask)
        {
            for c in p {
                *c = ((u32::from(*c) * u32::from(m) + 127) / 255) as u8;
            }
        }
        self.generation += 1;
    }

    // MARK: the status line

    /// The status line: on the left the tool and its settings, the width, the color, whether
    /// something is selected and how many annotations there are, or a message for three
    /// seconds; on the right the file's name.
    pub fn status(&self) -> (String, String) {
        let name = (self.source.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some((message, _)) = &self.flash {
            return (message.clone(), name);
        }
        let mut parts = vec![format!("EDIT [{}]", self.tool.name())];
        match self.tool {
            Tool::Rect | Tool::Ellipse => parts.push(
                match self.fill {
                    FillMode::Stroke => "stroke",
                    FillMode::StrokeFill => "stroke+fill",
                    FillMode::Fill => "fill",
                }
                .to_owned(),
            ),
            Tool::Censor => parts.push(self.censor.name().to_owned()),
            Tool::Stamp => parts.push(EMOJIS[self.emoji].to_owned()),
            Tool::Badge => parts.push(format!("#{}", self.badge)),
            _ => {}
        }
        parts.push(format!("w{}", self.line_width));
        parts.push(PALETTE[self.palette].0.to_owned());
        if self.selection.is_some() {
            parts.push("sel".to_owned());
        }
        parts.push(format!("{} items", self.annotations.len()));
        (parts.join(" | "), name)
    }

    /// When the message on the status line goes.
    pub fn deadline(&self) -> Option<Instant> {
        self.flash.as_ref().map(|(_, until)| *until)
    }

    pub fn timers(&mut self, now: Instant) -> Vec<Response> {
        match &self.flash {
            Some((_, until)) if *until <= now => {
                self.flash = None;
                vec![Response::Bar]
            }
            _ => Vec::new(),
        }
    }
}

fn finite(p: Point) -> bool {
    p.x.is_finite() && p.y.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{families, text};
    use crate::{Word, recognize};

    const GRAY: [u8; 4] = [100, 120, 140, 255];

    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn image_of(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .flat_map(|(x, y)| pixel(x, y))
            .collect()
    }

    fn edit_of(
        rgba: &[u8],
        width: u32,
        height: u32,
        turn: Turn,
        cap: u64,
    ) -> Result<Edit, EditError> {
        let levels = [rgba];
        let image = Image {
            width,
            height,
            levels: &levels,
            opaque: rgba.chunks(4).all(|p| p[3] == 255),
        };
        let source = PathBuf::from("/pics/photo.jpg");
        Edit::new(&image, turn, None, cap, source, families())
    }

    /// A 400 × 300 gray image in a canvas of its size, so a point of the canvas is the
    /// image's pixel.
    fn edit() -> Edit {
        let rgba = image_of(400, 300, |_, _| GRAY);
        let mut edit = edit_of(&rgba, 400, 300, Turn::default(), u64::MAX).unwrap();
        edit.resize(Size {
            width: 400.0,
            height: 300.0,
        });
        edit
    }

    fn key(edit: &mut Edit, name: &str) -> Vec<Response> {
        edit.key(&Keystroke::new(name)).expect("an edit key")
    }

    fn ctrl(edit: &mut Edit, name: &str) -> Option<Vec<Response>> {
        edit.key(&Keystroke::ctrl(name))
    }

    /// A press at the first point, drags through the others, and a release at the last.
    fn stroke(edit: &mut Edit, text: &mut Text, points: &[Point]) -> Vec<Response> {
        let mut out = edit.press(points[0], 1, text);
        for &at in &points[1..] {
            out.extend(edit.drag(at));
        }
        out.extend(edit.release(points[points.len() - 1]));
        out
    }

    fn left(edit: &Edit) -> String {
        edit.status().0
    }

    #[test]
    fn the_base_is_the_image_as_shown_and_one_past_the_cap_is_refused() {
        // a 3 × 2 image, its corner red and half clear
        let rgba = image_of(3, 2, |x, y| {
            if (x, y) == (0, 0) {
                [255, 0, 0, 128]
            } else {
                GRAY
            }
        });
        let turn = Turn {
            rotation: 90,
            flip_h: false,
            flip_v: false,
        };
        let edit = edit_of(&rgba, 3, 2, turn, 1 << 20).unwrap();
        // turned clockwise, the top left corner goes to the top right
        let base = edit.base_image();
        assert_eq!((base.width, base.height), (2, 3));
        assert_eq!(base.rgba[4..8], [255, 0, 0, 128], "alpha kept");
        assert_eq!(base.rgba[..4], GRAY);
        assert_eq!(edit.status().1, "photo.jpg");
        // eight copies of 4 bytes a pixel, and the blur's line of 16 bytes a pixel of the
        // longer side
        assert_eq!(
            edit_of(&rgba, 3, 2, turn, 239).err(),
            Some(EditError::TooLarge {
                needed: 240,
                cap: 239
            })
        );
        assert!(edit_of(&rgba, 3, 2, turn, 240).is_ok());
        // one pixel high, where the line is as much as four copies
        let line = image_of(5, 1, |_, _| GRAY);
        let flat = Turn::default();
        let refused = EditError::TooLarge {
            needed: 160 + 80,
            cap: 239,
        };
        assert_eq!(edit_of(&line, 5, 1, flat, 239).err(), Some(refused));
        assert!(edit_of(&line, 5, 1, flat, 240).is_ok());
    }

    #[test]
    fn a_size_whose_bytes_overflow_is_refused_whatever_the_cap() {
        let levels: [&[u8]; 1] = [&[]];
        let image = Image {
            width: u32::MAX,
            height: u32::MAX,
            levels: &levels,
            opaque: true,
        };
        let source = PathBuf::from("huge.png");
        let edit = Edit::new(&image, Turn::default(), None, u64::MAX, source, families());
        let refused = EditError::TooLarge {
            needed: u64::MAX,
            cap: u64::MAX,
        };
        assert_eq!(edit.err(), Some(refused));
    }

    #[test]
    fn the_image_fits_the_canvas_centered_and_never_grows() {
        let mut edit = edit();
        let (mut text, wide) = (text(), [p(10.0, 10.0), p(100.0, 80.0)]);
        // half the size, in a canvas 300 wide: 50 to each side
        edit.resize(Size {
            width: 300.0,
            height: 150.0,
        });
        stroke(&mut edit, &mut text, &[p(55.0, 5.0), p(100.0, 40.0)]);
        let a = &edit.annotations()[0];
        assert_eq!((a.start, a.end), (wide[0], wide[1]));
        // twice the size: shown at 100%, in the middle
        edit.resize(Size {
            width: 800.0,
            height: 600.0,
        });
        stroke(&mut edit, &mut text, &[p(210.0, 160.0), p(300.0, 230.0)]);
        let a = &edit.annotations()[1];
        assert_eq!((a.start, a.end), (wide[0], wide[1]));
    }

    #[test]
    fn each_tool_is_drawn_by_a_press_drags_and_a_release() {
        let mut text = text();
        let path = [p(10.0, 10.0), p(50.0, 40.0), p(100.0, 80.0)];
        for (name, tool) in [
            ("a", Tool::Arrow),
            ("r", Tool::Rect),
            ("e", Tool::Ellipse),
            ("p", Tool::Pencil),
            ("m", Tool::Marker),
            ("n", Tool::Badge),
            ("s", Tool::Stamp),
            ("c", Tool::Censor),
            ("h", Tool::Spotlight),
            ("t", Tool::Text),
        ] {
            let mut edit = edit();
            key(&mut edit, name);
            let out = stroke(&mut edit, &mut text, &path);
            assert_eq!(edit.annotations().len(), 1, "{name}");
            let a = &edit.annotations()[0];
            // a path keeps its points, and its end where it began
            let (end, points) = if matches!(tool, Tool::Pencil | Tool::Marker) {
                (path[0], path.to_vec())
            } else {
                (path[2], Vec::new())
            };
            assert_eq!((a.tool, a.start, a.end), (tool, path[0], end), "{name}");
            assert_eq!(a.points, points, "{name}");
            let words = match tool {
                Tool::Stamp => "✅",
                _ => "",
            };
            assert_eq!((a.text.as_str(), a.number), (words, 1), "{name}");
            let asks = out.iter().any(|r| matches!(r, Response::EditText { .. }));
            assert_eq!(asks, tool == Tool::Text, "{name}");
        }
    }

    #[test]
    fn a_stray_click_draws_a_shape_of_nothing_but_places_a_badge_or_a_stamp() {
        let mut text = text();
        for name in ["a", "r", "e", "c", "h", "p", "m"] {
            let mut edit = edit();
            key(&mut edit, name);
            stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(12.0, 12.0)]);
            let drawn = matches!(name, "p" | "m");
            assert_eq!(
                edit.annotations().len(),
                usize::from(drawn),
                "{name}: 2.8 pixels"
            );
            stroke(&mut edit, &mut text, &[p(10.0, 10.0)]);
            assert_eq!(
                edit.annotations().len(),
                usize::from(drawn),
                "{name}: a click"
            );
        }
        for name in ["n", "s"] {
            let mut edit = edit();
            key(&mut edit, name);
            stroke(&mut edit, &mut text, &[p(10.0, 10.0)]);
            assert_eq!(edit.annotations().len(), 1, "{name}");
        }
    }

    #[test]
    fn a_shapes_end_stays_on_the_image() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "r");
        stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(500.0, -20.0)]);
        assert_eq!(edit.annotations()[0].end, p(400.0, 0.0));
        // a path's points are where the pointer went
        key(&mut edit, "p");
        stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(500.0, -20.0)]);
        assert_eq!(edit.annotations()[1].points[1], p(500.0, -20.0));
    }

    #[test]
    fn space_held_moves_a_shape_while_it_is_drawn() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "r");
        edit.press(p(10.0, 10.0), 1, &mut text);
        edit.drag(p(50.0, 50.0));
        key(&mut edit, " ");
        edit.drag(p(60.0, 70.0));
        edit.space(false);
        edit.drag(p(80.0, 80.0));
        edit.release(p(80.0, 80.0));
        let a = &edit.annotations()[0];
        assert_eq!((a.start, a.end), (p(20.0, 30.0), p(80.0, 80.0)));
        // not a path, which goes on through
        key(&mut edit, "p");
        edit.press(p(10.0, 10.0), 1, &mut text);
        edit.space(true);
        edit.drag(p(60.0, 70.0));
        edit.release(p(60.0, 70.0));
        assert_eq!(edit.annotations()[1].points, [p(10.0, 10.0), p(60.0, 70.0)]);
    }

    #[test]
    fn select_moves_an_annotation_and_resizes_it_by_each_handle() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "r");
        stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(100.0, 80.0)]);
        key(&mut edit, "v");
        // the stroke, inside the hollow's edge
        stroke(&mut edit, &mut text, &[p(12.0, 40.0), p(22.0, 50.0)]);
        let id = edit.annotations()[0].id;
        assert_eq!(edit.selection(), Some(id));
        assert!(left(&edit).contains("| sel |"), "{}", left(&edit));
        let rect = |edit: &Edit| {
            let r = edit.annotations()[0].rect();
            (r.x, r.y, r.max_x(), r.max_y())
        };
        assert_eq!(rect(&edit), (20.0, 20.0, 110.0, 90.0));
        // each handle taken from within 12 pixels of it, and dragged 10 pixels out
        for (handle, from, expected) in [
            (0, p(25.0, 25.0), (15.0, 15.0, 110.0, 90.0)),
            (1, p(105.0, 15.0), (15.0, 5.0, 115.0, 90.0)),
            (2, p(15.0, 85.0), (5.0, 5.0, 115.0, 95.0)),
            (3, p(112.0, 92.0), (5.0, 5.0, 122.0, 102.0)),
        ] {
            let out = |v: f64, middle: f64| v + (v - middle).signum() * 10.0;
            let to = p(out(from.x, 60.0), out(from.y, 50.0));
            stroke(&mut edit, &mut text, &[from, to]);
            assert_eq!(rect(&edit), expected, "handle {handle}");
            assert_eq!(edit.selection(), Some(id));
        }
        // a click on nothing lets go
        stroke(&mut edit, &mut text, &[p(300.0, 250.0)]);
        assert_eq!(edit.selection(), None);
    }

    #[test]
    fn choosing_a_tool_drops_what_the_pointer_draws_and_lets_go_of_a_selection() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "r");
        edit.press(p(10.0, 10.0), 1, &mut text);
        edit.drag(p(100.0, 80.0));
        key(&mut edit, "v");
        assert_eq!(edit.drawing, None);
        assert_eq!(edit.release(p(100.0, 80.0)), []);
        assert!(edit.annotations().is_empty());
        // a selection taken, then another tool: the drag that follows moves nothing
        key(&mut edit, "r");
        stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(100.0, 80.0)]);
        key(&mut edit, "v");
        edit.press(p(12.0, 40.0), 1, &mut text);
        key(&mut edit, "a");
        key(&mut edit, "v");
        edit.drag(p(52.0, 80.0));
        assert_eq!(edit.annotations()[0].start, p(10.0, 10.0));
    }

    #[test]
    fn a_color_key_while_the_pointer_holds_a_selection_stays() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "r");
        stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(100.0, 80.0)]);
        key(&mut edit, "v");
        edit.press(p(12.0, 40.0), 1, &mut text);
        key(&mut edit, "5");
        edit.drag(p(22.0, 50.0));
        edit.release(p(22.0, 50.0));
        let a = &edit.annotations()[0];
        assert_eq!((a.start, a.color), (p(20.0, 20.0), PALETTE[4].1));
    }

    #[test]
    fn text_left_empty_lets_go_of_its_selection() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "t");
        stroke(&mut edit, &mut text, &[p(40.0, 100.0)]);
        edit.text_entered("Hi".to_owned());
        key(&mut edit, "v");
        edit.press(p(45.0, 90.0), 2, &mut text);
        edit.release(p(45.0, 90.0));
        edit.text_entered(String::new());
        assert!(edit.annotations().is_empty());
        assert_eq!(edit.selection(), None);
        assert_eq!(left(&edit), "EDIT [select] | w4 | red | 0 items");
        assert_eq!(key(&mut edit, "Escape"), [Response::Leave { save: false }]);
    }

    #[test]
    fn letting_go_of_a_selection_asks_for_a_frame() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "c");
        stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(100.0, 80.0)]);
        key(&mut edit, "v");
        edit.press(p(50.0, 40.0), 1, &mut text);
        edit.drag(p(60.0, 50.0));
        assert_eq!(edit.release(p(60.0, 50.0)), [Response::Redraw]);
        // nothing taken, nothing to draw again
        edit.press(p(300.0, 250.0), 1, &mut text);
        assert_eq!(edit.release(p(300.0, 250.0)), []);
    }

    #[test]
    fn a_point_or_a_view_that_is_not_finite_changes_nothing() {
        let (mut edit, mut text) = (edit(), text());
        let nan = p(f64::NAN, 10.0);
        key(&mut edit, "r");
        assert_eq!(edit.press(nan, 1, &mut text), []);
        assert_eq!(edit.drawing, None);
        // a shape ends where the pointer was last
        edit.press(p(10.0, 10.0), 1, &mut text);
        edit.drag(p(100.0, 80.0));
        assert_eq!(edit.drag(p(f64::INFINITY, 0.0)), []);
        edit.release(nan);
        let a = &edit.annotations()[0];
        assert_eq!((a.start, a.end), (p(10.0, 10.0), p(100.0, 80.0)));
        // a selection dragged to nowhere stays where it was
        key(&mut edit, "v");
        edit.press(p(12.0, 40.0), 1, &mut text);
        edit.drag(nan);
        edit.release(nan);
        let a = &edit.annotations()[0];
        assert_eq!((a.start, a.end), (p(10.0, 10.0), p(100.0, 80.0)));
        // and to a finite point over a view of almost nothing, which lands at infinity
        edit.press(p(12.0, 40.0), 1, &mut text);
        edit.resize(Size {
            width: 1e-320,
            height: 1e-320,
        });
        edit.drag(p(50.0, 50.0));
        edit.release(p(50.0, 50.0));
        let a = &edit.annotations()[0];
        assert_eq!((a.start, a.end), (p(10.0, 10.0), p(100.0, 80.0)));
        // the view stays: half the size, so (50, 50) is the image's (100, 100)
        edit.resize(Size {
            width: 200.0,
            height: 150.0,
        });
        edit.resize(Size {
            width: f64::NAN,
            height: 150.0,
        });
        key(&mut edit, "a");
        stroke(&mut edit, &mut text, &[p(50.0, 50.0), p(150.0, 50.0)]);
        assert_eq!(edit.annotations()[1].start, p(100.0, 100.0));
    }

    #[test]
    fn the_badge_counter_follows_drawing_undo_redo_and_delete() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "n");
        for x in [50.0, 150.0] {
            stroke(&mut edit, &mut text, &[p(x, 50.0)]);
        }
        let numbers = |edit: &Edit| {
            edit.annotations()
                .iter()
                .map(|a| a.number)
                .collect::<Vec<_>>()
        };
        assert_eq!(numbers(&edit), [1, 2]);
        assert!(left(&edit).starts_with("EDIT [badge] | #3 |"));
        key(&mut edit, "u");
        assert!(left(&edit).contains("#2"));
        key(&mut edit, "U");
        assert!(left(&edit).contains("#3"));
        // deleting the first lowers the counter all the same, so the next repeats a number
        key(&mut edit, "v");
        stroke(&mut edit, &mut text, &[p(50.0, 50.0)]);
        key(&mut edit, "D");
        key(&mut edit, "n");
        stroke(&mut edit, &mut text, &[p(250.0, 50.0)]);
        assert_eq!(numbers(&edit), [2, 2]);
        // undo never takes it below 1
        for _ in 0..3 {
            key(&mut edit, "u");
        }
        assert!(left(&edit).contains("#1"));
    }

    #[test]
    fn undo_and_redo_step_through_the_annotations_and_a_new_one_ends_redo() {
        let (mut edit, mut text) = (edit(), text());
        for y in [10.0, 20.0, 30.0] {
            stroke(&mut edit, &mut text, &[p(10.0, y), p(100.0, y)]);
        }
        let ys = |edit: &Edit| {
            edit.annotations()
                .iter()
                .map(|a| a.start.y)
                .collect::<Vec<_>>()
        };
        key(&mut edit, "u");
        ctrl(&mut edit, "z");
        assert_eq!(ys(&edit), [10.0]);
        ctrl(&mut edit, "Z");
        assert_eq!(ys(&edit), [10.0, 20.0]);
        stroke(&mut edit, &mut text, &[p(10.0, 40.0), p(100.0, 40.0)]);
        key(&mut edit, "U");
        assert_eq!(ys(&edit), [10.0, 20.0, 40.0], "nothing left to redo");
        for _ in 0..5 {
            key(&mut edit, "u");
        }
        assert!(edit.annotations().is_empty());
    }

    #[test]
    fn the_keys_change_the_tool_and_its_settings_as_the_status_line_says() {
        let (mut edit, mut text) = (edit(), text());
        assert_eq!(left(&edit), "EDIT [arrow] | w4 | red | 0 items");
        for (name, status) in [
            ("v", "EDIT [select] | w4 | red | 0 items"),
            ("r", "EDIT [rect] | stroke | w4 | red | 0 items"),
            ("f", "EDIT [rect] | stroke+fill | w4 | red | 0 items"),
            ("f", "EDIT [rect] | fill | w4 | red | 0 items"),
            ("e", "EDIT [ellipse] | fill | w4 | red | 0 items"),
            ("f", "EDIT [ellipse] | stroke | w4 | red | 0 items"),
            ("t", "EDIT [text] | w4 | red | 0 items"),
            ("p", "EDIT [pencil] | w4 | red | 0 items"),
            ("m", "EDIT [marker] | w4 | red | 0 items"),
            ("n", "EDIT [badge] | #1 | w4 | red | 0 items"),
            ("s", "EDIT [stamp] | ✅ | w4 | red | 0 items"),
            (">", "EDIT [stamp] | ❌ | w4 | red | 0 items"),
            ("<", "EDIT [stamp] | ✅ | w4 | red | 0 items"),
            ("<", "EDIT [stamp] | 🍆 | w4 | red | 0 items"),
            ("c", "EDIT [censor] | pixelate | w4 | red | 0 items"),
            ("C", "EDIT [censor] | blur | w4 | red | 0 items"),
            ("C", "EDIT [censor] | solid | w4 | red | 0 items"),
            ("C", "EDIT [censor] | pixelate | w4 | red | 0 items"),
            ("h", "EDIT [spotlight] | w4 | red | 0 items"),
            ("a", "EDIT [arrow] | w4 | red | 0 items"),
            ("]", "EDIT [arrow] | w5 | red | 0 items"),
            ("[", "EDIT [arrow] | w4 | red | 0 items"),
            ("2", "EDIT [arrow] | w4 | orange | 0 items"),
            ("3", "EDIT [arrow] | w4 | yellow | 0 items"),
            ("4", "EDIT [arrow] | w4 | green | 0 items"),
            ("5", "EDIT [arrow] | w4 | blue | 0 items"),
            ("6", "EDIT [arrow] | w4 | purple | 0 items"),
            ("7", "EDIT [arrow] | w4 | black | 0 items"),
            ("8", "EDIT [arrow] | w4 | white | 0 items"),
            ("1", "EDIT [arrow] | w4 | red | 0 items"),
        ] {
            assert_eq!(key(&mut edit, name), [Response::Redraw], "{name}");
            assert_eq!(left(&edit), status, "after {name}");
        }
        // the width from 1 to 24
        for _ in 0..30 {
            key(&mut edit, "]");
        }
        assert!(left(&edit).contains("| w24 |"));
        for _ in 0..30 {
            key(&mut edit, "[");
        }
        assert!(left(&edit).contains("| w1 |"));
        // the font size from 10 to 120 by 4, which new text takes
        let font_size = |edit: &mut Edit, text: &mut Text| {
            key(edit, "t");
            stroke(edit, text, &[p(10.0, 50.0)]);
            edit.text_entered("x".to_owned());
            edit.annotations().last().unwrap().font_size
        };
        key(&mut edit, "}");
        assert_eq!(font_size(&mut edit, &mut text), 32.0);
        for _ in 0..30 {
            key(&mut edit, "}");
        }
        assert_eq!(font_size(&mut edit, &mut text), 120.0);
        for _ in 0..30 {
            key(&mut edit, "{");
        }
        assert_eq!(font_size(&mut edit, &mut text), 10.0);
        // a color key colors the selection too
        key(&mut edit, "v");
        stroke(&mut edit, &mut text, &[p(10.0, 50.0)]);
        key(&mut edit, "5");
        assert_eq!(edit.annotations()[2].color, PALETTE[4].1);
        assert!(left(&edit).ends_with("| blue | sel | 3 items"));
        // BackSpace and Delete delete it as D does
        for name in ["BackSpace", "Delete"] {
            stroke(&mut edit, &mut text, &[p(10.0, 50.0)]);
            key(&mut edit, name);
        }
        assert_eq!(edit.annotations().len(), 1);
        // a key that is no edit key does nothing, and one with Control is not the edit's
        assert_eq!(key(&mut edit, "y"), []);
        assert_eq!(ctrl(&mut edit, "q"), None);
    }

    #[test]
    fn the_keys_that_leave_save_copy_and_recognize() {
        let (mut edit, mut text) = (edit(), text());
        let leave = |save| vec![Response::Leave { save }];
        assert_eq!(key(&mut edit, "w"), leave(true));
        assert_eq!(ctrl(&mut edit, "s"), Some(leave(true)));
        assert_eq!(key(&mut edit, "q"), leave(false));
        // Escape lets go of the selection first
        stroke(&mut edit, &mut text, &[p(10.0, 10.0), p(100.0, 10.0)]);
        key(&mut edit, "v");
        stroke(&mut edit, &mut text, &[p(50.0, 10.0)]);
        assert_eq!(key(&mut edit, "Escape"), [Response::Redraw]);
        assert_eq!(edit.selection(), None);
        assert_eq!(key(&mut edit, "Escape"), leave(false));
        assert_eq!(
            ctrl(&mut edit, "c"),
            Some(vec![Response::CopyImage, Response::Bar])
        );
        let asked = |what, flattened| Response::Recognize {
            job: 0,
            what,
            flattened,
        };
        let job = |mut out: Vec<Response>| {
            if let Some(Response::Recognize { job, .. }) = out.first_mut() {
                *job = 0;
            }
            out
        };
        assert_eq!(job(key(&mut edit, "o")), [asked(Recognize::Text, true)]);
        assert_eq!(job(key(&mut edit, "Q")), [asked(Recognize::Barcodes, true)]);
        assert_eq!(job(key(&mut edit, "F")), [asked(Recognize::Faces, false)]);
        assert_eq!(job(key(&mut edit, "P")), [asked(Recognize::Text, false)]);
        assert_eq!(
            job(key(&mut edit, "B")),
            [asked(Recognize::Foreground, false), Response::Bar]
        );
        assert_eq!(left(&edit), "removing background…");
        // invert changes the base, which undo leaves as it is
        let generation = edit.generation;
        assert_eq!(key(&mut edit, "I"), [Response::Redraw]);
        assert_eq!(edit.base_image().rgba[..4], [155, 135, 115, 255]);
        assert_eq!(edit.generation, generation + 1);
        key(&mut edit, "u");
        assert_eq!(edit.base_image().rgba[..4], [155, 135, 115, 255]);
    }

    #[test]
    fn a_message_stands_for_the_status_line_three_seconds() {
        let mut edit = edit();
        let before = Instant::now();
        ctrl(&mut edit, "c");
        let until = edit.deadline().unwrap();
        assert!(until >= before + FLASH && until <= Instant::now() + FLASH);
        assert_eq!(left(&edit), "copied to clipboard");
        assert_eq!(edit.timers(until - Duration::from_millis(1)), []);
        assert_eq!(left(&edit), "copied to clipboard");
        assert_eq!(edit.timers(until), [Response::Bar]);
        assert_eq!(left(&edit), "EDIT [arrow] | w4 | red | 0 items");
        assert_eq!(edit.deadline(), None);
    }

    #[test]
    fn text_asks_for_its_words_and_left_empty_goes() {
        let (mut edit, mut text) = (edit(), text());
        key(&mut edit, "t");
        let out = stroke(&mut edit, &mut text, &[p(40.0, 100.0)]);
        // from the baseline up, 28 + 12 high, as wide as the canvas leaves but 220 at least
        let field = Response::EditText {
            rect: Rect {
                x: 40.0,
                y: 64.0,
                width: 340.0,
                height: 40.0,
            },
            text: String::new(),
            font: 28.0,
            color: PALETTE[0].1,
        };
        assert_eq!(out, [Response::Redraw, Response::Redraw, field]);
        // the field has the keys meanwhile
        assert_eq!(key(&mut edit, "r"), []);
        assert_eq!(edit.tool(), Tool::Text);
        assert_eq!(edit.text_entered(String::new()), [Response::Redraw]);
        assert!(edit.annotations().is_empty());
        stroke(&mut edit, &mut text, &[p(40.0, 100.0)]);
        edit.text_entered("Hi\nthere".to_owned());
        assert_eq!(edit.annotations()[0].text, "Hi\nthere");
        // a double click on it opens it again
        key(&mut edit, "v");
        let out = edit.press(p(45.0, 90.0), 2, &mut text);
        let Some(Response::EditText { text: words, .. }) = out.last() else {
            panic!("{out:?}");
        };
        assert_eq!(words, "Hi\nthere");
        edit.release(p(45.0, 90.0));
        edit.text_entered("Hi".to_owned());
        assert_eq!(edit.annotations()[0].text, "Hi");
    }

    #[test]
    fn a_recognition_forgotten_is_answered_with_nothing() {
        let mut edit = edit();
        let job = match key(&mut edit, "o")[0] {
            Response::Recognize { job, .. } => job,
            _ => unreachable!(),
        };
        edit.forget(job);
        assert_eq!(edit.recognitions_waiting(), 0);
        let text = Recognition::Text("abc".to_owned(), Vec::new());
        assert_eq!(edit.recognized(job, text), []);
    }

    #[test]
    fn each_recognition_asked_counts_as_its_answer_comes() {
        let mut edit = edit();
        let job = |out: Vec<Response>| match out[0] {
            Response::Recognize { job, .. } => job,
            _ => panic!("{out:?}"),
        };
        // o, then Q before o's answer: the text is copied, then the payloads
        let ocr = job(key(&mut edit, "o"));
        let barcodes = job(key(&mut edit, "Q"));
        let text = Recognition::Text("abc".to_owned(), Vec::new());
        assert_eq!(
            edit.recognized(ocr, text.clone()),
            [Response::CopyText("abc".to_owned()), Response::Redraw]
        );
        assert_eq!(left(&edit), "OCR: 3 chars copied");
        let payloads = Recognition::Barcodes(vec!["x".to_owned(), "y".to_owned()]);
        assert_eq!(
            edit.recognized(barcodes, payloads.clone()),
            [Response::CopyText("x\ny".to_owned()), Response::Redraw]
        );
        assert_eq!(left(&edit), "QR: copied 2 payload(s)");
        assert_eq!(edit.recognized(barcodes, payloads), [], "answered already");
        // F, then P before F's answer: the faces and the personal data are both censored
        let rect = |x| Rect {
            x,
            y: 10.0,
            width: 20.0,
            height: 30.0,
        };
        let faces = job(key(&mut edit, "F"));
        let pii = job(key(&mut edit, "P"));
        edit.recognized(faces, Recognition::Faces(vec![rect(1.0)]));
        assert_eq!(left(&edit), "censored 1 face(s)");
        let words = vec![Word {
            text: "jane@example.org".to_owned(),
            rect: rect(200.0),
            line: 0,
        }];
        edit.recognized(pii, Recognition::Text(String::new(), words));
        assert_eq!(left(&edit), "censored 1 PII region(s)");
        assert_eq!(edit.annotations().len(), 2, "a face and an address");
        // nothing found beeps
        let ocr = job(key(&mut edit, "o"));
        let nothing = Recognition::Text(String::new(), Vec::new());
        assert_eq!(edit.recognized(ocr, nothing), [Response::Beep]);
        let faces = job(key(&mut edit, "F"));
        assert_eq!(
            edit.recognized(faces, Recognition::Unavailable)[0],
            Response::Beep
        );
    }

    #[test]
    fn faces_and_personal_data_are_censored_one_annotation_a_region() {
        let mut edit = edit();
        key(&mut edit, "C");
        let job = |out: Vec<Response>| match out[0] {
            Response::Recognize { job, .. } => job,
            _ => panic!("{out:?}"),
        };
        let rect = |x| Rect {
            x,
            y: 10.0,
            width: 20.0,
            height: 30.0,
        };
        let faces = job(key(&mut edit, "F"));
        edit.recognized(faces, Recognition::Faces(vec![rect(1.0), rect(50.0)]));
        assert_eq!(left(&edit), "censored 2 face(s)");
        let a = &edit.annotations()[1];
        assert_eq!((a.tool, a.censor), (Tool::Censor, CensorMode::Blur));
        assert_eq!((a.start, a.end), (p(50.0, 10.0), p(70.0, 40.0)));
        let words = vec![
            Word {
                text: "mail".to_owned(),
                rect: rect(100.0),
                line: 0,
            },
            Word {
                text: "jane@example.org".to_owned(),
                rect: rect(200.0),
                line: 0,
            },
        ];
        let pii = job(key(&mut edit, "P"));
        edit.recognized(pii, Recognition::Text(String::new(), words));
        assert_eq!(left(&edit), "censored 1 PII region(s)");
        assert_eq!(edit.annotations()[2].start, p(197.0, 7.0));
        key(&mut edit, "u");
        assert_eq!(edit.annotations().len(), 2, "one undo step a region");
        // words with nothing personal in them beep, and censor nothing
        let words = vec![Word {
            text: "mail".to_owned(),
            rect: rect(100.0),
            line: 0,
        }];
        let pii = job(key(&mut edit, "P"));
        let beep = edit.recognized(pii, Recognition::Text(String::new(), words));
        assert_eq!((beep, edit.annotations().len()), (vec![Response::Beep], 2));
    }

    #[test]
    fn background_removal_keeps_what_the_foreground_covers() {
        let mut edit = edit();
        let job = match key(&mut edit, "B")[0] {
            Response::Recognize { job, .. } => job,
            _ => unreachable!(),
        };
        let mut mask = vec![255; 400 * 300];
        mask[0] = 0;
        edit.recognized(job, Recognition::Foreground(Some(mask)));
        assert_eq!(left(&edit), "background removed");
        let base = edit.base_image();
        assert_eq!((base.rgba[3], &base.rgba[4..8]), (0, &GRAY[..]));
        let job = match key(&mut edit, "B")[0] {
            Response::Recognize { job, .. } => job,
            _ => unreachable!(),
        };
        assert_eq!(
            edit.recognized(job, Recognition::Foreground(None)),
            [Response::Bar]
        );
        assert_eq!(
            left(&edit),
            "background removal needs macOS 14+ / no subject found"
        );
        // without a recognizer every kind is unavailable
        let image = edit.base_image();
        assert_eq!(
            recognize(None, Recognize::Faces, &image),
            Recognition::Unavailable
        );
    }
}
