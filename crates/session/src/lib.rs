//! One running viewer: the viewer's state with the loading, the thumbnails, the animation and
//! the drawing that every frontend needs, and no event loop or display of its own.
//!
//! A frontend passes events in through methods and carries out the [`Request`]s each returns,
//! in order. Decoders, thumbnail makers, animations and the font loader answer on threads of
//! their own: each answer calls the frontend's `wake`, from any thread, and the frontend then
//! calls [`Session::answers`] on its own. The viewer's timers are deadlines the session keeps:
//! after every call into the session, the frontend reads [`Session::deadline`] again, and once
//! that time has passed it calls [`Session::timers`].

#![forbid(unsafe_code)]

mod edit;
mod key_handler;
mod line;
mod pointer;
mod still;
mod thumbnails;

pub use edit::EditSetup;
pub use key_handler::key_handlers;
pub use nsxiv_ng_edit::Recognizer;
pub use nsxiv_ng_render::text::{FontBytes, MapFont};
pub use pointer::{Button, Scroll};
pub use still::Still;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use nsxiv_ng_core::config::{Config, ZOOM_LEVELS};
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::Options;
use nsxiv_ng_core::viewer::{Effect, ImageInfo, Keystroke, LoadError, Part, Timer, Viewer};
use nsxiv_ng_core::viewport::Viewport;
use nsxiv_ng_core::{Rect, Size};
use nsxiv_ng_decode::{Animation, DecodeError, Image, Loaded, Loader, Pyramid, Stamp};
use nsxiv_ng_helper::Helpers;
use nsxiv_ng_render::grid_view::{self, GridColors};
use nsxiv_ng_render::image_view::{self, Turn};
use nsxiv_ng_render::text::Text;
use nsxiv_ng_render::{Canvas, Colors, bar, canvas};

use thumbnails::{Made, Thumbs};

/// How long a decoding helper may work on one file before it is ended. A large AVIF or a heavy
/// SVG takes seconds on a slow machine, and failing to open it is worse than a stuck file
/// holding one helper a while longer.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the status bar shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bar {
    /// [`Session::draw`] draws it at the bottom of the canvas, as high as its font needs.
    Drawn,
    /// The frontend shows it itself, as a terminal does on its last row. While it shows, it
    /// takes this many logical units at the bottom of the window, and the canvas the session
    /// draws is the rest.
    Shown(f64),
}

/// How [`Session::draw`] draws a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// The canvas outside the image stays clear, so the frontend's own background shows
    /// there, as a terminal's does. A frame that shows the text of what went wrong is filled
    /// with the background all the same, whole, since the text's color is chosen for it.
    pub clear: bool,
    /// The image alone, all of it over the canvas above a bar the session draws, or the whole
    /// canvas without one, which the frontend sized for it, as for a crop a terminal pans in.
    pub whole: bool,
    /// The canvas holds the frame [`Session::draw`] drew last, as it drew it: a promise only
    /// the frontend can make, which lets a pan move that frame and draw only what came into
    /// view, the same pixels as the whole frame drawn.
    pub kept: bool,
}

/// What the last frame [`Session::draw`] drew showed, where it was an opaque still that covered
/// the area above the bar: the image, the view of it, the scale, the area's size, where the
/// whole image lay scaled, the turn, and the steps of gamma and contrast.
struct LastFrame {
    image: Weak<Pyramid>,
    view: Viewport,
    scale: f64,
    area: (u32, u32),
    origin: (i64, i64),
    turn: Turn,
    colors: (i32, i32),
}

/// How a pan moves the last frame: where the whole image lies scaled now, how far it moved,
/// and the strips of the area that came into view.
struct Move {
    origin: (i64, i64),
    by: (i64, i64),
    strips: Vec<(i64, i64, i64, i64)>,
}

/// The colors a frame is drawn in, each an sRGB triple: the config's five, the grid's
/// placeholder for a thumbnail not in, and the text that says why a file shows nothing, which
/// are the config's bar background and foreground where the frontend has no colors of its own
/// for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub bar_background: [u8; 3],
    pub selection: [u8; 3],
    pub mark: [u8; 3],
    pub placeholder: [u8; 3],
    pub error: [u8; 3],
}

/// What the frontend must do, in the order given.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Draw the frame again with [`Session::draw`]. It comes at most once, last. With
    /// `bar_only`, only the status bar changed, which a frontend that shows the bar itself can
    /// redraw alone.
    Redraw {
        bar_only: bool,
    },
    /// Size the window to the image, kept inside the screen, unless the window is fullscreen:
    /// `image` in logical units, with `bar` logical units more below it for the bar.
    FitWindow {
        image: Size,
        bar: f64,
    },
    /// The window size the config starts with, for the grid, unless -g fixed the size or the
    /// window is fullscreen.
    ResetWindowSize,
    ToggleFullscreen,
    Beep,
    /// A line for stderr, to print where it cannot garble the screen.
    Warning(String),
    /// Watch this file, and only it, for changes, and call [`Session::file_changed`] when it
    /// is written or replaced; None stops watching.
    Watch(Option<PathBuf>),
    /// Hand the terminal or the screen to the user's key-handler, and call
    /// [`Session::run_key_handler`], which runs it and returns once it has ended. Call it
    /// before any other call into the session: the viewer keeps the files' places in its
    /// list for after the run, which another event could move.
    RunKeyHandler,
    /// These files changed, or were invalidated: drop anything kept of them, as frames drawn
    /// from them. An invalidated file is named again once its new image is in, before the
    /// redraw that shows it, since frames drawn while it decodes show the old one.
    Forget(Vec<PathBuf>),
    /// The file next to the current one is in, after [`Session::want_stills`]: its still can
    /// be drawn ahead, through [`Session::still`].
    Still(PathBuf),
    /// Print these paths to stdout, one per line, and exit with status 0.
    Quit(Vec<PathBuf>),
    /// Show a text field of the frontend's own over `rect`, in logical units from the top
    /// left of the canvas the session draws, holding `text`, in a bold sans-serif `font`
    /// logical units high, in `color`; it has the keys until Return or Escape ends it, then
    /// call [`Session::text_entered`] with its text. Only where [`EditSetup::field`] says the
    /// frontend has one.
    EditText {
        rect: Rect,
        text: String,
        font: f64,
        color: [u8; 3],
    },
    /// End the text field as Return would, and call [`Session::text_entered`] with its text.
    EndText,
    /// Put this PNG image on the pasteboard, in place of what it held.
    CopyImage(Vec<u8>),
    /// Put this text on the pasteboard, in place of what it held.
    CopyText(String),
}

/// What the threads behind the session answer.
enum Answer {
    Loaded(Loaded),
    /// A file decoded ahead, or found in the cache, with the stamp the Loader took before it
    /// read the pixels. Weak, so an answer not yet taken in keeps no image the cache dropped.
    Ahead {
        path: PathBuf,
        pyramid: Weak<Pyramid>,
        stamp: Stamp,
    },
    /// More frames of the animation started for this request.
    Frames {
        request: u64,
        first: usize,
        delays: Vec<f64>,
        complete: bool,
    },
    Thumbnail(Made),
    /// A recognition's answer, for the edit numbered `edit`.
    Recognized {
        edit: u64,
        job: u64,
        answer: nsxiv_ng_edit::Recognition,
    },
}

type Wake = Arc<dyn Fn() + Send + Sync>;

const TIMERS: [Timer; 3] = [Timer::Animation, Timer::Slideshow, Timer::Reload];

fn slot(timer: Timer) -> usize {
    match timer {
        Timer::Animation => 0,
        Timer::Slideshow => 1,
        Timer::Reload => 2,
    }
}

