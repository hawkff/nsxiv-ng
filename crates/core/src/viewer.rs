//! The viewer's state machine: the file list, marks, modes and key bindings that every
//! frontend shares, with nsxiv's keys. Events go in through methods, and each
//! returns the effects the frontend carries out, in order.

use std::path::PathBuf;

use crate::config::{CC_STEPS, Config, SLIDESHOW_DELAY};
use crate::file_list::FileEntry;
use crate::grid::GridLayout;
use crate::options::Options;
use crate::viewport::{ScaleMode, Viewport};
use crate::{Direction, Point, Size};

/// A key as the bindings see it: the typed text ("j", "G", "?"), or nsxiv's X keysym name
/// for a key without text ("Return", "Left", "BackSpace").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keystroke {
    pub key: String,
    pub ctrl: bool,
}

impl Keystroke {
    pub fn new(key: &str) -> Self {
        Keystroke {
            key: key.to_owned(),
            ctrl: false,
        }
    }

    pub fn ctrl(key: &str) -> Self {
        Keystroke {
            key: key.to_owned(),
            ctrl: true,
        }
    }
}

/// What decoding a file gave: its first frame's size, orientation applied, and one delay in
/// seconds for each frame decoded so far.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageInfo {
    pub size: Size,
    pub delays: Vec<f64>,
    /// Every frame has been decoded once, so `delays` holds them all. An animation's frames
    /// after the first may still arrive, through [`Viewer::frames_decoded`].
    pub complete: bool,
}

impl ImageInfo {
    pub fn frame_count(&self) -> usize {
        self.delays.len()
    }

    pub fn is_animated(&self) -> bool {
        self.delays.len() > 1
    }
}

/// Why a file did not load.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LoadError {
    /// The file could not be read or decoded.
    Unreadable,
    /// Its pixels would need more memory than the cap allows; the size, when the file's
    /// header gave one before the cap stopped the decoder.
    TooLarge(Option<Size>),
    /// A camera RAW file with no embedded JPEG, which is all of a RAW the viewer shows.
    NoPreview,
}

/// The part of the window a redraw concerns; frontends merge them into one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// A new image or error, or another frame, turn, flip, gamma or contrast.
    Image,
    /// Zoom, pan, antialiasing or the alpha checkerboard.
    View,
    /// Files, marks, the selection or the grid layout.
    Grid,
    Bar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    /// The next frame of an animation.
    Animation,
    /// The next image of the slideshow.
    Slideshow,
    /// A reload once the watched file has been quiet for a moment.
    Reload,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Decode the file; answer with [`Viewer::loaded`] and the same request number. Until
    /// then the canvas keeps the last image, and the bar already names this file.
    Load {
        request: u64,
        path: PathBuf,
    },
    Redraw(Part),
    /// The mode or the bar changed: work out the canvas again and call [`Viewer::resized`].
    Layout,
    /// Size the window to [`Viewer::fitted_size`], unless it is fullscreen.
    FitWindow,
    /// The starting window size for the grid, unless -g fixed it or the window is fullscreen.
    ResetWindowSize,
    ToggleFullscreen,
    Beep,
    /// Drop these files' thumbnails and decoded images, and redraw what showed them.
    InvalidateThumbnails(Vec<PathBuf>),
    /// Start the timer, or start it again, to fire in this many seconds through [`Viewer::timer`].
    Timer(Timer, f64),
    CancelTimer(Timer),
    /// Watch this file, and only it, for changes; report them through [`Viewer::file_changed`].
    Watch(Option<PathBuf>),
    /// Run the user's key-handler with the key and the files on its stdin, blocking as nsxiv
    /// does, then call [`Viewer::key_handler_done`]. A missing handler gets a warning instead.
    RunKeyHandler {
        key: String,
        files: Vec<PathBuf>,
    },
    EnterEditMode,
    /// Print these paths to stdout, one per line, and exit with status 0.
    Quit {
        print: Vec<PathBuf>,
    },
}

struct PendingLoad {
    request: u64,
    // a reload keeps the turn and the view where it can
    reload: bool,
    old_size: Size,
}

pub struct Viewer {
    options: Options,
    files: Vec<FileEntry>,
    current: usize,
    alternate: usize,
    /// The anchor of M: the file marked or unmarked last, as in nsxiv.
    last_marked: usize,
    mark_count: usize,
    thumbnail_mode: bool,
    bar_visible: bool,
    image: Option<ImageInfo>,
    error_text: Option<String>,
    frame_index: usize,
    animation_playing: bool,
    /// An edit paused the image's animation: frames that decode later do not start it, until
    /// another image shows or it is played again.
    animation_held: bool,
    /// The frame after this one has yet to decode, so the animation waits for it.
    awaiting_frame: bool,
    /// Degrees clockwise, a multiple of 90.
    rotation: u32,
    flip_h: bool,
    flip_v: bool,
    gamma: i32,
    contrast: i32,
    viewport: Viewport,
    grid: GridLayout,
    slideshow_running: bool,
    slideshow_delay: f64,
    prefix_external: bool,
    requests: u64,
    pending: Option<PendingLoad>,
    /// The request whose answer is on screen.
    shown: u64,
    /// -z, applied to the first image that loads.
    start_zoom: Option<f64>,
    watched: Option<PathBuf>,
    reload_attempts: u32,
    handler_targets: Vec<usize>,
    marked_printed: bool,
    cc_steps: i32,
    gamma_max: f64,
    contrast_max: f64,
    fx: Vec<Effect>,
}

impl Viewer {
    pub fn new(options: Options, files: Vec<FileEntry>) -> Self {
        Viewer::with_config(options, files, &Config::default())
    }

    /// A viewer whose slideshow delay, color steps, zoom levels, pan step and thumbnail sizes
    /// come from `config`.
    pub fn with_config(options: Options, files: Vec<FileEntry>, config: &Config) -> Self {
        // Options and a Config built in code skip the parsers' checks, so start_at may be 0
        // and the delays and steps odd
        let current = options
            .start_at
            .saturating_sub(1)
            .min(files.len().saturating_sub(1));
        let delay = options
            .slideshow
            .into_iter()
            .chain([config.slideshow_delay])
            .find(|d| d.is_finite() && *d > 0.0)
            .unwrap_or(SLIDESHOW_DELAY);
        let mark_count = files.iter().filter(|f| f.marked).count();
        let mut viewport = Viewport::with_config(config);
        if let Some(mode) = options.scale_mode {
            viewport.set_scale_mode(mode);
        }
        let mut grid = GridLayout::with_config(config);
        grid.set_count(files.len());
        Viewer {
            slideshow_delay: delay.max(0.1),
            thumbnail_mode: options.thumbnail_mode,
            bar_visible: config.show_bar && !options.no_bar,
            start_zoom: options.zoom,
            files,
            current,
            alternate: current,
            last_marked: 0,
            mark_count,
            image: None,
            error_text: None,
            frame_index: 0,
            animation_playing: false,
            animation_held: false,
            awaiting_frame: false,
            rotation: 0,
            flip_h: false,
            flip_v: false,
            gamma: 0,
            contrast: 0,
            viewport,
            grid,
            slideshow_running: false,
            prefix_external: false,
            requests: 0,
            pending: None,
            shown: 0,
            watched: None,
            reload_attempts: 0,
            handler_targets: Vec::new(),
            marked_printed: false,
            cc_steps: config.cc_steps.max(2),
            gamma_max: config.gamma_max,
            contrast_max: config.contrast_max,
            fx: Vec::new(),
            options,
        }
    }

    // MARK: state the frontends draw

    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn current_entry(&self) -> Option<&FileEntry> {
        self.files.get(self.current)
    }

    pub fn thumbnail_mode(&self) -> bool {
        self.thumbnail_mode
    }

    pub fn bar_visible(&self) -> bool {
        self.bar_visible
    }

    pub fn image(&self) -> Option<&ImageInfo> {
        self.image.as_ref()
    }

    pub fn error_text(&self) -> Option<&str> {
        self.error_text.as_deref()
    }

    pub fn frame_index(&self) -> usize {
        self.frame_index
    }

    pub fn rotation(&self) -> u32 {
        self.rotation
    }

    pub fn flips(&self) -> (bool, bool) {
        (self.flip_h, self.flip_v)
    }

    pub fn gamma(&self) -> i32 {
        self.gamma
    }

    pub fn contrast(&self) -> i32 {
        self.contrast
    }

    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    pub fn grid(&self) -> &GridLayout {
        &self.grid
    }

    pub fn mark_count(&self) -> usize {
        self.mark_count
    }

    /// The request whose answer [`Viewer::loaded`] takes, while the viewer waits for one; an
    /// answer to any other changes nothing.
    pub fn awaited(&self) -> Option<u64> {
        self.pending.as_ref().map(|p| p.request)
    }

    /// The image's pixels as shown, the turn applied.
    pub fn display_size(&self) -> Option<Size> {
        let s = self.image.as_ref()?.size;
        Some(if self.rotation.is_multiple_of(180) {
            s
        } else {
            Size {
                width: s.height,
                height: s.width,
            }
        })
    }

    /// The size the window follows, keeping the image's aspect; none in the grid or with -g.
    pub fn fitted_size(&self) -> Option<Size> {
        if self.options.geometry.is_some() || self.thumbnail_mode {
            None
        } else {
            self.display_size()
        }
    }

    pub fn gamma_value(&self) -> f64 {
        color_factor(self.gamma, self.cc_steps, self.gamma_max)
    }

    pub fn contrast_value(&self) -> f64 {
        color_factor(self.contrast, self.cc_steps, self.contrast_max)
    }

    /// The bar's left part, the file name, and its right part, fields separated by " | ".
    pub fn status_text(&self) -> (String, String) {
        let Some(entry) = self.current_entry() else {
            return (String::new(), "0/0".to_owned());
        };
        let mut right = String::new();
        if self.slideshow_running {
            right += &format!("{}s | ", self.slideshow_delay);
        }
        if self.gamma != 0 {
            right += &format!("G{:+} | ", self.gamma);
        }
        if self.contrast != 0 {
            right += &format!("C{:+} | ", self.contrast);
        }
        if self.mark_count > 0 {
            right += &format!("*{} | ", self.mark_count);
        }
        if entry.marked {
            right += "* | ";
        }
        if !self.thumbnail_mode
            && let Some(image) = &self.image
        {
            if image.is_animated() {
                let state = if self.animation_playing { "▶" } else { "⏸" };
                right += &format!(
                    "{state}[{}/{}] | ",
                    self.frame_index + 1,
                    image.frame_count()
                );
            }
            right += &format!("{}x{} | ", image.size.width, image.size.height);
            right += &format!("{}% | ", (self.viewport.zoom() * 100.0).round());
        }
        right += &format!("{}/{}", self.current + 1, self.files.len());
        let name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (name, right)
    }