/// One running viewer.
pub struct Session {
    viewer: Viewer,
    loader: Loader,
    sender: Sender<Answer>,
    answers: Receiver<Answer>,
    wake: Wake,
    text: Text,
    max_image_bytes: u64,
    animation_budget: u64,
    /// The height of the bar the session draws, in logical units, once the fonts are in.
    bar_height: Option<f64>,
    /// A frame drawn now would be final, as [`Session::frame_is_final`] has it, and the
    /// redraw that says so went out.
    frame_was_final: bool,
    /// Whether the bar font's family was looked at once the fonts were in.
    font_told: bool,
    bar: Bar,
    quiet: bool,
    /// The window's content in logical units.
    window: Size,
    /// The window's device scale, which thumbnails are made for, once a resize gave it.
    scale: Option<f64>,
    pointer: pointer::Gesture,
    nav_width_fraction: f64,
    /// The zoom levels, more steps than which change nothing more.
    zoom_levels: u32,
    /// The image on screen and its turn, which stay while the next file decodes.
    shown: Option<Arc<Pyramid>>,
    turn: Turn,
    /// The animation of the image on screen, with the request it answers, and the frame it
    /// was told is on screen.
    animation: Option<(u64, Animation)>,
    frame_shown: usize,
    /// The animation frame drawn last, which stays while the one on screen is not decoded.
    drawn: Option<Arc<Image>>,
    /// What the last frame drawn showed, where a pan may move it.
    last_frame: Option<LastFrame>,
    /// When each timer fires, by [`slot`].
    deadlines: [Option<Instant>; 3],
    /// How long thumbnails that come in wait for a whole frame, as a terminal's frames cost an
    /// image each, and when the frame they wait for is due.
    thumbnail_wait: Option<Duration>,
    thumbnail_due: Option<Instant>,
    /// Each file's stamp when it was last shown or drawn ahead.
    stamps: HashMap<PathBuf, Stamp>,
    /// Files invalidated whose new image has yet to come in, which frames drawn meanwhile
    /// show as they were.
    invalidated: HashSet<PathBuf>,
    /// The neighbors' images, once [`Session::want_stills`] asked for them, while the cache
    /// keeps them.
    stills: HashMap<PathBuf, Weak<Pyramid>>,
    thumbs: Thumbs,
    key_handlers: Vec<PathBuf>,
    /// The key-handler run the frontend was asked to start: the handler, the key and the files.
    handler_run: Option<(PathBuf, String, Vec<PathBuf>)>,
    /// What the frontend does to the key-handler's command before it starts.
    prepare_handler: fn(&mut std::process::Command),
    /// The color table of the image on screen, for the gamma and contrast steps it was built
    /// for, which stays while the next file decodes, as the turn does.
    colors: Option<Colors>,
    colors_for: (i32, i32),
    background: u32,
    foreground: u32,
    bar_background: u32,
    /// The text of what went wrong with a file.
    error: u32,
    grid_colors: GridColors,
    /// The viewer's current file when an event last ended, and whether it went forward to it.
    last: usize,
    forward: bool,
    /// What a frontend that can edit gave, the edit under way, and how many edits began.
    edit_setup: Option<EditSetup>,
    editing: Option<edit::Editing>,
    edits: u64,
    /// A recognition runs, and those asked for since, which wait for it in turn.
    recognizing: bool,
    next_recognition: VecDeque<edit::Asked>,
    /// The fonts count as still loading, whatever they are.
    #[cfg(test)]
    hold_fonts: bool,
}

impl Session {
    /// A viewer of `files`, with its limits from `config` and the machine. `program` is the
    /// viewer's own binary, which decoding helpers run as. `key_handlers` are where to look
    /// for the key-handler, in order, at each run, as [`key_handlers`] gives them.
    /// `bar` says where the status bar shows.
    ///
    /// `wake` runs on any thread whenever an answer is in for [`Session::answers`], and may
    /// run after the session is gone. It must not block. Two wakes may come before one call
    /// of `answers`, so it must leave a mark that stays until `answers` is called, as a flag
    /// or a channel does, not a notification that a frontend not waiting for it would miss.
    pub fn new(
        options: Options,
        files: Vec<FileEntry>,
        config: &Config,
        program: PathBuf,
        key_handlers: Vec<PathBuf>,
        bar: Bar,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Session {
        let limits = Limits::new(config, physical_memory(), cores());
        let helpers = Arc::new(Helpers::new(
            program,
            limits.helpers,
            limits.max_image_bytes,
            HELPER_TIMEOUT,
        ));
        let wake: Wake = Arc::new(wake);
        let (sender, answers) = mpsc::channel();
        let decoding = Arc::clone(&helpers);
        let loader = Loader::with_decoder(
            limits.threads,
            limits.max_image_bytes,
            limits.cache_bytes,
            move |path, cap| decoding.decode(path, cap),
            answerer(&sender, &wake, Answer::Loaded),
        );
        // the grid loads no images, so its thumbnails take the Loader's share of the cores
        let thumbs = Thumbs::new(
            options.private_mode,
            thumbnails::BUDGET,
            limits.threads,
            Arc::clone(&helpers),
            limits.max_image_bytes,
            answerer(&sender, &wake, Answer::Thumbnail),
        );
        let ready = Arc::clone(&wake);
        let text = Text::new(&config.bar_font, config.bar_font_size, move || ready());
        let quiet = options.quiet;
        let viewer = Viewer::with_config(options, files, config);
        let color = |[r, g, b]: [u8; 3]| canvas::rgb(r, g, b);
        Session {
            last: viewer.current(),
            forward: true,
            viewer,
            loader,
            sender,
            answers,
            wake,
            text,
            max_image_bytes: limits.max_image_bytes,
            animation_budget: limits.animation_budget,
            bar_height: None,
            frame_was_final: false,
            font_told: false,
            bar,
            quiet,
            window: Size::default(),
            scale: None,
            pointer: pointer::Gesture::default(),
            nav_width_fraction: config.nav_width_fraction,
            // the viewport takes the defaults for a list it refuses
            zoom_levels: config.zoom_levels.len().max(ZOOM_LEVELS.len()) as u32,
            shown: None,
            turn: Turn::default(),
            animation: None,
            frame_shown: 0,
            drawn: None,
            last_frame: None,
            deadlines: [None; 3],
            thumbnail_wait: None,
            thumbnail_due: None,
            stamps: HashMap::new(),
            invalidated: HashSet::new(),
            stills: HashMap::new(),
            thumbs,
            key_handlers,
            handler_run: None,
            prepare_handler: |_| {},
            colors: None,
            colors_for: (0, 0),
            background: color(config.background),
            foreground: color(config.foreground),
            bar_background: color(config.bar_background),
            error: color(config.foreground),
            grid_colors: GridColors {
                placeholder: color(config.bar_background),
                selection: color(config.selection_color),
                mark: color(config.mark_color),
            },
            edit_setup: None,
            editing: None,
            edits: 0,
            recognizing: false,
            next_recognition: VecDeque::new(),
            #[cfg(test)]
            hold_fonts: false,
        }
    }

    pub fn viewer(&self) -> &Viewer {
        &self.viewer
    }

    /// Thumbnails that come in one by one wait up to `wait` for one whole frame, rather than
    /// a frame each, through [`Session::deadline`]; a redraw any other event causes is never
    /// delayed. Called once after `new`, as a terminal does, whose frames cost an image each.
    pub fn wait_for_thumbnails(&mut self, wait: Duration) {
        self.thumbnail_wait = Some(wait);
    }

    /// Asks for the stills of the files next to the current one, which a terminal draws ahead:
    /// [`Request::Still`] names each once its decode ahead ends. Called once after `new`; a
    /// frontend that does not call it gets no such request and no wake for them.
    pub fn want_stills(&mut self) {
        let (sender, wake) = (self.sender.clone(), Arc::clone(&self.wake));
        self.loader.tell_ahead(move |path, pyramid, stamp| {
            let ahead = Answer::Ahead {
                path: path.to_owned(),
                pyramid: Arc::downgrade(pyramid),
                stamp,
            };
            let _ = sender.send(ahead);
            wake();
        });
    }

    /// The still of `path`, a file next to the current one that [`Request::Still`] named, to
    /// draw on any thread as [`Session::draw`] would once the viewer moved to it, in the
    /// canvas the viewer has now: the view a new image gets, with the settings that last, as
    /// the zoom mode, antialiasing, the alpha layer, gamma and contrast. None once it is no
    /// neighbor of the current file, for a file not decoded ahead, and for one the cache of
    /// decoded images had no room to keep. After a resize, call it again for the files
    /// [`Request::Still`] named since the last move.
    pub fn still(&self, path: &Path) -> Option<Still> {
        if !self.ahead().iter().any(|neighbor| neighbor == path) {
            return None;
        }
        let pyramid = self.stills.get(path)?.upgrade()?;
        let mut view = self.viewer.viewport().clone();
        let size = Size {
            width: f64::from(pyramid.levels[0].width),
            height: f64::from(pyramid.levels[0].height),
        };
        // as a file the viewer moves to gets it
        view.set_image_size(size, true);
        Some(Still {
            pyramid,
            view,
            colors: Colors::new(self.viewer.gamma_value(), self.viewer.contrast_value()),
            background: self.background,
        })
    }

    /// The colors from now on, as a frontend that follows the system's appearance resolves
    /// them; a whole redraw follows.
    pub fn set_palette(&mut self, palette: Palette) -> Vec<Request> {
        let color = |[r, g, b]: [u8; 3]| canvas::rgb(r, g, b);
        self.background = color(palette.background);
        self.foreground = color(palette.foreground);
        self.bar_background = color(palette.bar_background);
        self.error = color(palette.error);
        self.grid_colors = GridColors {
            placeholder: color(palette.placeholder),
            selection: color(palette.selection),
            mark: color(palette.mark),
        };
        vec![Request::Redraw { bar_only: false }]
    }

    /// The height of a title bar the frontend draws above the canvas, in logical units: the
    /// status bar's, 0 while the fonts load. It changes once, when they are in; with
    /// [`Bar::Drawn`] a redraw follows then.
    pub fn title_height(&self) -> f64 {
        self.bar_height.unwrap_or(0.0)
    }

    /// Draws a title bar with `title` and the close button's `×` over all of `canvas`, in the
    /// status bar's font and colors, at `scale` pixels a logical unit.
    pub fn draw_title(&mut self, canvas: &mut Canvas, title: &str, scale: f64) {
        let colors = (self.bar_background, self.foreground);
        bar::title(canvas, &mut self.text, title, scale, colors);
    }

    /// Whether a frame drawn now is final, so a window shown with it has the size it keeps: the
    /// fonts have finished loading, whether or not one serves, and the answers have taken them
    /// in, so the bar's and a title bar's rows are known, and the viewer shows an image, the
    /// text of what went wrong, or the grid, so the window first shows at its size and with its
    /// image; a frontend shows nothing before this holds. Once it
    /// holds it holds for good; with [`Bar::Drawn`] the answers that make it hold come with a
    /// redraw.
    pub fn frame_is_final(&mut self) -> bool {
        let viewer = &self.viewer;
        let shows =
            viewer.thumbnail_mode() || viewer.image().is_some() || viewer.error_text().is_some();
        // fonts in that the answers have yet to take in leave the rows the window has unknown
        let fonts_in = !self.text.loading() && bar::height(&mut self.text) == self.bar_height;
        self.frame_was_final || (shows && fonts_in)
    }

    /// Once the window exists and its size is known through [`Session::resize`].
    pub fn start(&mut self) -> Vec<Request> {
        let effects = self.viewer.start();
        self.carry(effects)
    }

    /// The window's content is `window` in logical units, the bar included, at device
    /// `scale`, which thumbnails are made for.
    /// A scale that is not finite and above zero keeps the last one.
    pub fn resize(&mut self, window: Size, scale: f64) -> Vec<Request> {
        let before = self.canvas();
        self.window = window;
        if scale.is_finite() && scale > 0.0 {
            self.scale = Some(scale);
        }
        // the edit takes the canvas, and the viewer lays out for it once the edit ends
        if self.editing.is_some() {
            return self.edit_resized(before);
        }
        self.carry(vec![Effect::Layout])
    }

    /// A key that no binding names, as a menu shortcut or a key without a name, still ends a
    /// C-x prefix, without running the key-handler.
    pub fn end_prefix(&mut self) {
        self.viewer.end_prefix();
    }

    /// The requests of the key's binding, or None when no binding takes the key.
    pub fn key(&mut self, key: &Keystroke) -> Option<Vec<Request>> {
        if self.editing.is_some() {
            return self.edit_key(key);
        }
        let effects = self.viewer.key(key)?;
        Some(self.carry(effects))
    }

    /// When the next timer is due, if one is set. It changes with every call into the session.
    pub fn deadline(&self) -> Option<Instant> {
        // the viewer's timers wait while an edit is under way
        if let Some(editing) = &self.editing {
            return editing.edit.deadline();
        }
        let thumbnails = self.thumbnail_due.iter();
        self.deadlines
            .iter()
            .flatten()
            .chain(thumbnails)
            .min()
            .copied()
    }

    /// Fires the timers due at `now`, each once at most, the earliest first.
    pub fn timers(&mut self, now: Instant) -> Vec<Request> {
        if self.editing.is_some() {
            return one_redraw(self.edit_timers(now));
        }
        let mut due: Vec<(Instant, Timer)> = TIMERS
            .into_iter()
            .filter_map(|timer| Some((self.deadlines[slot(timer)]?, timer)))
            .filter(|&(at, _)| at <= now)
            .collect();
        due.sort_by_key(|&(at, _)| at);
        let mut requests = Vec::new();
        for (_, timer) in due {
            // one fired before it may have canceled it or set it again
            if self.deadlines[slot(timer)].is_some_and(|at| at <= now) {
                self.deadlines[slot(timer)] = None;
                let effects = self.viewer.timer(timer);
                requests.extend(self.carry(effects));
            }
        }
        if self.thumbnail_due.is_some_and(|at| at <= now) {
            self.thumbnail_due = None;
            requests.push(Request::Redraw { bar_only: false });
        }
        one_redraw(requests)
    }

    /// The file [`Request::Watch`] named was written or replaced.
    pub fn file_changed(&mut self) -> Vec<Request> {
        let effects = self.viewer.file_changed();
        self.carry(effects)
    }

    /// Has the key-handler's command readied by `prepare` before it starts, as a frontend
    /// whose own state the handler must not inherit asks: nothing is done to it by default.
    pub fn prepare_key_handler(&mut self, prepare: fn(&mut std::process::Command)) {
        self.prepare_handler = prepare;
    }

    /// After [`Request::RunKeyHandler`], on the frontend's thread between its own steps: runs
    /// the key-handler with the key, its files on its stdin, and waits for it to end. The
    /// files it may have changed, moved or deleted are then read again. Call it before any
    /// other call into the session once the request came, as the run belongs to the viewer's
    /// state at that moment.
    pub fn run_key_handler(&mut self) -> Vec<Request> {
        let Some((handler, key, files)) = self.handler_run.take() else {
            return Vec::new();
        };
        if let Err(why) = key_handler::run(&handler, &key, &files, self.prepare_handler) {
            if self.quiet {
                return Vec::new();
            }
            let warning = format!("{}: key-handler failed: {why}", handler.display());
            return vec![Request::Warning(warning)];
        }
        let effects = self.viewer.key_handler_done();
        self.carry(effects)
    }

    /// Puts `paths` in the list after the current file, in their order, and shows the first
    /// of them, as files opened from Finder while the viewer runs: the list and its marks
    /// stay. A path the list holds already goes in no second time, as Finder opens a file
    /// and not a name twice, and where it is the first its entry is shown. During an edit
    /// they go in and nothing new shows: the edit goes on, and they come next once it ends.
    pub fn open_after_current(&mut self, paths: Vec<PathBuf>) -> Vec<Request> {
        // in one pass over the list and one over the paths, as Finder may open thousands
        let held: HashSet<&Path> = (self.viewer.files().iter())
            .map(|f| f.path.as_path())
            .collect();
        let mut opened = HashSet::new();
        let mut new: Vec<PathBuf> = (paths.iter())
            .filter(|path| !held.contains(path.as_path()) && opened.insert(path.as_path()))
            .cloned()
            .collect();
        let mut effects = Vec::new();
        // an empty list takes the first as its current file, which loads
        if self.viewer.files().is_empty() && !new.is_empty() {
            effects.extend(self.viewer.insert_after_current(new.remove(0)));
        }
        effects.extend(self.viewer.insert_after(new));
        let current = self.viewer.current();
        let first = (paths.first())
            .and_then(|path| self.viewer.files().iter().position(|f| f.path == *path));
        if let Some(first) = first.filter(|&i| i != current && !self.editing()) {
            effects.extend(self.viewer.navigate(first as isize - current as isize));
        }
        one_redraw(self.carry(effects))
    }

    /// Takes in the answers that came since the last call, after `wake`.
    pub fn answers(&mut self) -> Vec<Request> {
        let mut effects = Vec::new();
        // what goes before the effects' requests, and what an edit asks after them
        let mut first = Vec::new();
        let mut edited = Vec::new();
        while let Ok(answer) = self.answers.try_recv() {
            match answer {
                Answer::Loaded(loaded) => self.loaded(loaded, &mut effects, &mut first),
                Answer::Ahead {
                    path,
                    pyramid,
                    stamp,
                } => {
                    // one the viewer has moved past is no neighbor any more, and one the cache
                    // dropped since is no news at all
                    if self.ahead().contains(&path) && pyramid.strong_count() > 0 {
                        if self.changed(&path, Some(stamp)) {
                            first.push(Request::Forget(vec![path.clone()]));
                        }
                        // the cache keeps it, which may drop it again for room
                        self.stills.insert(path.clone(), pyramid);
                        first.push(Request::Still(path));
                    }
                }
                Answer::Frames {
                    request,
                    first,
                    delays,
                    complete,
                } => {
                    // progress of an animation the viewer left changes nothing there
                    let fx = self
                        .viewer
                        .frames_decoded(request, first, &delays, complete);
                    effects.extend(fx);
                }
                Answer::Recognized { edit, job, answer } => {
                    edited.extend(self.recognized(edit, job, answer));
                }
                Answer::Thumbnail(made) => {
                    let viewer = &self.viewer;
                    // only a store past its budget needs the cells in view
                    let visible = || in_view(viewer).into_iter().collect::<HashSet<_>>();
                    // many in at once give one redraw, and where they wait, one in a while
                    if self.thumbs.made(made, visible) {
                        match self.thumbnail_wait {
                            Some(wait) if self.viewer.thumbnail_mode() => {
                                let due = Instant::now().checked_add(wait);
                                self.thumbnail_due = self.thumbnail_due.or(due);
                            }
                            _ => effects.push(Effect::Redraw(Part::Grid)),
                        }
                    }
                }
            }
        }
        // the bar's height is known once the fonts are in, and the canvas changes with it
        let bar_height = bar::height(&mut self.text);
        if bar_height != self.bar_height {
            // only a bar the session draws, and shows, takes room by its font
            let before = self.bar();
            self.bar_height = bar_height;
            if self.bar() != before {
                effects.push(Effect::Layout);
                if self.bar() > 0.0 {
                    effects.push(Effect::FitWindow);
                }
            } else if self.bar == Bar::Drawn {
                // a hidden bar leaves the canvas as it was, so no layout, which would scroll the
                // grid back to the selection; the frame, final now the fonts are in, is redrawn
                effects.push(Effect::Redraw(Part::Image));
            } else if !self.viewer.thumbnail_mode() && self.viewer.error_text().is_some() {
                // the text of what went wrong is all that needs the fonts there
                effects.push(Effect::Redraw(Part::Image));
            }
        }
        // a bar font asked for by a name no installed family has, told once the fonts are in
        if !self.font_told && !self.text.loading() {
            self.font_told = true;
            if let Some(used) = self.text.substitute().map(str::to_owned)
                && !self.quiet
            {
                first.push(Request::Warning(format!(
                    "bar_font: no installed font family is named \"{}\"; {used} is used instead",
                    self.text.family()
                )));
            }
        }
        // the first final frame, which a frontend waits for to show its window, comes with a
        // redraw: the fonts' layout asks for one, and with no font installed this does
        if !self.frame_was_final && self.frame_is_final() {
            self.frame_was_final = true;
            if self.bar_height.is_none() {
                effects.push(Effect::Redraw(Part::Image));
            }
        }
        first.extend(self.carry(effects));
        first.extend(edited);
        first.extend(self.edit_fonts_in());
        one_redraw(first)
    }

    // Whether the file changed since it was last shown or drawn ahead, as its stamp tells, or
    // was invalidated and its new image is this one; the stamp is kept for the next time. A
    // file known before that can no longer be read has changed.
    fn changed(&mut self, path: &Path, stamp: Option<Stamp>) -> bool {
        let invalidated = self.invalidated.remove(path);
        let changed = match stamp {
            Some(stamp) => self
                .stamps
                .insert(path.to_owned(), stamp)
                .is_some_and(|old| old != stamp),
            None => self.stamps.remove(path).is_some(),
        };
        invalidated || changed
    }

    // An answer from the Loader, which the viewer takes when it still waits for it: its
    // effects, and before them a warning for a file that could not be read and word of one
    // that changed since it was shown.
    fn loaded(&mut self, loaded: Loaded, effects: &mut Vec<Effect>, first: &mut Vec<Request>) {
        let Loaded {
            request,
            pyramid,
            stamp,
        } = loaded;
        // an answer the viewer moved past changes nothing
        if self.viewer.awaited() != Some(request) {
            return;
        }
        let result = match &pyramid {
            Ok(p) => Ok(ImageInfo {
                size: Size {
                    width: f64::from(p.levels[0].width),
                    height: f64::from(p.levels[0].height),
                },
                delays: vec![0.1],
                // an animation's frames after the first come through its progress
                complete: !p.levels[0].may_animate,
            }),
            Err(DecodeError::TooLarge { size }) => {
                Err(LoadError::TooLarge(size.map(|(width, height)| Size {
                    width: f64::from(width),
                    height: f64::from(height),
                })))
            }
            Err(DecodeError::Unreadable(_)) => Err(LoadError::Unreadable),
            Err(DecodeError::NoPreview) => Err(LoadError::NoPreview),
        };
        effects.extend(self.viewer.loaded(request, result));
        let Some(path) = self.viewer.current_entry().map(|e| e.path.clone()) else {
            return;
        };
        self.animation = None;
        self.drawn = None;
        // the reason, which the screen does not show, as a warning
        if let Err(why @ (DecodeError::Unreadable(_) | DecodeError::NoPreview)) = &pyramid
            && !self.quiet
        {
            first.push(Request::Warning(format!("{}: {why}", path.display())));
        }
        if self.changed(&path, stamp) {
            first.push(Request::Forget(vec![path.clone()]));
        }
        if let Ok(p) = &pyramid
            && p.levels[0].may_animate
        {
            let (sender, wake) = (self.sender.clone(), Arc::clone(&self.wake));
            let progress = move |first, delays: &[f64], complete| {
                let delays = delays.to_vec();
                let frames = Answer::Frames {
                    request,
                    first,
                    delays,
                    complete,
                };
                let _ = sender.send(frames);
                wake();
            };
            let budget = self.animation_budget;
            let animation = Animation::start(path, budget, self.max_image_bytes, progress);
            self.animation = Some((request, animation));
            self.frame_shown = 0;
        }
        self.shown = pyramid.ok();
    }

    /// Draws the frame onto `canvas` at `scale` pixels a logical unit: the background or a
    /// clear canvas, then the image or the grid in the area above the bar, or the text of what
    /// went wrong in place of the image, then the bar where the session draws it. The canvas is
    /// the window, or with [`Bar::Shown`] the part above the frontend's bar. The scale is the
    /// frame's own, which may differ from the window's, as for a terminal that scales an image
    /// itself. True when every pixel came out opaque.
    pub fn draw(&mut self, canvas: &mut Canvas, scale: f64, frame: Frame) -> bool {
        // a frame drawn otherwise than below leaves none a pan may move
        let last = self.last_frame.take();
        if self.draw_edit(canvas, scale) {
            return true;
        }
        let width = canvas.width();
        let bar = self.bar_rows(canvas.height(), scale);
        let main = canvas.height() - bar;
        if frame.kept
            && !frame.clear
            && !frame.whole
            && let Some(last) = last
            && self.move_last(canvas, scale, main, last)
        {
            canvas.pixels_mut()[width as usize * main as usize..].fill(self.background);
            self.draw_status(canvas, scale, main, bar);
            return true;
        }
        let viewer = &self.viewer;
        // the text of what went wrong keeps the background it is drawn for
        let clear = frame.clear && (viewer.thumbnail_mode() || viewer.error_text().is_none());
        let background = if clear { 0 } else { self.background };
        canvas.pixels_mut().fill(background);
        let mut covered = false;
        {
            // rows top to bottom with no padding, so the area above the bar is a prefix
            let pixels = &mut canvas.pixels_mut()[..width as usize * main as usize];
            let mut area = Canvas::new(pixels, width, main).expect("whole rows of the frame");
            if viewer.thumbnail_mode() {
                let visible = viewer.grid().visible();
                // each thumbnail's size and its one level
                type Thumb<'a> = (u32, u32, [&'a [u8]; 1]);
                let thumbs: Vec<Option<Thumb>> = visible
                    .clone()
                    .map(|i| {
                        let thumb = self.thumbs.get(&viewer.files().get(i)?.path)?;
                        Some((thumb.width, thumb.height, [&thumb.rgba[..]]))
                    })
                    .collect();
                let thumbnail = |i: usize| {
                    let (width, height, levels) =
                        thumbs.get(i.checked_sub(visible.start)?)?.as_ref()?;
                    Some(image_view::Image {
                        width: *width,
                        height: *height,
                        levels,
                        // only a claim that lets drawing skip work; opaque pixels cover anyway
                        opaque: false,
                    })
                };
                // a thumbnail drawn before at this size over this background is copied, and one
                // drawn anew taken from the frame where the budget has room to keep it
                let kept = |i: usize, size| {
                    let path = &viewer.files().get(i)?.path;
                    self.thumbs.cell(path, size, background)
                };
                let mut in_view = thumbnails::InView::new(|| {
                    let files = viewer.files();
                    let paths = visible
                        .clone()
                        .filter_map(|i| Some(files.get(i)?.path.as_path()));
                    paths.collect()
                });
                let room = |i: usize, size| {
                    let Some(entry) = viewer.files().get(i) else {
                        return false;
                    };
                    self.thumbs.room(&entry.path, size, &mut in_view)
                };
                let colors = self.grid_colors;
                let drawn =
                    grid_view::draw_kept(&mut area, viewer, scale, colors, thumbnail, kept, room);
                for cell in drawn {
                    let Some(entry) = viewer.files().get(cell.index) else {
                        continue;
                    };
                    let (size, pixels) = (cell.size, cell.pixels);
                    self.thumbs
                        .keep(&entry.path, size, background, pixels, &mut in_view);
                }
            } else if let Some(message) = viewer.error_text() {
                let whole = [0, 0, width as i32, main as i32];
                bar::error(&mut area, &mut self.text, message, whole, scale, self.error);
            } else if let Some(pyramid) = &self.shown {
                if let Some((_, animation)) = &self.animation
                    && viewer.image().is_some_and(ImageInfo::is_animated)
                    && let Some(frame) = animation.frame(viewer.frame_index())
                {
                    self.drawn = Some(frame);
                }
                let view = viewer.viewport();
                let colors = self.colors.as_ref();
                covered = match &self.drawn {
                    Some(drawn) => {
                        let image = image_view::Image {
                            width: drawn.width,
                            height: drawn.height,
                            levels: &[&drawn.rgba[..]],
                            opaque: false,
                        };
                        let turn = self.turn;
                        image_view::draw(&mut area, &image, view, turn, colors, scale, frame.whole)
                    }
                    None => {
                        let turn = self.turn;
                        let covered =
                            still::draw(&mut area, pyramid, view, turn, colors, scale, frame.whole);
                        // an opaque still that covers the area, which a pan may move
                        let levels: Vec<&[u8]> =
                            pyramid.levels.iter().map(|l| &l.rgba[..]).collect();
                        let image = image_view::Image {
                            width: pyramid.levels[0].width,
                            height: pyramid.levels[0].height,
                            levels: &levels,
                            opaque: pyramid.opaque,
                        };
                        let animated = viewer.image().is_some_and(ImageInfo::is_animated);
                        let placed =
                            image_view::placement((width, main), &image, view, turn, scale);
                        if let Some(placed) = placed
                            && covered
                            && pyramid.opaque
                            && !animated
                            && !frame.whole
                            && !frame.clear
                        {
                            self.last_frame = Some(LastFrame {
                                image: Arc::downgrade(pyramid),
                                view: view.clone(),
                                scale,
                                area: (width, main),
                                origin: placed.origin,
                                turn,
                                colors: self.colors_for,
                            });
                        }
                        covered
                    }
                };
            }
        }
        self.draw_status(canvas, scale, main, bar);
        covered || !clear
    }

    // The status bar over the `bar` rows below the `main` ones, where it has any.
    fn draw_status(&mut self, canvas: &mut Canvas, scale: f64, main: u32, bar: u32) {
        if bar == 0 {
            return;
        }
        let (left, right) = self.viewer.status_text();
        bar::status(
            canvas,
            &mut self.text,
            [0, main as i32, canvas.width() as i32, bar as i32],
            scale,
            (&left, &right),
            (self.bar_background, self.foreground),
        );
    }

    /// Whether a frame of `width` × `height` pixels drawn now at `scale` over the last frame,
    /// which its canvas would hold, would move that frame rather than draw it whole: a
    /// frontend that keeps its last frame elsewhere copies it into the canvas only then.
    pub fn would_move(&self, (width, height): (u32, u32), scale: f64) -> bool {
        let main = height - self.bar_rows(height, scale);
        self.last_frame
            .as_ref()
            .is_some_and(|last| self.moves(last, width, main, scale).is_some())
    }

    // Where the frame `last` shows, which the canvas holds, may be moved: the image is the
    // same opaque still under the same view but for its place, it covers the area, and every
    // strip that came into view comes out as the whole frame drawn would give it. The image's
    // place now, how far it moved, and the strips.
    fn moves(&self, last: &LastFrame, width: u32, main: u32, scale: f64) -> Option<Move> {
        let viewer = &self.viewer;
        let pyramid = self.shown.as_ref()?;
        if viewer.thumbnail_mode()
            || viewer.error_text().is_some()
            || self.drawn.is_some()
            || viewer.image().is_some_and(ImageInfo::is_animated)
            || !Weak::ptr_eq(&last.image, &Arc::downgrade(pyramid))
        {
            return None;
        }
        let view = viewer.viewport();
        let same = last.view.zoom() == view.zoom()
            && last.view.view_size() == view.view_size()
            && last.view.antialias == view.antialias
            && last.view.alpha_layer == view.alpha_layer
            && last.scale == scale
            && last.area == (width, main)
            && last.turn == self.turn
            && last.colors == self.colors_for;
        if !same {
            return None;
        }
        let levels: Vec<&[u8]> = pyramid.levels.iter().map(|l| &l.rgba[..]).collect();
        let image = image_view::Image {
            width: pyramid.levels[0].width,
            height: pyramid.levels[0].height,
            levels: &levels,
            opaque: pyramid.opaque,
        };
        let (w, h) = (i64::from(width), i64::from(main));
        let placed = image_view::placement((width, main), &image, view, self.turn, scale)?;
        let by = (
            placed.origin.0 - last.origin.0,
            placed.origin.1 - last.origin.1,
        );
        let (dx, dy) = by;
        if !placed.covers(&image, view, (w, h)) || dx.abs() >= w || dy.abs() >= h {
            return None;
        }
        // the rows that came into view, the whole width, and the columns, the other rows
        let rows = if dy > 0 {
            (0, 0, w, dy)
        } else {
            (0, h + dy, w, -dy)
        };
        let columns = if dx > 0 {
            (0, dy.max(0), dx, h - dy.abs())
        } else {
            (w + dx, dy.max(0), -dx, h - dy.abs())
        };
        let strips: Vec<_> = [rows, columns]
            .into_iter()
            .filter(|&(_, _, sw, sh)| sw > 0 && sh > 0)
            .collect();
        let exact = |strip: &(i64, i64, i64, i64)| {
            image_view::clip_draws_as_whole((width, main), &image, view, self.turn, scale, *strip)
        };
        // the part kept was drawn with the last view, so it must round as the whole frame does
        let kept =
            image_view::wholes_agree((width, main), &image, &last.view, view, self.turn, scale);
        (kept && strips.iter().all(exact)).then_some(Move {
            origin: placed.origin,
            by,
            strips,
        })
    }

    // Moves the frame `last` shows, which the canvas holds, as [`Session::moves`] finds it
    // may, and draws the strips that came into view. False, having drawn nothing, where it
    // may not.
    fn move_last(&mut self, canvas: &mut Canvas, scale: f64, main: u32, last: LastFrame) -> bool {
        let width = canvas.width();
        let Some(Move {
            origin,
            by: (dx, dy),
            strips,
        }) = self.moves(&last, width, main, scale)
        else {
            return false;
        };
        let Some(pyramid) = &self.shown else {
            return false;
        };
        let (w, h) = (i64::from(width), i64::from(main));
        let pixels = &mut canvas.pixels_mut()[..width as usize * main as usize];
        let (from_x, to_x, length) = if dx >= 0 {
            (0, dx, w - dx)
        } else {
            (-dx, 0, w + dx)
        };
        let row = |to: i64| {
            let from = (to - dy) * w + from_x;
            (
                from as usize..(from + length) as usize,
                (to * w + to_x) as usize,
            )
        };
        // rows moving down go last first, so none is overwritten before it moves
        if dy > 0 {
            for to in (dy..h).rev() {
                let (from, to) = row(to);
                pixels.copy_within(from, to);
            }
        } else {
            for to in 0..h + dy {
                let (from, to) = row(to);
                pixels.copy_within(from, to);
            }
        }
        let levels: Vec<&[u8]> = pyramid.levels.iter().map(|l| &l.rgba[..]).collect();
        let image = image_view::Image {
            width: pyramid.levels[0].width,
            height: pyramid.levels[0].height,
            levels: &levels,
            opaque: pyramid.opaque,
        };
        let view = self.viewer.viewport();
        let mut area = Canvas::new(pixels, width, main).expect("whole rows of the frame");
        let colors = self.colors.as_ref();
        for strip in strips {
            image_view::draw_clipped(&mut area, &image, view, self.turn, colors, scale, strip);
        }
        self.last_frame = Some(LastFrame {
            view: view.clone(),
            origin,
            ..last
        });
        true
    }

    /// The rows of pixels at the bottom of a frame `height` rows high at `scale` that the bar
    /// [`Session::draw`] draws takes: none with [`Bar::Shown`], while the bar hides, or while
    /// its fonts load. A frontend that redraws the bar alone draws these rows.
    pub fn bar_rows(&self, height: u32, scale: f64) -> u32 {
        match self.bar {
            Bar::Drawn => ((self.bar() * scale).round() as u32).min(height),
            Bar::Shown(_) => 0,
        }
    }

    /// Draws the status bar alone over all of `canvas`, as [`Session::draw`] draws it in the
    /// rows [`Session::bar_rows`] gives, for a [`Request::Redraw`] of the bar alone, which
    /// leaves the rest of the frame as it was.
    pub fn draw_bar(&mut self, canvas: &mut Canvas, scale: f64) {
        if self.draw_edit_bar(canvas, scale) {
            return;
        }
        let (width, height) = (canvas.width() as i32, canvas.height() as i32);
        let (left, right) = self.viewer.status_text();
        bar::status(
            canvas,
            &mut self.text,
            [0, 0, width, height],
            scale,
            (&left, &right),
            (self.bar_background, self.foreground),
        );
    }

    /// The room the bar takes at the bottom of the window, in logical units: none while it
    /// hides, or while the fonts of a bar the session draws load. An edit's status line shows
    /// in it whether or not the viewer's bar does.
    pub fn bar(&self) -> f64 {
        match self.bar {
            _ if !self.viewer.bar_visible() && self.editing.is_none() => 0.0,
            Bar::Drawn => self.bar_height.unwrap_or(0.0),
            Bar::Shown(height) => height,
        }
    }

    /// The canvas the viewer lays out for, in logical units.
    fn canvas(&self) -> Size {
        Size {
            width: self.window.width,
            height: (self.window.height - self.bar()).max(0.0),
        }
    }

    // Carries out the viewer's effects in order, and gives what is left to the frontend.
    fn carry(&mut self, effects: Vec<Effect>) -> Vec<Request> {
        self.follow();
        let mut requests = Vec::new();
        let mut redraw = None;
        let mut queue = VecDeque::from(effects);
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::Load { request, path } => {
                    self.loader.load(request, path);
                    let ahead = self.ahead();
                    self.stills.retain(|path, _| ahead.contains(path));
                    self.loader.prefetch(ahead);
                }
                Effect::Redraw(part) => {
                    // marks show in the grid alone
                    let bar_only = match part {
                        Part::Image | Part::View => false,
                        Part::Grid => !self.viewer.thumbnail_mode(),
                        Part::Bar => true,
                    };
                    redraw = Some(redraw.unwrap_or(true) && bar_only);
                }
                Effect::Layout => {
                    // what the new canvas causes comes before the effects after it
                    for effect in self.viewer.resized(self.canvas()).into_iter().rev() {
                        queue.push_front(effect);
                    }
                }
                // the window keeps its size while an edit is under way
                Effect::FitWindow if self.editing.is_some() => {}
                Effect::FitWindow => {
                    if let Some(image) = self.viewer.fitted_size() {
                        let bar = self.bar();
                        requests.push(Request::FitWindow { image, bar });
                    }
                }
                Effect::ResetWindowSize => requests.push(Request::ResetWindowSize),
                Effect::ToggleFullscreen => requests.push(Request::ToggleFullscreen),
                Effect::Beep => requests.push(Request::Beep),
                Effect::Quit { print } => requests.push(Request::Quit(print)),
                Effect::InvalidateThumbnails(paths) => {
                    self.loader.invalidate(&paths);
                    self.thumbs.forget(&paths);
                    for path in &paths {
                        // the next load or decode ahead is the file as it is now, and frames
                        // drawn before it comes in show the old one
                        self.stamps.remove(path);
                        self.stills.remove(path);
                        self.invalidated.insert(path.clone());
                    }
                    redraw = Some(false);
                    if !paths.is_empty() {
                        requests.push(Request::Forget(paths));
                    }
                }
                Effect::Timer(timer, seconds) => {
                    // a delay too long for the clock never comes
                    self.deadlines[slot(timer)] = Duration::try_from_secs_f64(seconds.max(0.0))
                        .ok()
                        .and_then(|delay| Instant::now().checked_add(delay));
                }
                Effect::CancelTimer(timer) => self.deadlines[slot(timer)] = None,
                Effect::Watch(path) => requests.push(Request::Watch(path)),
                Effect::RunKeyHandler { key, files } => {
                    // decided here, before the frontend hands its screen over for the run
                    let (files, left_out) = key_handler::passable(files);
                    if !self.quiet {
                        for file in &left_out {
                            requests.push(Request::Warning(format!(
                                "{}: not passed to the key-handler, as its name has a newline",
                                file.display()
                            )));
                        }
                    }
                    // with nothing to pass the handler does not run, as with no file at all; the
                    // viewer keeps nothing that waits for the run's end
                    if files.is_empty() {
                        continue;
                    }
                    match key_handler::find(&self.key_handlers) {
                        Some(handler) => {
                            self.handler_run = Some((handler.to_owned(), key, files));
                            requests.push(Request::RunKeyHandler);
                        }
                        None if self.quiet => {}
                        None => {
                            let warning = match self.key_handlers.first() {
                                Some(path) => {
                                    format!("no executable key-handler at {}", path.display())
                                }
                                None => "no executable key-handler".to_owned(),
                            };
                            requests.push(Request::Warning(warning));
                        }
                    }
                }
                Effect::EnterEditMode => {
                    if self.enter_edit(&mut requests) {
                        redraw = Some(false);
                        // the slideshow stops and an animation pauses for good
                        for effect in self.viewer.edit_entered().into_iter().rev() {
                            queue.push_front(effect);
                        }
                    }
                }
            }
        }
        self.want_thumbnails();
        if let Some(bar_only) = redraw {
            // a whole frame shows the thumbnails waiting for one too
            if !bar_only {
                self.thumbnail_due = None;
            }
            requests.push(Request::Redraw { bar_only });
        }
        requests
    }

    // Keeps up with the viewer after an event: the way it last moved, the turn and the colors of
    // the image on screen, which stay while the next file decodes, as the viewer has reset its
    // turn already and applies a new gamma once it is in, and the animation of the file on
    // screen, gone once the viewer leaves it.
    fn follow(&mut self) {
        let current = self.viewer.current();
        if current != self.last {
            self.forward = current > self.last;
            self.last = current;
        }
        if self.viewer.image().is_some() {
            let (flip_h, flip_v) = self.viewer.flips();
            self.turn = Turn {
                rotation: self.viewer.rotation(),
                flip_h,
                flip_v,
            };
            let steps = (self.viewer.gamma(), self.viewer.contrast());
            if steps != self.colors_for {
                let (gamma, contrast) = (self.viewer.gamma_value(), self.viewer.contrast_value());
                self.colors = Colors::new(gamma, contrast);
                self.colors_for = steps;
            }
        }
        if self.viewer.image().is_none() || self.viewer.thumbnail_mode() {
            self.animation = None;
        }
        if let Some((_, animation)) = &self.animation
            && self.viewer.frame_index() != self.frame_shown
        {
            self.frame_shown = self.viewer.frame_index();
            animation.show(self.frame_shown);
        }
    }

    // Asks for the thumbnails the grid shows.
    fn want_thumbnails(&mut self) {
        // none before the window's scale is known, which would make them again
        let Some(scale) = self.scale else {
            return;
        };
        // the largest cell the grid can draw, so zooming finds the same thumbnail
        let side = (self.viewer.grid().max_thumb_size() * scale).ceil() as u32;
        self.thumbs.want(&wanted(&self.viewer), side);
    }

    /// The files to decode ahead: the current file's neighbor in the direction the viewer
    /// last went, then the other.
    fn ahead(&self) -> Vec<PathBuf> {
        let current = self.viewer.current();
        let files = self.viewer.files();
        let next = files.get(current + 1);
        let previous = current.checked_sub(1).and_then(|i| files.get(i));
        let (first, second) = if self.forward {
            (next, previous)
        } else {
            (previous, next)
        };
        first
            .into_iter()
            .chain(second)
            .map(|f| f.path.clone())
            .collect()
    }
}