    // MARK: events

    /// Once the window exists.
    pub fn start(&mut self) -> Vec<Effect> {
        self.emit(Effect::Layout);
        if self.thumbnail_mode {
            self.grid.reveal(self.current);
            self.emit(Effect::Redraw(Part::Grid));
        } else {
            self.load_current();
        }
        if self.options.fullscreen {
            self.emit(Effect::ToggleFullscreen);
        }
        if self.options.slideshow.is_some() && !self.thumbnail_mode {
            self.start_slideshow();
        }
        self.emit(Effect::Redraw(Part::Bar));
        self.take()
    }

    /// The answer to [`Effect::Load`]: the image, or why it did not load, which the error
    /// text names. An answer to a request the viewer has moved past changes nothing.
    pub fn loaded(&mut self, request: u64, image: Result<ImageInfo, LoadError>) -> Vec<Effect> {
        let Some(pending) = self.pending.take_if(|p| p.request == request) else {
            return Vec::new();
        };
        // C-a during a reload may have restarted the old image's animation
        self.stop_animation();
        self.animation_held = false;
        self.shown = request;
        self.frame_index = 0;
        let name = |entry: &FileEntry| {
            entry
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        self.error_text = match (&image, self.current_entry()) {
            (Ok(_), _) => None,
            (Err(_), None) => Some("no images".to_owned()),
            (Err(LoadError::Unreadable), Some(entry)) => {
                Some(format!("could not load: {}", name(entry)))
            }
            (Err(LoadError::TooLarge(Some(size))), _) => {
                Some(format!("too large: {}x{}", size.width, size.height))
            }
            (Err(LoadError::TooLarge(None)), Some(entry)) => {
                Some(format!("too large: {}", name(entry)))
            }
            (Err(LoadError::NoPreview), Some(_)) => {
                Some("could not load: no preview in RAW".to_owned())
            }
        };
        self.image = image.ok();
        let size = self.display_size().unwrap_or_default();
        if pending.reload {
            // a fit mode fits a file whose size changed, say one a key-handler turned
            let reset = size != pending.old_size && self.viewport.scale_mode() != ScaleMode::Manual;
            self.viewport.set_image_size(size, reset);
        } else {
            self.emit(Effect::FitWindow);
            self.viewport.set_image_size(size, true);
        }
        // -z goes to the first image that loads, however it was asked for
        if self.image.is_some()
            && let Some(zoom) = self.start_zoom.take()
        {
            self.viewport.set_zoom(zoom, None);
        }
        self.emit(Effect::Redraw(Part::Image));
        self.emit(Effect::Redraw(Part::View));
        if pending.reload {
            self.emit(Effect::FitWindow);
        }
        if self.image.as_ref().is_some_and(ImageInfo::is_animated) {
            self.start_animation();
        }
        self.emit(Effect::Redraw(Part::Bar));
        // each image, however it came, gets the slideshow's whole delay on screen
        if self.slideshow_running {
            self.emit(Effect::Timer(Timer::Slideshow, self.slideshow_delay));
        }
        self.take()
    }

    /// More frames of the animation on screen decoded: the delays of its frames from `first`
    /// on, as far as it decoded them, and whether its last frame is among them. Updates come in
    /// order; one for an image no longer on screen, or for one the grid hides, changes nothing.
    pub fn frames_decoded(
        &mut self,
        request: u64,
        first: usize,
        delays: &[f64],
        complete: bool,
    ) -> Vec<Effect> {
        if request != self.shown || self.thumbnail_mode {
            return Vec::new();
        }
        let Some(image) = self.image.as_mut() else {
            return Vec::new();
        };
        let was_animated = image.is_animated();
        // the first update replaces the one delay a still gets
        image.delays.truncate(first);
        image.delays.extend_from_slice(delays);
        image.complete = complete;
        if !was_animated && image.is_animated() && !self.animation_held {
            self.start_animation();
        } else if self.awaiting_frame {
            self.next_frame();
        }
        // the frame on screen may be one that just decoded, say again after a step back past
        // the frames an animation keeps, which a paused animation would never redraw
        self.emit(Effect::Redraw(Part::Image));
        self.emit(Effect::Redraw(Part::Bar));
        self.take()
    }

    /// The canvas the image or the grid gets, the bar left out.
    pub fn resized(&mut self, canvas: Size) -> Vec<Effect> {
        self.viewport.set_view_size(canvas);
        self.grid.set_view_size(canvas);
        // the column count changes with the width
        self.grid.reveal(self.current);
        self.emit(Effect::Redraw(Part::View));
        self.emit(Effect::Redraw(Part::Grid));
        self.emit(Effect::Redraw(Part::Bar));
        self.take()
    }

    pub fn timer(&mut self, timer: Timer) -> Vec<Effect> {
        match timer {
            Timer::Animation => {
                if self.animation_playing {
                    self.next_frame();
                }
            }
            Timer::Slideshow if self.slideshow_running => {
                // an image still decoding is not skipped; otherwise the next one, wrapping like nsxiv
                if self.pending.is_none() {
                    let next = if self.current + 1 < self.files.len() {
                        self.current + 1
                    } else {
                        0
                    };
                    self.set_current(next);
                }
                // a decoding image starts the countdown once it shows, in loaded
                if self.pending.is_none() {
                    self.emit(Effect::Timer(Timer::Slideshow, self.slideshow_delay));
                }
            }
            Timer::Slideshow => {}
            Timer::Reload => self.reload_when_quiet(),
        }
        self.take()
    }

    /// The watched file was written or replaced. Editors write in chunks, or save a temporary
    /// file and rename it over the original, so the reload waits until the file has been
    /// quiet for 0.2 s.
    pub fn file_changed(&mut self) -> Vec<Effect> {
        if self.watched.is_some() {
            self.reload_attempts = 10;
            self.emit(Effect::Timer(Timer::Reload, 0.2));
        }
        self.take()
    }

    /// After [`Effect::RunKeyHandler`]: the handler may have edited, moved or deleted its files.
    pub fn key_handler_done(&mut self) -> Vec<Effect> {
        let targets = std::mem::take(&mut self.handler_targets);
        let paths: Vec<PathBuf> = targets
            .iter()
            .filter_map(|&i| self.files.get(i))
            .map(|f| f.path.clone())
            .collect();
        self.emit(Effect::InvalidateThumbnails(paths));
        let gone: Vec<usize> = targets
            .into_iter()
            .filter(|&i| self.files.get(i).is_some_and(|f| !f.path.exists()))
            .collect();
        let current_gone = gone.contains(&self.current);
        self.remove_files(&gone);
        if !current_gone {
            self.reload_current();
        }
        self.take()
    }

    /// A key the bindings did not get, as a menu shortcut or one without a name, still ends a
    /// C-x prefix, without running the handler.
    pub fn end_prefix(&mut self) {
        self.prefix_external = false;
    }

    /// The effects of the key's binding, or `None` when no binding takes the key.
    pub fn key(&mut self, k: &Keystroke) -> Option<Vec<Effect>> {
        let bound = self.handle(k);
        let fx = self.take();
        bound.then_some(fx)
    }

    // MARK: pointer events

    /// A click in the grid.
    pub fn select(&mut self, i: usize) -> Vec<Effect> {
        if i < self.files.len() {
            self.current = i;
            self.grid.reveal(i);
            self.emit(Effect::Redraw(Part::Grid));
            self.emit(Effect::Redraw(Part::Bar));
        }
        self.take()
    }

    /// Puts `path` in the list after the current file and makes it current, as with the image
    /// edit mode saved, which the user sees next: in image mode it loads, in the grid the cell
    /// is selected. The marks stay with their files.
    pub fn insert_after_current(&mut self, path: PathBuf) -> Vec<Effect> {
        // C-6's file becomes the current one, in set_current
        let at = self.insert_next(vec![path]);
        self.set_current(at);
        self.take()
    }

    /// Puts `paths` in the list after the current file, in their order, and the current file
    /// stays current, as files opened while the viewer runs. The marks, M's anchor and C-6's
    /// file stay with their files.
    pub fn insert_after(&mut self, paths: Vec<PathBuf>) -> Vec<Effect> {
        if paths.is_empty() {
            return self.take();
        }
        self.insert_next(paths);
        // the bar counts the files, the grid shows them
        self.emit(Effect::Redraw(Part::Bar));
        if self.thumbnail_mode {
            self.emit(Effect::Redraw(Part::Grid));
        }
        self.take()
    }

    // Puts `paths` in the list after the current file, in one move of the files after it,
    // and gives where the first went.
    fn insert_next(&mut self, paths: Vec<PathBuf>) -> usize {
        let at = (self.current + 1).min(self.files.len());
        // M's anchor and C-6's file stay with their files where they name one
        let (len, count) = (self.files.len(), paths.len());
        let (anchored, alternate) = (self.last_marked < len, self.alternate < len);
        self.files
            .splice(at..at, paths.into_iter().map(FileEntry::new));
        if anchored && self.last_marked >= at {
            self.last_marked += count;
        }
        if alternate && self.alternate >= at {
            self.alternate += count;
        }
        self.grid.set_count(self.files.len());
        at
    }

    /// A double click in the grid.
    pub fn open(&mut self, i: usize) -> Vec<Effect> {
        self.set_current(i);
        self.toggle_mode();
        self.take()
    }

    pub fn pan(&mut self, dx: f64, dy: f64) -> Vec<Effect> {
        self.change_view(|v| v.scroll(dx, dy));
        self.take()
    }

    pub fn zoom_to(&mut self, zoom: f64, anchor: Point) -> Vec<Effect> {
        self.change_view(|v| v.set_zoom(zoom, Some(anchor)));
        self.take()
    }

    pub fn zoom_step(&mut self, dir: i32) -> Vec<Effect> {
        self.change_view(|v| v.zoom_step(dir));
        self.take()
    }

    pub fn scroll_grid(&mut self, dy: f64) -> Vec<Effect> {
        self.grid.scroll(dy);
        self.emit(Effect::Redraw(Part::Grid));
        self.take()
    }

    /// A right click in the grid marks without moving the selection, like nsxiv.
    pub fn toggle_mark(&mut self, i: usize) -> Vec<Effect> {
        self.toggle_mark_at(i);
        self.take()
    }

    /// A click near the left or right edge.
    pub fn navigate(&mut self, d: isize) -> Vec<Effect> {
        self.go(d);
        self.take()
    }

    /// A right click in image mode.
    pub fn switch_mode(&mut self) -> Vec<Effect> {
        self.toggle_mode();
        self.take()
    }

    // MARK: loading

    fn emit(&mut self, effect: Effect) {
        self.fx.push(effect);
    }

    fn take(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.fx)
    }