// The files whose thumbnails the grid shows, in order; none outside the grid.
fn wanted(viewer: &Viewer) -> Vec<&Path> {
    if viewer.thumbnail_mode() {
        in_view(viewer)
    } else {
        Vec::new()
    }
}

// The files of the grid's cells in view and the row below it, in order, whatever the mode.
fn in_view(viewer: &Viewer) -> Vec<&Path> {
    let files = viewer.files();
    let visible = viewer.grid().visible();
    visible
        .filter_map(|i| Some(files.get(i)?.path.as_path()))
        .collect()
}

// What a worker calls with its answer: the answer into the channel, then the wake.
fn answerer<T: 'static>(
    sender: &Sender<Answer>,
    wake: &Wake,
    wrap: fn(T) -> Answer,
) -> impl Fn(T) + Send + Sync + 'static {
    let (sender, wake) = (sender.clone(), Arc::clone(wake));
    move |answer| {
        // a session gone takes its channel with it, and nobody waits
        let _ = sender.send(wrap(answer));
        wake();
    }
}

// The requests with their redraws as one, last, of the bar alone only where each was.
fn one_redraw(mut requests: Vec<Request>) -> Vec<Request> {
    let mut redraw = None;
    requests.retain(|r| match r {
        Request::Redraw { bar_only } => {
            redraw = Some(redraw.unwrap_or(true) && *bar_only);
            false
        }
        _ => true,
    });
    if let Some(bar_only) = redraw {
        requests.push(Request::Redraw { bar_only });
    }
    requests
}