    fn request_load(&mut self, reload: bool) -> Option<PathBuf> {
        let path = self.current_entry()?.path.clone();
        self.requests += 1;
        self.pending = Some(PendingLoad {
            request: self.requests,
            reload,
            old_size: self.viewport.image_size(),
        });
        self.emit(Effect::Load {
            request: self.requests,
            path: path.clone(),
        });
        Some(path)
    }

    fn set_current(&mut self, i: usize) {
        if i >= self.files.len() {
            return;
        }
        if i != self.current {
            self.alternate = self.current;
        }
        self.current = i;
        self.grid.reveal(i);
        if self.thumbnail_mode {
            self.emit(Effect::Redraw(Part::Grid));
            self.emit(Effect::Redraw(Part::Bar));
        } else {
            self.load_current();
        }
    }

    fn load_current(&mut self) {
        self.stop_animation();
        self.rotation = 0;
        self.flip_h = false;
        self.flip_v = false;
        self.frame_index = 0;
        self.image = None;
        let had_error = self.error_text.take().is_some();
        self.emit(Effect::CancelTimer(Timer::Reload));
        let path = self.request_load(false);
        self.watched.clone_from(&path);
        self.emit(Effect::Watch(path.clone()));
        if path.is_some() {
            // the bar names the new file at once; the canvas keeps the last image until it decodes,
            // but the text of what went wrong goes now
            if had_error {
                self.emit(Effect::Redraw(Part::Image));
            }
            self.emit(Effect::Redraw(Part::Bar));
        } else {
            // no file to load: what loading would do, at once
            self.requests += 1;
            self.pending = Some(PendingLoad {
                request: self.requests,
                reload: false,
                old_size: Size::default(),
            });
            let fx = self.loaded(self.requests, Err(LoadError::Unreadable));
            self.fx.extend(fx);
        }
    }

    fn reload_current(&mut self) {
        if self.current_entry().is_none() {
            return;
        }
        let paths = vec![self.files[self.current].path.clone()];
        self.emit(Effect::InvalidateThumbnails(paths));
        if self.thumbnail_mode {
            return;
        }
        self.stop_animation();
        // a file that has yet to show loads as new, view reset and all, not as a reload
        let reload = !self.pending.as_ref().is_some_and(|p| !p.reload);
        self.request_load(reload);
    }

    fn reload_when_quiet(&mut self) {
        let Some(path) = self.watched.clone() else {
            return;
        };
        if !path.exists() {
            // a save that deletes before it writes leaves the path empty for a while
            if self.reload_attempts > 1 {
                self.reload_attempts -= 1;
                self.emit(Effect::Timer(Timer::Reload, 0.2));
            }
            return;
        }
        if self.current_entry().is_some_and(|e| e.path == path) {
            self.reload_current();
            // a save that renames a new file over the old one leaves a new inode to watch
            self.emit(Effect::Watch(Some(path)));
        } else {
            self.emit(Effect::InvalidateThumbnails(vec![path]));
        }
    }

    // after another frame, flip, gamma or contrast
    fn show_frame(&mut self) {
        // with no image yet, the canvas keeps the last one, and loading applies the change
        let Some(size) = self.display_size() else {
            return;
        };
        self.viewport.set_image_size(size, false);
        self.emit(Effect::Redraw(Part::Image));
        self.emit(Effect::Redraw(Part::View));
    }

    fn change_view(&mut self, change: impl FnOnce(&mut Viewport)) {
        change(&mut self.viewport);
        self.emit(Effect::Redraw(Part::View));
        self.emit(Effect::Redraw(Part::Bar));
    }

    fn color_changed(&mut self) {
        self.show_frame();
        self.emit(Effect::Redraw(Part::Bar));
    }

    fn rotate(&mut self, degrees: u32) {
        if self.image.is_none() && self.pending.is_none() {
            return;
        }
        // under one flip a clockwise turn on screen is counterclockwise before the flip;
        // two flips make a half turn and keep the direction
        let turn = if self.flip_h == self.flip_v {
            degrees
        } else {
            360 - degrees
        };
        self.rotation = (self.rotation + turn) % 360;
        // a turn pressed while the image loads waits for it: loading fits the turned size
        let Some(size) = self.image.as_ref().map(|i| i.size) else {
            return;
        };
        let turned = if self.rotation.is_multiple_of(180) {
            size
        } else {
            Size {
                width: size.height,
                height: size.width,
            }
        };
        // a fit mode fits the turned image again, as nsxiv does
        let reset = self.viewport.scale_mode() != ScaleMode::Manual;
        self.viewport.set_image_size(turned, reset);
        self.emit(Effect::Redraw(Part::Image));
        self.emit(Effect::Redraw(Part::View));
        self.emit(Effect::FitWindow);
        self.emit(Effect::Redraw(Part::Bar));
    }

    // MARK: animation and slideshow

    fn start_animation(&mut self) {
        if self.image.as_ref().is_some_and(ImageInfo::is_animated) {
            self.animation_playing = true;
            self.schedule_next_frame();
        }
    }

    fn schedule_next_frame(&mut self) {
        if let Some(image) = &self.image {
            let delay = image.delays[self.frame_index % image.frame_count()];
            // 10 ms or less means the 100 ms browsers use, so no file can spin the timer
            let delay = if delay.is_finite() && delay >= 0.011 {
                delay
            } else {
                0.1
            };
            self.emit(Effect::Timer(Timer::Animation, delay));
        }
    }

    // the next frame, once its delay is known: after the last frame of a complete animation,
    // the first
    fn next_frame(&mut self) {
        let Some(image) = &self.image else {
            return;
        };
        let next = self.frame_index + 1;
        if next < image.delays.len() {
            self.frame_index = next;
        } else if image.complete {
            self.frame_index = 0;
        } else {
            // the decoder is behind; frames_decoded carries on from here
            self.awaiting_frame = true;
            return;
        }
        self.awaiting_frame = false;
        self.show_frame();
        self.emit(Effect::Redraw(Part::Bar));
        self.schedule_next_frame();
    }

    fn stop_animation(&mut self) {
        self.awaiting_frame = false;
        if self.animation_playing {
            self.animation_playing = false;
            self.emit(Effect::CancelTimer(Timer::Animation));
        }
    }

    fn toggle_animation(&mut self) {
        if self.animation_playing {
            self.stop_animation();
        } else {
            self.animation_held = false;
            self.start_animation();
        }
        self.emit(Effect::Redraw(Part::Bar));
    }

    fn navigate_frame(&mut self, d: isize) {
        let Some((count, complete)) = self
            .image
            .as_ref()
            .filter(|i| i.is_animated())
            .map(|i| (i.frame_count() as isize, i.complete))
        else {
            return;
        };
        self.stop_animation();
        let target = self.frame_index as isize + d;
        // while frames still arrive, stepping goes no further than the last one known
        self.frame_index = if complete {
            target.rem_euclid(count)
        } else {
            target.clamp(0, count - 1)
        } as usize;
        self.show_frame();
        self.emit(Effect::Redraw(Part::Bar));
    }

    fn start_slideshow(&mut self) {
        self.slideshow_running = true;
        // an image still decoding starts the countdown once it shows, in loaded
        if self.pending.is_none() {
            self.emit(Effect::Timer(Timer::Slideshow, self.slideshow_delay));
        }
        self.emit(Effect::Redraw(Part::Bar));
    }

    fn stop_slideshow(&mut self) {
        if self.slideshow_running {
            self.slideshow_running = false;
            self.emit(Effect::CancelTimer(Timer::Slideshow));
        }
        self.emit(Effect::Redraw(Part::Bar));
    }

    fn adjust_slideshow_delay(&mut self, d: f64) {
        self.slideshow_delay = (self.slideshow_delay + d).clamp(1.0, 600.0);
        if self.slideshow_running {
            self.start_slideshow();
        }
        self.emit(Effect::Redraw(Part::Bar));
    }

    // MARK: navigation and modes

    fn go(&mut self, d: isize) {
        match self
            .current
            .checked_add_signed(d)
            .filter(|&i| i < self.files.len())
        {
            Some(i) => self.set_current(i),
            None => self.emit(Effect::Beep),
        }
    }

    fn go_marked(&mut self, d: isize) {
        let mut i = self.current.checked_add_signed(d);
        while let Some(j) = i.filter(|&j| j < self.files.len()) {
            if self.files[j].marked {
                self.set_current(j);
                return;
            }
            i = j.checked_add_signed(d);
        }
        self.emit(Effect::Beep);
    }

    fn toggle_mode(&mut self) {
        self.thumbnail_mode = !self.thumbnail_mode;
        if self.thumbnail_mode {
            // an image still decoding no longer shows; its answer must change nothing
            self.pending = None;
            self.stop_slideshow();
            self.stop_animation();
            self.grid.reveal(self.current);
            self.emit(Effect::Redraw(Part::Grid));
        } else {
            self.load_current();
        }
        self.emit(Effect::Layout);
        // a window fitted to a small image would leave the grid one clipped column
        if self.thumbnail_mode {
            self.emit(Effect::ResetWindowSize);
        }
    }

    fn move_selection(&mut self, d: Direction) {
        if !self.files.is_empty() {
            let i = self.grid.neighbor(self.current, d);
            let fx = self.select(i);
            self.fx.extend(fx);
        }
    }

    // MARK: marks and removal

    fn toggle_mark_at(&mut self, i: usize) {
        if let Some(file) = self.files.get_mut(i) {
            file.marked = !file.marked;
            self.last_marked = i;
            self.sync_marks();
        }
    }

    fn mark_range(&mut self) {
        if self.last_marked >= self.files.len() || self.current >= self.files.len() {
            return;
        }
        let state = self.files[self.last_marked].marked;
        let (from, to) = (
            self.last_marked.min(self.current),
            self.last_marked.max(self.current),
        );
        for file in &mut self.files[from..=to] {
            file.marked = state;
        }
        self.last_marked = self.current;
        self.sync_marks();
    }

    fn set_all_marks(&mut self, mark: impl Fn(bool) -> bool) {
        for file in &mut self.files {
            file.marked = mark(file.marked);
        }
        self.sync_marks();
    }

    fn sync_marks(&mut self) {
        self.mark_count = self.files.iter().filter(|f| f.marked).count();
        self.emit(Effect::Redraw(Part::Grid));
        self.emit(Effect::Redraw(Part::Bar));
    }

    fn remove_files(&mut self, removed: &[usize]) {
        let kept: Vec<usize> = (0..self.files.len())
            .filter(|i| !removed.contains(i))
            .collect();
        if kept.len() == self.files.len() {
            return;
        }
        if kept.is_empty() {
            self.files.clear(); // -o must not print what was just removed
            self.grid.set_count(0);
            self.mark_count = 0;
            self.quit();
            return;
        }
        let current_removed = removed.contains(&self.current);
        self.current = remap(self.current, &kept);
        self.alternate = remap(self.alternate, &kept);
        self.last_marked = remap(self.last_marked, &kept);
        let mut i = 0;
        self.files.retain(|_| {
            i += 1;
            !removed.contains(&(i - 1))
        });
        self.grid.set_count(self.files.len());
        self.grid.reveal(self.current);
        self.sync_marks();
        if current_removed && !self.thumbnail_mode {
            self.load_current();
        }
    }

    fn quit(&mut self) {
        let print = if self.options.output_marked && !self.marked_printed {
            self.files
                .iter()
                .filter(|f| f.marked)
                .map(|f| f.path.clone())
                .collect()
        } else {
            Vec::new()
        };
        self.marked_printed = true;
        self.emit(Effect::Quit { print });
    }

    fn run_key_handler(&mut self, key: String) {
        // the marked files, or else the current one
        let mut targets: Vec<usize> = (0..self.files.len())
            .filter(|&i| self.files[i].marked)
            .collect();
        if targets.is_empty() && self.current < self.files.len() {
            targets.push(self.current);
        }
        if targets.is_empty() {
            return;
        }
        let files = targets
            .iter()
            .map(|&i| self.files[i].path.clone())
            .collect();
        self.handler_targets = targets;
        self.emit(Effect::RunKeyHandler { key, files });
    }

    /// An edit began over the image shown: the slideshow stops and an animation pauses, so the
    /// image edited is the one shown, and neither starts again once the edit ends.
    pub fn edit_entered(&mut self) -> Vec<Effect> {
        self.stop_slideshow();
        self.stop_animation();
        // and one whose next frames are still to decode stays paused when they come
        self.animation_held = true;
        self.take()
    }

    fn enter_edit_mode(&mut self) {
        if self.thumbnail_mode || self.image.is_none() {
            self.emit(Effect::Beep);
        } else {
            self.emit(Effect::EnterEditMode);
        }
    }

    // MARK: key bindings

    fn handle(&mut self, k: &Keystroke) -> bool {
        if self.prefix_external {
            self.prefix_external = false;
            // Escape aborts, as in nsxiv
            if k.key != "Escape" {
                self.run_key_handler(format!("{}{}", if k.ctrl { "C-" } else { "" }, k.key));
            }
            return true;
        }
        if k.ctrl {
            match k.key.as_str() {
                "x" => self.prefix_external = true,
                "h" | "Left" => self.scroll_screen_or_page(Direction::Left),
                "j" | "Down" => self.scroll_screen_or_page(Direction::Down),
                "k" | "Up" => self.scroll_screen_or_page(Direction::Up),
                "l" | "Right" => self.scroll_screen_or_page(Direction::Right),
                "m" => self.set_all_marks(|m| !m),
                "u" => self.set_all_marks(|_| false),
                "g" => {
                    self.gamma = 0;
                    self.color_changed();
                }
                "n" => self.navigate_frame(1),
                "p" => self.navigate_frame(-1),
                "a" | " " => self.toggle_animation(),
                "6" => self.set_current(self.alternate),
                "]" => self.step_contrast(1),
                "[" => self.step_contrast(-1),
                _ => return false,
            }
            return true;
        }
        match k.key.as_str() {
            "Return" => self.toggle_mode(),
            "Left" => self.directional(Direction::Left),
            "Right" => self.directional(Direction::Right),
            "Up" => self.directional(Direction::Up),
            "Down" => self.directional(Direction::Down),
            "BackSpace" => {
                if !self.thumbnail_mode {
                    self.go(-1);
                }
            }
            "q" => self.quit(),
            "Q" => {
                // pick-quit: the current file instead of the marks
                let print = self
                    .current_entry()
                    .map(|e| e.path.clone())
                    .into_iter()
                    .collect();
                self.marked_printed = true;
                self.emit(Effect::Quit { print });
            }
            "f" => self.emit(Effect::ToggleFullscreen),
            "b" => {
                self.bar_visible = !self.bar_visible;
                self.emit(Effect::Layout);
                self.emit(Effect::FitWindow);
            }
            "g" => self.set_current(0),
            "G" => self.set_current(self.files.len().saturating_sub(1)),
            "r" => self.reload_current(),
            "R" => {
                if self.thumbnail_mode {
                    let paths = self.files.iter().map(|f| f.path.clone()).collect();
                    self.emit(Effect::InvalidateThumbnails(paths));
                }
            }
            "D" => self.remove_files(&[self.current]),
            "m" => {
                self.toggle_mark_at(self.current);
                if !self.thumbnail_mode {
                    self.go(1);
                }
            }
            "M" => self.mark_range(),
            "N" => self.go_marked(1),
            "P" => self.go_marked(-1),
            "{" => self.step_gamma(-1),
            "}" => self.step_gamma(1),
            "(" => self.step_contrast(-1),
            ")" => self.step_contrast(1),
            "+" => self.zoom_key(1),
            "-" => self.zoom_key(-1),
            key if self.thumbnail_mode => return self.handle_grid_key(key),
            key => return self.handle_image_key(key),
        }
        true
    }

    fn handle_grid_key(&mut self, key: &str) -> bool {
        match key {
            "h" => self.move_selection(Direction::Left),
            "j" => self.move_selection(Direction::Down),
            "k" => self.move_selection(Direction::Up),
            "l" => self.move_selection(Direction::Right),
            " " => self.toggle_mode(),
            _ => return false,
        }
        true
    }

    fn handle_image_key(&mut self, key: &str) -> bool {
        match key {
            "j" => self.go(1),
            "k" => self.go(-1),
            " " => {} // disabled
            "]" => self.go(10),
            "[" => self.go(-10),
            "h" => self.change_view(|v| v.scroll_step(Direction::Left, false)),
            "l" => self.change_view(|v| v.scroll_step(Direction::Right, false)),
            "u" => self.change_view(|v| v.scroll_step(Direction::Up, false)),
            "d" => self.change_view(|v| v.scroll_step(Direction::Down, false)),
            "H" => self.change_view(|v| v.scroll_to_edge(Direction::Left)),
            "J" => self.change_view(|v| v.scroll_to_edge(Direction::Down)),
            "K" => self.change_view(|v| v.scroll_to_edge(Direction::Up)),
            "L" => self.change_view(|v| v.scroll_to_edge(Direction::Right)),
            "z" => self.change_view(Viewport::center_image),
            "=" => self.change_view(|v| v.set_zoom(1.0, None)),
            "w" => self.change_view(|v| v.set_scale_mode(ScaleMode::Down)),
            "W" => self.change_view(|v| v.set_scale_mode(ScaleMode::Fit)),
            "F" => self.change_view(|v| v.set_scale_mode(ScaleMode::Fill)),
            "e" => self.change_view(|v| v.set_scale_mode(ScaleMode::Width)),
            "E" => self.change_view(|v| v.set_scale_mode(ScaleMode::Height)),
            "<" => self.rotate(270),
            ">" => self.rotate(90),
            "?" => self.rotate(180),
            "|" => {
                self.flip_h = !self.flip_h;
                self.show_frame();
            }
            "_" => {
                self.flip_v = !self.flip_v;
                self.show_frame();
            }
            "a" => self.change_view(|v| v.antialias = !v.antialias),
            "A" => self.change_view(|v| v.alpha_layer = !v.alpha_layer),
            "x" => self.enter_edit_mode(),
            "s" => {
                if self.slideshow_running {
                    self.stop_slideshow();
                } else {
                    self.start_slideshow();
                }
            }
            "S" => self.adjust_slideshow_delay(1.0),
            _ => return false,
        }
        true
    }

    fn step_gamma(&mut self, d: i32) {
        self.gamma = (self.gamma + d).clamp(-self.cc_steps + 1, self.cc_steps);
        self.color_changed();
    }

    fn step_contrast(&mut self, d: i32) {
        self.contrast = (self.contrast + d).clamp(-self.cc_steps + 1, self.cc_steps);
        self.color_changed();
    }

    fn zoom_key(&mut self, dir: i32) {
        if !self.thumbnail_mode {
            self.change_view(|v| v.zoom_step(dir));
        } else if self.grid.zoom(dir) {
            self.grid.reveal(self.current);
            self.emit(Effect::Redraw(Part::Grid));
        }
    }

    fn scroll_screen_or_page(&mut self, d: Direction) {
        if self.thumbnail_mode {
            self.grid.scroll_page(d);
            self.emit(Effect::Redraw(Part::Grid));
        } else {
            self.change_view(|v| v.scroll_step(d, true));
        }
    }

    fn directional(&mut self, d: Direction) {
        if self.thumbnail_mode {
            self.move_selection(d);
        } else {
            self.change_view(|v| v.scroll_step(d, false));
        }
    }
}

/// A gamma or contrast step as the renderer's factor: 1 is neutral, and the top step reaches
/// `max`.
pub fn color_value(step: i32, max: f64) -> f64 {
    color_factor(step, CC_STEPS, max)
}

fn color_factor(step: i32, steps: i32, max: f64) -> f64 {
    let steps = f64::from(steps);
    if step <= 0 {
        (steps + f64::from(step)) / steps
    } else {
        // the share first, so a huge max stays finite
        1.0 + f64::from(step) / steps * (max - 1.0)
    }
}

/// Where index `i` lands once only the indices in `kept` stay: on its own file, or on the
/// next one that stays when its file went, or on the new last.
pub fn remap(i: usize, kept: &[usize]) -> usize {
    kept.iter()
        .position(|&k| k >= i)
        .unwrap_or(kept.len().saturating_sub(1))
}