/// What the config and the machine allow.
#[derive(Debug, PartialEq)]
struct Limits {
    max_image_bytes: u64,
    cache_bytes: u64,
    animation_budget: u64,
    /// The Loader's workers, and the thumbnail makers'.
    threads: usize,
    /// The decoding helpers at most.
    helpers: usize,
}

impl Limits {
    fn new(config: &Config, memory: u64, cores: usize) -> Limits {
        let share = |percent: f64| (memory as f64 * percent / 100.0) as u64;
        Limits {
            max_image_bytes: share(config.max_image_percent),
            cache_bytes: share(config.image_cache_percent)
                .min(config.image_cache_max_mib.saturating_mul(1 << 20)),
            animation_budget: config.animation_budget_mib.saturating_mul(1 << 20),
            // one core for the frontend; the Loader keeps one of at least two free for the
            // file the viewer waits for
            threads: cores.saturating_sub(1).max(2),
            helpers: cores.clamp(2, 4),
        }
    }
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Physical memory in bytes, from `/proc/meminfo` on Linux and `sysctl` on macOS, or 4 GiB
/// where that cannot be read.
fn physical_memory() -> u64 {
    #[cfg(target_os = "linux")]
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| mem_total(&text));
    #[cfg(target_os = "macos")]
    let memory = hw_memsize();
    memory.unwrap_or(4 << 30)
}