/// The bar's right part in a bar too narrow for it: whole " | " fields drop from the front,
/// so the index, rightmost, stays longest.
pub fn fitted_status(right: &str, room: f64, width: impl Fn(&str) -> f64) -> String {
    let mut fields = right;
    while width(fields) > room
        && let Some(cut) = fields.find(" | ")
    {
        fields = &fields[cut + 3..];
    }
    fields.to_owned()
}

/// How far a window with edges at 0 and `extent` may grow inside the room `lo..hi` around it,
/// where it cannot move itself. A window whose compositor keeps its top-left corner needs the
/// room after it; one that keeps its center, half the growth on each side; where that is
/// unknown, growth must suit both. A window already reaching outside the room, as a user may
/// park it, grows freely on that axis.
pub fn grown_limit(extent: i64, lo: f64, hi: f64, keeps_top_left: bool) -> i64 {
    if lo > 0.0 || hi < extent as f64 {
        return i64::MAX;
    }
    let after = hi - extent as f64;
    let growth = if keeps_top_left {
        after
    } else {
        after.min(-2.0 * lo)
    };
    extent + growth.floor() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(n: usize) -> Vec<FileEntry> {
        (0..n)
            .map(|i| FileEntry::new(PathBuf::from(format!("/pics/{i}.png"))))
            .collect()
    }

    fn still() -> ImageInfo {
        ImageInfo {
            size: Size {
                width: 320.0,
                height: 160.0,
            },
            delays: vec![0.1],
            complete: true,
        }
    }

    // a frontend that decodes every file at once, as a 320x160 still
    fn answer(v: &mut Viewer, fx: Vec<Effect>) -> Vec<Effect> {
        let mut all = Vec::new();
        for effect in fx {
            let request = match effect {
                Effect::Load { request, .. } => Some(request),
                _ => None,
            };
            all.push(effect);
            if let Some(request) = request {
                let more = v.loaded(request, Ok(still()));
                all.extend(answer(v, more));
            }
        }
        all
    }

    fn press(v: &mut Viewer, key: &str) -> Vec<Effect> {
        let fx = v.key(&Keystroke::new(key)).expect("a bound key");
        answer(v, fx)
    }

    fn started(options: Options, n: usize) -> Viewer {
        started_with(options, &Config::default(), n)
    }

    /// The options a command line gives.
    fn run_options(args: &[&str]) -> Options {
        match crate::options::parse(args.iter().map(std::ffi::OsString::from)) {
            Ok(crate::options::Command::Run(options)) => options,
            other => panic!("{args:?} gave {other:?}"),
        }
    }

    fn started_with(options: Options, config: &Config, n: usize) -> Viewer {
        let mut v = Viewer::with_config(options, files(n), config);
        v.resized(Size {
            width: 640.0,
            height: 480.0,
        });
        let fx = v.start();
        answer(&mut v, fx);
        v
    }

    fn beeps(fx: &[Effect]) -> usize {
        fx.iter().filter(|e| **e == Effect::Beep).count()
    }

    #[test]
    fn bindings_take_nsxiv_s_keys() {
        let mut v = started(Options::default(), 5);
        assert_eq!(
            v.viewport().zoom(),
            1.0,
            "w (the default) keeps a small image at 100%"
        );
        press(&mut v, "W");
        assert_eq!(v.viewport().zoom(), 2.0, "W fits the image to the view");
        press(&mut v, ">");
        assert_eq!(
            v.display_size(),
            Some(Size {
                width: 160.0,
                height: 320.0
            })
        );
        assert_eq!(
            v.viewport().zoom(),
            1.5,
            "a turn fits the turned image again"
        );
        press(&mut v, "|");
        press(&mut v, ">");
        assert_eq!(
            v.rotation(),
            0,
            "under a flip, a clockwise turn on screen undoes the previous one"
        );
        v.toggle_mark(1);
        press(&mut v, "Return");
        assert!(v.thumbnail_mode(), "Return opens the grid");
        v.select(3);
        press(&mut v, "M");
        let marks: Vec<bool> = v.files().iter().map(|f| f.marked).collect();
        assert_eq!(
            (marks, v.mark_count()),
            (vec![false, true, true, true, false], 3),
            "M marks from the image marked last"
        );
        press(&mut v, "D");
        assert_eq!(
            (v.files().len(), v.current(), v.mark_count()),
            (4, 3, 2),
            "D removes the selection, which moves to the new last file"
        );
        let fx = [press(&mut v, "g"), press(&mut v, "k")].concat();
        assert_eq!(
            (v.current(), beeps(&fx)),
            (0, 0),
            "k stays on the first thumbnail without a beep"
        );
        press(&mut v, "Return");
        let fx = press(&mut v, "k");
        assert!(
            !v.thumbnail_mode() && beeps(&fx) == 1,
            "k before the first image beeps"
        );
        assert_eq!(v.key(&Keystroke::new("y")), None, "an unbound key");
    }

    fn pan(v: &Viewer) -> (f64, f64) {
        let r = v.viewport().image_rect();
        (r.x, r.y)
    }

    fn presses(v: &mut Viewer, key: &str, times: usize) {
        for _ in 0..times {
            press(v, key);
        }
    }

    fn loads(fx: &[Effect]) -> Vec<PathBuf> {
        (fx.iter())
            .filter_map(|e| match e {
                Effect::Load { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    fn names(v: &Viewer) -> Vec<String> {
        (v.files().iter())
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn an_inserted_file_goes_after_the_current_one_and_loads() {
        let mut v = started(Options::default(), 5);
        let fx = v.navigate(2);
        answer(&mut v, fx);
        let fx = v.insert_after_current(PathBuf::from("/pics/2-edit.png"));
        assert_eq!(
            names(&v),
            ["0.png", "1.png", "2.png", "2-edit.png", "3.png", "4.png"]
        );
        assert_eq!(v.current(), 3);
        assert_eq!(loads(&fx), [PathBuf::from("/pics/2-edit.png")]);
        answer(&mut v, fx);
        let fx = v.key(&Keystroke::ctrl("6")).unwrap();
        answer(&mut v, fx);
        assert_eq!(v.current(), 2, "C-6 goes back to the file edited");
    }

    #[test]
    fn a_file_inserted_after_keeps_the_current_one_the_marks_and_the_anchor_of_m() {
        let mut v = started(Options::default(), 5);
        let fx = v.navigate(2);
        answer(&mut v, fx);
        v.toggle_mark(1);
        v.toggle_mark(4);
        let fx = v.insert_after(vec![PathBuf::from("/pics/opened.png")]);
        assert_eq!(
            names(&v),
            ["0.png", "1.png", "2.png", "opened.png", "3.png", "4.png"]
        );
        assert_eq!(v.current(), 2);
        assert!(loads(&fx).is_empty(), "nothing loads");
        assert!(fx.contains(&Effect::Redraw(Part::Bar)), "the bar counts it");
        let marked: Vec<_> = (v.files().iter().enumerate())
            .filter(|(_, f)| f.marked)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, [1, 5]);
        // M marks from 4.png, marked last and now sixth, back to the current file
        press(&mut v, "M");
        assert_eq!(v.mark_count(), 5);
        assert!((1..6).all(|i| v.files()[i].marked));
    }

    #[test]
    fn a_file_inserted_after_leaves_c_6_on_the_file_it_named() {
        // C-6 names 2.png, visited before 1.png, the current file; the insert goes between
        let mut v = started(Options::default(), 3);
        let fx = v.navigate(2);
        answer(&mut v, fx);
        let fx = v.navigate(-1);
        answer(&mut v, fx);
        v.insert_after(vec![PathBuf::from("/pics/opened.png")]);
        assert_eq!(names(&v), ["0.png", "1.png", "opened.png", "2.png"]);
        let fx = v.key(&Keystroke::ctrl("6")).unwrap();
        assert_eq!(loads(&fx), [PathBuf::from("/pics/2.png")]);
    }

    #[test]
    fn several_files_inserted_after_go_in_their_order_and_leave_marks_m_and_c_6_on_theirs() {
        // 4.png visited before 2.png, the current file, which C-6 names; 1.png and 4.png
        // marked, 4.png last, which M marks from
        let mut v = started(Options::default(), 5);
        let fx = v.navigate(4);
        answer(&mut v, fx);
        let fx = v.navigate(-2);
        answer(&mut v, fx);
        v.toggle_mark(1);
        v.toggle_mark(4);
        let opened = ["a.png", "b.png", "c.png"].map(|name| PathBuf::from("/pics").join(name));
        let fx = v.insert_after(opened.to_vec());
        assert_eq!(
            names(&v),
            [
                "0.png", "1.png", "2.png", "a.png", "b.png", "c.png", "3.png", "4.png"
            ]
        );
        assert_eq!(v.current(), 2);
        assert!(loads(&fx).is_empty(), "nothing loads");
        let marked: Vec<_> = (v.files().iter().enumerate())
            .filter(|(_, f)| f.marked)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, [1, 7]);
        // M marks from 4.png, now eighth, back to the current file
        press(&mut v, "M");
        assert_eq!(v.mark_count(), 7);
        assert!((1..8).all(|i| v.files()[i].marked));
        let fx = v.key(&Keystroke::ctrl("6")).unwrap();
        assert_eq!(loads(&fx), [PathBuf::from("/pics/4.png")]);
    }

    #[test]
    fn an_inserted_file_after_the_last_one_ends_the_list() {
        let mut v = started(Options::default(), 3);
        press(&mut v, "G");
        let fx = v.insert_after_current(PathBuf::from("/pics/2-edit.png"));
        assert_eq!(names(&v), ["0.png", "1.png", "2.png", "2-edit.png"]);
        assert_eq!((v.current(), loads(&fx).len()), (3, 1));
    }

    #[test]
    fn marks_stay_with_their_files_when_one_is_inserted() {
        let mut v = started(Options::default(), 5);
        let fx = v.navigate(2);
        answer(&mut v, fx);
        v.toggle_mark(1);
        v.toggle_mark(4);
        let fx = v.insert_after_current(PathBuf::from("/pics/2-edit.png"));
        answer(&mut v, fx);
        let marked: Vec<_> = (v.files().iter().enumerate())
            .filter(|(_, f)| f.marked)
            .map(|(i, _)| i)
            .collect();
        assert_eq!((marked, v.mark_count()), (vec![1, 5], 2));
        // M marks from the file marked last, 4.png, now fifth, to the new current one
        press(&mut v, "M");
        assert_eq!(v.mark_count(), 4);
        assert!(v.files()[3].marked && !v.files()[2].marked);
    }

    #[test]
    fn an_insert_into_an_empty_list_leaves_the_anchor_of_m_on_the_first_file() {
        let mut v = Viewer::new(Options::default(), Vec::new());
        let fx = v.insert_after_current(PathBuf::from("/pics/0.png"));
        answer(&mut v, fx);
        // C-m marks the one file, which M then marks from
        let fx = v.key(&Keystroke::ctrl("m")).unwrap();
        answer(&mut v, fx);
        let fx = v.insert_after_current(PathBuf::from("/pics/1.png"));
        answer(&mut v, fx);
        press(&mut v, "M");
        assert_eq!(v.mark_count(), 2);
    }

    #[test]
    fn an_inserted_file_in_the_grid_is_selected_and_counted() {
        let mut v = started(Options::default(), 4);
        let fx = v.navigate(1);
        answer(&mut v, fx);
        press(&mut v, "Return");
        assert!(v.thumbnail_mode());
        let fx = v.insert_after_current(PathBuf::from("/pics/1-edit.png"));
        assert_eq!((v.grid().count(), v.current()), (5, 2));
        assert!(loads(&fx).is_empty(), "the grid loads no image");
        assert!(fx.contains(&Effect::Redraw(Part::Grid)));
    }

    #[test]
    fn navigation_keys_move_through_the_list() {
        let mut v = started(Options::default(), 12);
        press(&mut v, "]");
        assert_eq!(v.current(), 10, "] goes ten on");
        assert_eq!(
            (beeps(&press(&mut v, "]")), v.current()),
            (1, 10),
            "past the end"
        );
        press(&mut v, "[");
        assert_eq!(v.current(), 0, "[ goes ten back");
        press(&mut v, "G");
        assert_eq!(v.current(), 11);
        press(&mut v, "BackSpace");
        assert_eq!(v.current(), 10, "BackSpace goes back in image mode");
        press(&mut v, "g");
        assert_eq!(beeps(&press(&mut v, "BackSpace")), 1, "before the first");
        let fx = v.key(&Keystroke::ctrl("6")).unwrap();
        answer(&mut v, fx);
        assert_eq!(v.current(), 10, "C-6 goes to the image shown before");
        // filled at 300%, 960 wide: an arrow pans a fifth of the view, 128, and stays
        press(&mut v, "F");
        assert_eq!(pan(&v).0, -160.0);
        press(&mut v, "Right");
        assert_eq!(
            (v.current(), pan(&v).0),
            (10, -288.0),
            "an arrow pans in image mode"
        );
    }

    #[test]
    fn the_bar_starts_hidden_and_b_shows_it() {
        let mut v = started(Options::default(), 1);
        assert!(!v.bar_visible());
        let fx = press(&mut v, "b");
        assert!(v.bar_visible() && fx.contains(&Effect::Layout) && fx.contains(&Effect::FitWindow));
        press(&mut v, "b");
        assert!(!v.bar_visible(), "and b hides it again");
    }

    #[test]
    fn show_bar_shows_it_at_start_and_b_flag_hides_it_whatever_the_config_says() {
        let shown = Config {
            show_bar: true,
            ..Config::default()
        };
        assert!(started_with(Options::default(), &shown, 1).bar_visible());
        let options = run_options(&["-b", "a.png"]);
        assert!(!started_with(options, &shown, 1).bar_visible(), "-b");
        let options = run_options(&["--no-bar", "a.png"]);
        assert!(!started_with(options, &shown, 1).bar_visible(), "--no-bar");
    }

    #[test]
    fn view_keys_zoom_pan_turn_and_color() {
        // 320x160 in a view of 640x480, the bar shown, which b hides
        let shown = Config {
            show_bar: true,
            ..Config::default()
        };
        let mut v = started_with(Options::default(), &shown, 1);
        for (key, zoom) in [("F", 3.0), ("W", 2.0), ("e", 2.0), ("E", 3.0), ("w", 1.0)] {
            press(&mut v, key);
            assert_eq!(v.viewport().zoom(), zoom, "{key}");
        }
        press(&mut v, "F");
        press(&mut v, "=");
        assert_eq!(
            (v.viewport().zoom(), v.viewport().scale_mode()),
            (1.0, ScaleMode::Manual),
            "= zooms to 100%"
        );
        for (key, zoom) in [("+", 1.5), ("+", 2.0), ("-", 1.5)] {
            press(&mut v, key);
            assert_eq!(v.viewport().zoom(), zoom, "{key} steps through the levels");
        }
        // filled at 300%, 960 wide: a fifth of the view is 128, the whole of it 640
        press(&mut v, "F");
        assert_eq!(pan(&v).0, -160.0);
        for (key, x) in [
            ("h", -32.0),
            ("l", -160.0),
            ("Left", -32.0),
            ("H", 0.0),
            ("L", -320.0),
            ("z", -160.0),
        ] {
            press(&mut v, key);
            assert_eq!(pan(&v).0, x, "{key}");
        }
        // at 400%, 640 high: a fifth of the view is 96
        press(&mut v, "+");
        assert_eq!(pan(&v).1, -80.0);
        for (key, y) in [("u", -160.0), ("d", -64.0), ("K", -160.0), ("J", 0.0)] {
            press(&mut v, key);
            assert_eq!(pan(&v).1, y, "{key}");
        }
        // at 800%, 2560 wide, room to pan a whole view, 640, either way from the middle
        press(&mut v, "+");
        press(&mut v, "z");
        assert_eq!(pan(&v).0, -960.0);
        for (key, x) in [("h", -320.0), ("l", -960.0)] {
            let fx = v.key(&Keystroke::ctrl(key)).unwrap();
            answer(&mut v, fx);
            assert_eq!(pan(&v).0, x, "C-{key} pans a whole view");
        }
        press(&mut v, "<");
        assert_eq!(v.rotation(), 270);
        press(&mut v, "?");
        assert_eq!(v.rotation(), 90);
        press(&mut v, "_");
        assert_eq!(v.flips(), (false, true));
        press(&mut v, "a");
        press(&mut v, "A");
        assert!(!v.viewport().antialias && v.viewport().alpha_layer);
        press(&mut v, "}");
        assert!(
            v.status_text().1.starts_with("G+1 | "),
            "{:?}",
            v.status_text()
        );
        presses(&mut v, "{", 2);
        assert_eq!(v.gamma(), -1);
        v.key(&Keystroke::ctrl("g")).unwrap();
        assert_eq!(v.gamma(), 0, "C-g resets gamma");
        presses(&mut v, "{", 40);
        assert_eq!(v.gamma(), -CC_STEPS + 1);
        presses(&mut v, "}", 80);
        assert_eq!(v.gamma(), CC_STEPS);
        press(&mut v, ")");
        assert_eq!(v.contrast(), 1);
        v.key(&Keystroke::ctrl("[")).unwrap();
        presses(&mut v, "(", 40);
        assert_eq!(v.contrast(), -CC_STEPS + 1);
        for _ in 0..80 {
            v.key(&Keystroke::ctrl("]")).unwrap();
        }
        assert_eq!(v.contrast(), CC_STEPS);
        let fx = press(&mut v, "b");
        assert!(
            !v.bar_visible() && fx.contains(&Effect::Layout) && fx.contains(&Effect::FitWindow)
        );
        assert_eq!(press(&mut v, "f"), [Effect::ToggleFullscreen]);
        assert_eq!(press(&mut v, " "), [], "space does nothing in image mode");
        assert_eq!(press(&mut v, "x"), [Effect::EnterEditMode]);
        press(&mut v, "s");
        assert!(v.status_text().1.starts_with("5s | "));
        press(&mut v, "S");
        assert!(v.status_text().1.starts_with("6s | "), "S adds a second");
        press(&mut v, "s");
        assert!(!v.status_text().1.contains("s | "), "s stops the slideshow");
        // S stops at 600 seconds; it only adds, so no key meets its floor of 1
        let mut v = started(
            Options {
                slideshow: Some(599.5),
                ..Options::default()
            },
            1,
        );
        for _ in 0..2 {
            press(&mut v, "S");
            assert!(
                v.status_text().1.starts_with("600s | "),
                "{:?}",
                v.status_text()
            );
        }
    }

    #[test]
    fn mark_and_file_keys_act_on_marks_and_files() {
        let mut v = started(Options::default(), 5);
        press(&mut v, "m");
        assert!(
            v.files()[0].marked && v.current() == 1,
            "m marks and goes on"
        );
        v.key(&Keystroke::ctrl("m")).unwrap();
        assert_eq!(v.mark_count(), 4, "C-m reverses every mark");
        v.key(&Keystroke::ctrl("u")).unwrap();
        assert_eq!(v.mark_count(), 0, "C-u clears them");
        v.toggle_mark(1);
        v.toggle_mark(3);
        press(&mut v, "g");
        for (key, at) in [("N", 1), ("N", 3), ("P", 1)] {
            press(&mut v, key);
            assert_eq!(v.current(), at, "{key}");
        }
        assert_eq!(beeps(&press(&mut v, "P")), 1, "no marked file before");
        let fx = press(&mut v, "r");
        assert!(
            fx.contains(&Effect::InvalidateThumbnails(vec![PathBuf::from(
                "/pics/1.png"
            )])) && fx.iter().any(|e| matches!(e, Effect::Load { .. })),
            "r reads the file again: {fx:?}"
        );
    }

    #[test]
    fn grid_keys_move_and_act_in_the_grid() {
        let grid = Options {
            thumbnail_mode: true,
            ..Options::default()
        };
        // 30 files, four columns of 148
        let mut v = started(grid, 30);
        for (key, at) in [
            ("l", 1),
            ("j", 5),
            ("k", 1),
            ("h", 0),
            ("Right", 1),
            ("Down", 5),
            ("Up", 1),
            ("Left", 0),
            ("BackSpace", 0),
        ] {
            press(&mut v, key);
            assert_eq!(v.current(), at, "{key}");
        }
        press(&mut v, "+");
        assert_eq!(v.grid().thumb_size(), 160.0);
        press(&mut v, "+");
        assert_eq!(v.grid().thumb_size(), 160.0, "the largest size stays");
        press(&mut v, "-");
        assert_eq!(v.grid().thumb_size(), 128.0);
        v.key(&Keystroke::ctrl("j")).unwrap();
        assert_eq!(v.grid().scroll_y(), 480.0, "C-j pages down");
        v.key(&Keystroke::ctrl("k")).unwrap();
        assert_eq!(v.grid().scroll_y(), 0.0);
        let fx = press(&mut v, "R");
        assert!(matches!(&fx[..], [Effect::InvalidateThumbnails(all)] if all.len() == 30));
        assert_eq!(v.key(&Keystroke::new("x")), None, "x is no grid key");
        press(&mut v, " ");
        assert!(!v.thumbnail_mode(), "space opens the image");
    }

    #[test]
    fn animation_keys_play_pause_and_step() {
        let mut v = Viewer::new(Options::default(), files(1));
        let request = load_request(&v.start());
        let gif = ImageInfo {
            delays: vec![0.1, 0.2],
            ..still()
        };
        v.loaded(request, Ok(gif));
        let fx = v.key(&Keystroke::ctrl("a")).unwrap();
        assert!(
            fx.contains(&Effect::CancelTimer(Timer::Animation)),
            "C-a pauses"
        );
        assert!(v.status_text().1.starts_with("⏸[1/2] | "));
        let fx = v.key(&Keystroke::ctrl(" ")).unwrap();
        assert!(
            fx.contains(&Effect::Timer(Timer::Animation, 0.1)),
            "C-Space plays"
        );
        v.key(&Keystroke::ctrl("p")).unwrap();
        assert_eq!(
            v.frame_index(),
            1,
            "C-p steps back, past the first to the last"
        );
        assert!(v.status_text().1.starts_with("⏸[2/2] | "));
    }

    #[test]
    fn the_bar_shows_its_fields_in_order() {
        let mut v = started(Options::default(), 3);
        assert_eq!(
            v.status_text(),
            ("0.png".to_owned(), "320x160 | 100% | 1/3".to_owned())
        );
        v.toggle_mark(0);
        v.toggle_mark(2);
        press(&mut v, "}");
        press(&mut v, "(");
        press(&mut v, "s");
        assert_eq!(
            v.status_text().1,
            "5s | G+1 | C-1 | *2 | * | 320x160 | 100% | 1/3",
            "the slideshow, gamma, contrast, the marks and the current one's, the size, the \
             zoom and the index"
        );
        press(&mut v, "Return");
        assert_eq!(
            v.status_text().1,
            "G+1 | C-1 | *2 | * | 1/3",
            "the grid shows no size or zoom, and stopped the slideshow"
        );
    }

    #[test]
    fn a_load_the_viewer_moved_past_changes_nothing() {
        let mut v = Viewer::new(Options::default(), files(3));
        let first = v.start().iter().find_map(|e| match e {
            Effect::Load { request, .. } => Some(*request),
            _ => None,
        });
        v.key(&Keystroke::new("j"));
        assert_eq!(v.awaited(), Some(first.unwrap() + 1));
        assert_eq!(v.loaded(first.unwrap(), Ok(still())), []);
        assert!(v.image().is_none() && v.current() == 1);
        assert_eq!(
            v.loaded(first.unwrap() + 1, Err(LoadError::Unreadable))
                .last(),
            Some(&Effect::Redraw(Part::Bar))
        );
        assert_eq!(v.error_text(), Some("could not load: 1.png"));
        assert_eq!(v.awaited(), None, "the answer taken, no request waits");
        let pending = load_request(&v.key(&Keystroke::new("j")).unwrap());
        assert_eq!(v.awaited(), Some(pending));
        v.key(&Keystroke::new("Return"));
        assert_eq!(v.awaited(), None, "the grid waits for no image");
    }

    #[test]
    fn the_text_of_what_went_wrong_goes_with_its_file() {
        let mut v = Viewer::new(Options::default(), files(2));
        let request = load_request(&v.start());
        v.loaded(request, Err(LoadError::Unreadable));
        let fx = v.key(&Keystroke::new("j")).unwrap();
        assert!(
            v.error_text().is_none() && fx.contains(&Effect::Redraw(Part::Image)),
            "the canvas no longer shows it: {fx:?}"
        );
        let request = load_request(&fx);
        v.loaded(request, Ok(still()));
        let fx = v.key(&Keystroke::new("k")).unwrap();
        assert!(
            !fx.contains(&Effect::Redraw(Part::Image)),
            "a shown image stays until the next one decodes: {fx:?}"
        );
    }

    #[test]
    fn the_error_text_says_why_a_file_did_not_load() {
        let mut v = Viewer::new(Options::default(), files(1));
        let mut text = |error| {
            let request = load_request(&v.key(&Keystroke::new("r")).unwrap());
            v.loaded(request, Err(error));
            assert!(v.image().is_none());
            v.error_text().map(str::to_owned)
        };
        let size = Size {
            width: 60000.0,
            height: 40000.0,
        };
        assert_eq!(
            text(LoadError::TooLarge(Some(size))).as_deref(),
            Some("too large: 60000x40000")
        );
        assert_eq!(
            text(LoadError::TooLarge(None)).as_deref(),
            Some("too large: 0.png"),
            "the name, when the cap stopped the decoder before the header"
        );
        assert_eq!(
            text(LoadError::Unreadable).as_deref(),
            Some("could not load: 0.png")
        );
        assert_eq!(
            text(LoadError::NoPreview).as_deref(),
            Some("could not load: no preview in RAW")
        );
    }

    #[test]
    fn quitting_prints_the_marks_or_the_current_file() {
        let marks = Options {
            output_marked: true,
            ..Options::default()
        };
        let mut v = started(marks.clone(), 3);
        v.toggle_mark(0);
        v.toggle_mark(2);
        let both = vec![PathBuf::from("/pics/0.png"), PathBuf::from("/pics/2.png")];
        assert_eq!(press(&mut v, "q"), [Effect::Quit { print: both }]);
        let mut v = started(marks, 3);
        v.toggle_mark(0);
        v.select(1);
        let current = vec![PathBuf::from("/pics/1.png")];
        assert_eq!(
            press(&mut v, "Q"),
            [Effect::Quit { print: current }],
            "Q prints the current file, not the marks"
        );
    }

    #[test]
    fn timers_drive_the_slideshow_and_animations() {
        let mut v = started(
            Options {
                slideshow: Some(2.0),
                ..Options::default()
            },
            2,
        );
        let fx = v.timer(Timer::Slideshow);
        assert!(
            answer(&mut v, fx).contains(&Effect::Timer(Timer::Slideshow, 2.0)) && v.current() == 1
        );
        let fx = v.timer(Timer::Slideshow);
        answer(&mut v, fx);
        assert_eq!(v.current(), 0, "the slideshow wraps, like nsxiv");
        let mut v = Viewer::new(Options::default(), files(1));
        let fx = v.start();
        let Some(Effect::Load { request, .. }) =
            fx.into_iter().find(|e| matches!(e, Effect::Load { .. }))
        else {
            panic!()
        };
        let gif = ImageInfo {
            delays: vec![0.1, 0.2],
            ..still()
        };
        assert!(
            v.loaded(request, Ok(gif))
                .contains(&Effect::Timer(Timer::Animation, 0.1))
        );
        assert_eq!(
            v.timer(Timer::Animation).last(),
            Some(&Effect::Timer(Timer::Animation, 0.2))
        );
        assert_eq!(v.frame_index(), 1);
        let fx = v.key(&Keystroke::ctrl("n")).unwrap();
        assert!(
            fx.contains(&Effect::CancelTimer(Timer::Animation)) && v.frame_index() == 0,
            "C-n stops and steps"
        );
    }

    #[test]
    fn an_edit_stops_the_slideshow_and_pauses_an_animation_for_good() {
        let mut v = Viewer::new(
            Options {
                slideshow: Some(2.0),
                ..Options::default()
            },
            files(2),
        );
        let fx = v.start();
        let Some(Effect::Load { request, .. }) =
            fx.into_iter().find(|e| matches!(e, Effect::Load { .. }))
        else {
            panic!()
        };
        let gif = ImageInfo {
            delays: vec![0.1, 0.2],
            ..still()
        };
        v.loaded(request, Ok(gif));
        let fx = v.edit_entered();
        assert!(
            fx.contains(&Effect::CancelTimer(Timer::Slideshow)),
            "{fx:?}"
        );
        assert!(
            fx.contains(&Effect::CancelTimer(Timer::Animation)),
            "{fx:?}"
        );
        // neither runs on: a second edit has nothing to stop
        let fx = v.edit_entered();
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::CancelTimer(_))),
            "{fx:?}"
        );
    }

    #[test]
    fn an_animation_still_decoding_stays_paused_after_an_edit_until_played() {
        let mut v = Viewer::new(Options::default(), files(1));
        let fx = v.start();
        let Some(Effect::Load { request, .. }) =
            fx.into_iter().find(|e| matches!(e, Effect::Load { .. }))
        else {
            panic!()
        };
        // its first frame alone, the rest still to decode
        let first = ImageInfo {
            delays: vec![0.1],
            complete: false,
            ..still()
        };
        v.loaded(request, Ok(first));
        v.edit_entered();
        let fx = v.frames_decoded(request, 1, &[0.2], true);
        assert!(
            !fx.iter()
                .any(|e| matches!(e, Effect::Timer(Timer::Animation, _))),
            "{fx:?}"
        );
        // played again, it plays
        let fx = v.key(&Keystroke::ctrl("a")).unwrap();
        assert!(fx.contains(&Effect::Timer(Timer::Animation, 0.1)), "{fx:?}");
    }

    #[test]
    fn the_key_handler_gets_the_marks_and_files_it_deleted_go() {
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-handler-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list: Vec<FileEntry> = ["a.png", "b.png", "c.png"]
            .iter()
            .map(|name| {
                std::fs::write(dir.join(name), b"").unwrap();
                FileEntry::new(dir.join(name))
            })
            .collect();
        let mut v = Viewer::new(Options::default(), list);
        let fx = v.start();
        answer(&mut v, fx);
        v.toggle_mark(0);
        v.toggle_mark(2);
        assert_eq!(v.key(&Keystroke::ctrl("x")), Some(vec![]));
        let run = Effect::RunKeyHandler {
            key: "c".into(),
            files: vec![dir.join("a.png"), dir.join("c.png")],
        };
        assert_eq!(v.key(&Keystroke::new("c")), Some(vec![run]));
        std::fs::remove_file(dir.join("c.png")).unwrap();
        let fx = v.key_handler_done();
        assert_eq!(
            (v.files().len(), v.mark_count()),
            (2, 1),
            "the deleted file leaves the list"
        );
        assert!(
            fx.iter().any(|e| matches!(e, Effect::Load { .. })),
            "the current file reloads"
        );
        v.key(&Keystroke::ctrl("x"));
        assert_eq!(
            v.key(&Keystroke::new("Escape")),
            Some(vec![]),
            "Escape aborts, as in nsxiv"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removal_keeps_indices_on_their_files() {
        assert_eq!(
            (0..=3).map(|i| remap(i, &[0, 2, 3])).collect::<Vec<_>>(),
            [0, 1, 1, 2]
        );
        assert_eq!(
            remap(2, &[0, 1]),
            1,
            "removing the last file lands on the new last"
        );
        let grid = Options {
            thumbnail_mode: true,
            ..Options::default()
        };
        let mut v = started(grid, 1);
        v.toggle_mark(0);
        assert_eq!(press(&mut v, "D"), [Effect::Quit { print: vec![] }]);
        assert_eq!(
            (
                v.files().len(),
                v.grid().count(),
                v.grid().visible(),
                v.mark_count()
            ),
            (0, 0, 0..0, 0),
            "the grid and the marks empty with the list"
        );
    }

    #[test]
    fn a_narrow_bar_drops_fields_from_the_front() {
        let status = "200x200 | 100% | 2/4";
        let fitted = |room| fitted_status(status, room, |s| s.chars().count() as f64);
        assert_eq!(
            [fitted(20.0), fitted(19.0), fitted(9.0), fitted(2.0)],
            [status, "100% | 2/4", "2/4", "2/4"]
        );
    }

    #[test]
    fn windows_grow_within_their_room() {
        // a 200-wide window 1000 from the room's left edge, 80 from its right
        assert_eq!(
            grown_limit(200, -1000.0, 280.0, true),
            280,
            "a window that keeps its top-left corner grows into the room after it"
        );
        assert_eq!(grown_limit(200, -1000.0, 280.0, false), 280);
        assert_eq!(
            grown_limit(200, -10.0, 1000.0, false),
            220,
            "where the anchoring is unknown, growth fits a centered window too"
        );
        assert_eq!(
            grown_limit(200, 50.0, 1000.0, false),
            i64::MAX,
            "a window parked across an edge grows freely"
        );
        assert_eq!(grown_limit(200, -10.0, 150.0, true), i64::MAX);
    }

    fn load_request(fx: &[Effect]) -> u64 {
        fx.iter()
            .find_map(|e| match e {
                Effect::Load { request, .. } => Some(*request),
                _ => None,
            })
            .expect("a load")
    }

    #[test]
    fn slow_loads_keep_the_state_straight() {
        let mut v = Viewer::new(
            Options {
                slideshow: Some(2.0),
                ..Options::default()
            },
            files(3),
        );
        let first = load_request(&v.start());
        assert!(
            !v.timer(Timer::Slideshow)
                .iter()
                .any(|e| matches!(e, Effect::Load { .. })),
            "a tick waits for the image"
        );
        assert!(
            v.loaded(first, Ok(still()))
                .contains(&Effect::Timer(Timer::Slideshow, 2.0)),
            "the wait ends once it shows"
        );
        let fx = v.key(&Keystroke::new("j")).unwrap();
        assert!(
            fx.contains(&Effect::Redraw(Part::Bar)),
            "the bar names the next file at once"
        );
        v.key(&Keystroke::new(">"));
        v.loaded(load_request(&fx), Ok(still()));
        assert_eq!(
            v.display_size(),
            Some(Size {
                width: 160.0,
                height: 320.0
            }),
            "a turn pressed while loading applies"
        );
        let pending = load_request(&v.key(&Keystroke::new("j")).unwrap());
        v.key(&Keystroke::new("Return"));
        assert_eq!(
            v.loaded(pending, Ok(still())),
            [],
            "the grid drops the load"
        );
    }

    #[test]
    fn reloads_and_double_clicks_finish_their_work() {
        let mut v = Viewer::new(
            Options {
                zoom: Some(2.0),
                ..Options::default()
            },
            files(2),
        );
        v.resized(Size {
            width: 640.0,
            height: 480.0,
        });
        v.start();
        let gif = ImageInfo {
            delays: vec![0.1, 0.2],
            ..still()
        };
        let reload = load_request(&v.key(&Keystroke::new("r")).unwrap());
        v.loaded(reload, Ok(gif));
        assert_eq!(
            v.viewport().zoom(),
            2.0,
            "-z survives a reload before the first decode"
        );
        let reload = load_request(&v.key(&Keystroke::new("r")).unwrap());
        v.key(&Keystroke::ctrl("a"));
        let fx = v.loaded(reload, Ok(still()));
        assert!(
            !fx.iter()
                .any(|e| matches!(e, Effect::Timer(Timer::Animation, _))),
            "a still does not animate"
        );
        v.key(&Keystroke::new("Return"));
        let fx = v.open(1);
        assert!(
            fx.contains(&Effect::Layout) && fx.iter().any(|e| matches!(e, Effect::Load { .. }))
        );
    }

    #[test]
    fn each_slideshow_image_gets_its_whole_delay() {
        let mut v = Viewer::new(
            Options {
                slideshow: Some(2.0),
                ..Options::default()
            },
            files(3),
        );
        let fx = v.start();
        assert!(
            !fx.contains(&Effect::Timer(Timer::Slideshow, 2.0)),
            "no countdown before the first image shows"
        );
        assert!(
            v.loaded(load_request(&fx), Ok(still()))
                .contains(&Effect::Timer(Timer::Slideshow, 2.0))
        );
        let fx = v.timer(Timer::Slideshow);
        assert!(
            !fx.contains(&Effect::Timer(Timer::Slideshow, 2.0)),
            "nor before the next one shows"
        );
        assert!(
            v.loaded(load_request(&fx), Ok(still()))
                .contains(&Effect::Timer(Timer::Slideshow, 2.0))
        );
    }

    #[test]
    fn a_reload_before_a_new_file_shows_loads_it_as_new() {
        let mut v = started(Options::default(), 2);
        for key in ["=", "+", "+", "+", "L"] {
            press(&mut v, key);
        }
        assert_eq!(
            v.viewport().image_rect().x,
            -640.0,
            "400%, panned to the right edge"
        );
        v.key(&Keystroke::new("j"));
        let fx = v.key(&Keystroke::new("r")).unwrap();
        v.loaded(load_request(&fx), Ok(still()));
        assert_eq!(
            v.viewport().image_rect().x,
            -320.0,
            "the new file shows centered, not at the old pan"
        );
    }

    #[test]
    fn an_animation_plays_its_frames_as_they_decode() {
        let mut v = Viewer::new(Options::default(), files(2));
        let request = load_request(&v.start());
        v.loaded(request, Ok(still()));
        assert_eq!(
            v.frames_decoded(request + 1, 0, &[0.1, 0.2], false),
            [],
            "another image's frames change nothing"
        );
        assert!(
            v.frames_decoded(request, 0, &[0.1, 0.2], false)
                .contains(&Effect::Timer(Timer::Animation, 0.1)),
            "a second frame starts the animation"
        );
        v.timer(Timer::Animation);
        assert_eq!(v.frame_index(), 1);
        assert_eq!(
            v.timer(Timer::Animation),
            [],
            "the third frame has yet to decode"
        );
        let fx = v.frames_decoded(request, 2, &[0.3], false);
        assert!(
            v.frame_index() == 2 && fx.contains(&Effect::Timer(Timer::Animation, 0.3)),
            "it shows once it does"
        );
        v.key(&Keystroke::ctrl("n"));
        assert_eq!(v.frame_index(), 2, "stepping stops at the last frame known");
        assert!(
            v.frames_decoded(request, 3, &[], true)
                .contains(&Effect::Redraw(Part::Image)),
            "a frame that arrives while paused shows"
        );
        v.key(&Keystroke::ctrl("n"));
        assert_eq!(v.frame_index(), 0, "and wraps once the count is known");
    }

    #[test]
    fn the_config_sets_the_delay_the_color_steps_and_the_views() {
        let config = Config {
            slideshow_delay: 3.0,
            cc_steps: 2,
            gamma_max: 3.0,
            zoom_levels: vec![1.0, 5.0],
            thumb_sizes: vec![50.0],
            thumb_size_index: 0,
            ..Config::default()
        };
        let mut v = Viewer::with_config(Options::default(), files(2), &config);
        v.resized(Size {
            width: 640.0,
            height: 480.0,
        });
        let fx = v.start();
        answer(&mut v, fx);
        press(&mut v, "s");
        assert!(v.status_text().1.starts_with("3s | "), "the config's delay");
        for _ in 0..3 {
            press(&mut v, "}");
        }
        assert_eq!(
            (v.gamma(), v.gamma_value()),
            (2, 3.0),
            "two steps reach the config's top"
        );
        for _ in 0..3 {
            press(&mut v, "{");
        }
        assert_eq!((v.gamma(), v.gamma_value()), (-1, 0.5));
        press(&mut v, "+");
        assert_eq!(v.viewport().zoom(), 5.0, "the config's zoom levels");
        assert_eq!(v.grid().thumb_size(), 50.0, "and thumbnail sizes");
        let options = Options {
            slideshow: Some(2.0),
            ..Options::default()
        };
        let v = Viewer::with_config(options, files(1), &config);
        assert_eq!(v.slideshow_delay, 2.0, "-S wins over the config");
        let one = Config {
            cc_steps: 1,
            ..Config::default()
        };
        let mut v = Viewer::with_config(Options::default(), files(1), &one);
        let fx = v.start();
        answer(&mut v, fx);
        press(&mut v, "{");
        assert_eq!(v.gamma(), -1, "a step down exists with too few steps");
    }

    #[test]
    fn a_huge_color_top_stays_finite() {
        assert_eq!(color_factor(32, 32, 1e308), 1e308);
        assert_eq!(color_factor(16, 32, 1e308), 1e308 / 2.0);
        assert_eq!(color_value(16, crate::config::GAMMA_MAX), 5.5);
    }

    #[test]
    fn a_file_picked_during_a_slideshow_gets_the_whole_delay() {
        let mut v = started(
            Options {
                slideshow: Some(2.0),
                ..Options::default()
            },
            3,
        );
        let fx = v.key(&Keystroke::new("j")).unwrap();
        let shown = v.loaded(load_request(&fx), Ok(still()));
        assert!(shown.contains(&Effect::Timer(Timer::Slideshow, 2.0)));
    }
}