/// The Mac's physical memory in bytes, as `sysctl -n hw.memsize` prints it.
#[cfg(target_os = "macos")]
fn hw_memsize() -> Option<u64> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kib.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn memory_comes_from_meminfo() {
        let meminfo = "MemTotal:       16314112 kB\nMemFree:         1234 kB\n";
        assert_eq!(mem_total(meminfo), Some(16314112 * 1024));
        assert_eq!(mem_total("MemFree: 1234 kB\n"), None);
        assert_eq!(mem_total("MemTotal: many kB\n"), None);
        assert!(physical_memory() > 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn memory_comes_from_sysctl() {
        let memory = hw_memsize().expect("sysctl reads hw.memsize");
        assert!(memory > 0);
        assert_eq!(physical_memory(), memory);
    }

    #[test]
    fn limits_follow_the_config_and_the_machine() {
        let gib = 1 << 30;
        let limits = Limits::new(&Config::default(), 16 * gib, 8);
        assert_eq!(
            limits,
            Limits {
                max_image_bytes: 4 * gib,
                cache_bytes: 256 << 20,
                animation_budget: 256 << 20,
                threads: 7,
                helpers: 4,
            },
            "a quarter of memory for an image, and the cache's 3% capped at 256 MiB"
        );
        let small = Limits::new(&Config::default(), 4 * gib, 1);
        assert_eq!(
            (small.cache_bytes, small.threads, small.helpers),
            ((4 * gib) * 3 / 100, 2, 2),
            "3% under the cap, and two workers and two helpers at least"
        );
    }

    fn files(n: usize) -> Vec<FileEntry> {
        (0..n)
            .map(|i| FileEntry::new(PathBuf::from(format!("/nonexistent/{i}.png"))))
            .collect()
    }

    #[test]
    fn decoding_ahead_takes_the_direction_of_travel_first() {
        let session = |keys: &[&str]| {
            let options = Options {
                start_at: 3,
                ..Options::default()
            };
            let program = PathBuf::from("nsxiv-ng");
            let config = Config::default();
            let mut session = Session::new(
                options,
                files(5),
                &config,
                program,
                vec![],
                Bar::Drawn,
                || {},
            );
            session.start();
            for key in keys {
                session.key(&Keystroke::new(key)).expect("a bound key");
            }
            let ahead = session.ahead();
            ahead
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            session(&[]),
            ["/nonexistent/3.png", "/nonexistent/1.png"],
            "forward at first"
        );
        assert_eq!(
            session(&["k"]),
            ["/nonexistent/0.png", "/nonexistent/2.png"]
        );
        assert_eq!(
            session(&["k", "r"]),
            ["/nonexistent/0.png", "/nonexistent/2.png"],
            "a reload keeps the direction"
        );
        assert_eq!(
            session(&["G"]),
            ["/nonexistent/3.png"],
            "the last file has one neighbor"
        );
        // in the grid, with no window yet, a column wide: down to 3, then back up to 2
        assert_eq!(
            session(&["Return", "j", "k", "Return"]),
            ["/nonexistent/1.png", "/nonexistent/3.png"],
            "a move that loads nothing still counts"
        );
    }

    #[test]
    fn news_of_an_image_the_cache_dropped_is_none() {
        let fixture = |name: &str| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/fixtures")
                .join(name)
        };
        let (photo, checker) = (fixture("photo.jpg"), fixture("checker.png"));
        let files = [&photo, &checker].map(|path| FileEntry::new(path.clone()));
        let (sender, wakes) = mpsc::channel();
        let mut session = Session::new(
            Options::default(),
            files.to_vec(),
            &Config::default(),
            PathBuf::from("nsxiv-ng"),
            vec![],
            Bar::Shown(16.0),
            move || {
                let _ = sender.send(());
            },
        );
        session.want_stills();
        let window = Size {
            width: 256.0,
            height: 192.0,
        };
        session.resize(window, 1.0);
        session.start();
        // the photo shown, the checker's still in, and the fonts
        while session.viewer().image().is_none()
            || session.still(&checker).is_none()
            || session.title_height() == 0.0
        {
            wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
            session.answers();
        }
        let known = session.stamps[&checker];
        // a stamp that differs, as of a rewrite, for an image already gone
        let other = session.stamps[&photo];
        assert_ne!(known, other);
        let gone = Answer::Ahead {
            path: checker.clone(),
            pyramid: Weak::new(),
            stamp: other,
        };
        session.sender.send(gone).unwrap();
        assert_eq!(session.answers(), [], "no Forget and no Still");
        assert_eq!(session.stamps[&checker], known, "the stamp as it was");
    }

    #[test]
    fn thumbnails_wait_only_in_the_grid() {
        let options = Options {
            thumbnail_mode: true,
            private_mode: true,
            ..Options::default()
        };
        let program = PathBuf::from("nsxiv-ng");
        let config = Config::default();
        let (sender, wakes) = mpsc::channel();
        let mut session = Session::new(
            options,
            files(2),
            &config,
            program,
            vec![],
            Bar::Shown(16.0),
            move || {
                let _ = sender.send(());
            },
        );
        session.wait_for_thumbnails(Duration::from_secs(60));
        let window = Size {
            width: 256.0,
            height: 192.0,
        };
        session.resize(window, 1.0);
        session.start();
        let path = PathBuf::from("/nonexistent/0.png");
        let made = Answer::Thumbnail(session.thumbs.made_now(&path));
        session.sender.send(made).unwrap();
        let requests = session.answers();
        assert!(
            !requests.iter().any(|r| matches!(r, Request::Redraw { .. })),
            "{requests:?}"
        );
        assert!(session.deadline().is_some(), "in the grid it waits");
        session.key(&Keystroke::new("Return"));
        assert_eq!(session.deadline(), None);
        // the file's answer and the fonts taken in, as each redraws a frame
        while session.viewer().awaited().is_some() || session.title_height() == 0.0 {
            wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
            session.answers();
        }
        let path = PathBuf::from("/nonexistent/1.png");
        let made = Answer::Thumbnail(session.thumbs.made_now(&path));
        session.sender.send(made).unwrap();
        let requests = session.answers();
        assert!(
            requests.contains(&Request::Redraw { bar_only: true }),
            "outside it, as before: {requests:?}"
        );
        assert_eq!(session.deadline(), None, "and no frame is due");
    }

    #[test]
    fn thumbnails_wait_for_the_window_s_scale() {
        let options = Options {
            thumbnail_mode: true,
            private_mode: true,
            ..Options::default()
        };
        let program = PathBuf::from("nsxiv-ng");
        let config = Config::default();
        let mut session = Session::new(
            options,
            files(3),
            &config,
            program,
            vec![],
            Bar::Drawn,
            || {},
        );
        session.start();
        session.answers();
        assert_eq!(session.thumbs.asked().0, 0, "none before a resize");
        let window = Size {
            width: 256.0,
            height: 192.0,
        };
        session.resize(window, f64::NAN);
        assert_eq!(session.thumbs.asked().0, 0, "nor with no scale");
        session.resize(window, 2.0);
        assert_eq!(
            session.thumbs.asked(),
            (3, 320),
            "the cells in view and the row below, at scale 2"
        );
        for no_scale in [0.0, -1.0, f64::INFINITY] {
            session.resize(window, no_scale);
            assert_eq!(
                session.thumbs.asked().1,
                320,
                "{no_scale} keeps the last scale"
            );
        }
    }

    #[test]
    fn a_key_without_a_name_ends_the_prefix() {
        let program = PathBuf::from("nsxiv-ng");
        let config = Config::default();
        let options = Options::default();
        let mut session = Session::new(
            options,
            files(1),
            &config,
            program,
            vec![],
            Bar::Drawn,
            || {},
        );
        session.start();
        session.key(&Keystroke::ctrl("x"));
        session.end_prefix();
        assert_eq!(
            session.key(&Keystroke::new("y")),
            None,
            "y is no key-handler's key, and no binding takes it"
        );
    }

    #[test]
    fn the_grid_asks_for_the_cells_in_view_in_order() {
        let options = Options {
            thumbnail_mode: true,
            ..Options::default()
        };
        let mut viewer = Viewer::new(options, files(100));
        viewer.resized(Size {
            width: 340.0,
            height: 296.0,
        });
        let names = |viewer: &Viewer| -> Vec<String> {
            let wanted = wanted(viewer);
            wanted.iter().map(|p| p.display().to_string()).collect()
        };
        // two columns of 148-pixel cells, two rows in view and the one below
        let first: Vec<String> = (0..6).map(|i| format!("/nonexistent/{i}.png")).collect();
        assert_eq!(names(&viewer), first);
        viewer.key(&Keystroke::new("G"));
        let last: Vec<String> = (96..100).map(|i| format!("/nonexistent/{i}.png")).collect();
        assert_eq!(
            names(&viewer),
            last,
            "after a scroll, the cells now in view"
        );
        viewer.key(&Keystroke::new("Return"));
        assert!(names(&viewer).is_empty(), "none outside the grid");
    }

    #[test]
    fn progress_for_a_file_the_viewer_left_changes_nothing() {
        let fixture = |name: &str| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/fixtures")
                .join(name)
        };
        let files = ["moving.gif", "photo.jpg"].map(|name| FileEntry::new(fixture(name)));
        let (sender, wakes) = mpsc::channel();
        let program = PathBuf::from("nsxiv-ng");
        let mut session = Session::new(
            Options::default(),
            files.to_vec(),
            &Config::default(),
            program,
            vec![],
            Bar::Drawn,
            move || {
                let _ = sender.send(());
            },
        );
        session.start();
        while session.viewer().image().is_none() {
            wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
            session.answers();
        }
        let (request, _) = session.animation.as_ref().expect("the GIF animates");
        let request = *request;
        session.key(&Keystroke::new("j"));
        assert!(
            session.animation.is_none(),
            "the animation goes with its file"
        );
        let late = || Answer::Frames {
            request,
            first: 0,
            delays: vec![0.1, 0.2, 0.3],
            complete: true,
        };
        // while the photo decodes, and again once it shows
        session.sender.send(late()).unwrap();
        session.answers();
        assert_eq!(session.deadline(), None, "no frame to step to");
        while session.viewer().image().is_none() {
            wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
            session.answers();
        }
        let photo = session.viewer().image().cloned();
        session.sender.send(late()).unwrap();
        session.answers();
        assert!(!photo.as_ref().unwrap().is_animated());
        assert_eq!(
            session.viewer().image(),
            photo.as_ref(),
            "the photo stays as it came"
        );
        assert_eq!(session.deadline(), None);
    }
}
