//! The terminal frontend: frames as kitty graphics, sixels or half-block cells, and keys from
//! the terminal's input.
//!
//! The viewer takes `/dev/tty` in raw mode on the alternate screen, and puts it back on every
//! way out, a panic included. The view is in units of a sixteenth of a row's height, so zoom
//! reads the same in every mode. A redraw sends the whole frame, or the status line alone where
//! only it changed, inside synchronized output, with the status line on the last row. Around
//! the image, the frame is clear, so the terminal's own background shows.
//!
//! A kitty terminal keeps the frames it got, within a budget, and shows one again for a
//! placement of a few dozen bytes. A terminal on the same machine reads each new frame as raw
//! pixels from a file the viewer writes; a remote one gets it as PNG. In tmux, kitty graphics
//! go through its passthrough to the terminal around it, and each image shows through the
//! placeholders tmux keeps and redraws with the pane.

mod ahead;
pub mod frame;
pub mod keep;
pub mod keys;
pub mod kitty;
pub mod text;
pub mod tmux;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use base64::prelude::{BASE64_STANDARD, Engine};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, PostAction, RegistrationToken};
use nsxiv_ng_core::Size;
use nsxiv_ng_core::config::Config;
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::Options;
use nsxiv_ng_core::viewer::{Keystroke, Viewer};
use nsxiv_ng_render::Canvas;
use nsxiv_ng_session::{Bar, Frame, Request, Session, key_handlers};
use rustix::termios::{
    InputModes, LocalModes, OptionalActions, OutputModes, SpecialCodeIndex, Termios, Winsize,
    tcgetattr, tcgetwinsize, tcsetattr,
};

use crate::driver::{self, Driver, Frontend, Handle};
use frame::ColorDepth;
use keep::{BUDGET, FrameKey, Kept};

/// How long the rest of an input sequence may take before what came counts as it is, so a
/// lone ESC is the Escape key.
const IDLE: Duration = Duration::from_millis(100);

/// The terminal the viewer runs in, before it takes it.
pub struct Tty {
    file: File,
    mode: Termios,
}

/// The controlling terminal, None without one.
pub fn open() -> Option<Tty> {
    let file = File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let mode = tcgetattr(&file).ok()?;
    Some(Tty { file, mode })
}

impl Tty {
    /// Runs the viewer of `files` in the terminal until it quits, then exits the process with
    /// status 0. `program` is the viewer's own binary. Returns only an error that keeps the
    /// viewer from running, the terminal put back.
    pub fn run(
        self,
        options: Options,
        files: Vec<FileEntry>,
        config: &Config,
        program: PathBuf,
    ) -> io::Error {
        driver::run(
            |handle| Terminal::new(self, handle),
            |terminal, wake| {
                let bar = terminal.bar();
                let mut session =
                    Session::new(options, files, config, program, key_handlers(), bar, wake);
                if matches!(terminal.mode, Mode::Kitty | Mode::Sixel) {
                    session.want_stills();
                }
                // a terminal's frame costs an image, so thumbnails that come one by one wait
                session.wait_for_thumbnails(Duration::from_millis(300));
                session
            },
        )
    }
}

/// How the terminal shows frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Kitty,
    Sixel,
    HalfBlocks(ColorDepth),
}

/// Kitty graphics where the probe found them; else sixels where the terminal has them and gives
/// its size in pixels, which they are drawn at; else half blocks, which every terminal shows.
fn mode(kitty: bool, sixel: bool, pixels: bool, depth: ColorDepth) -> Mode {
    if kitty {
        Mode::Kitty
    } else if sixel && pixels {
        Mode::Sixel
    } else {
        Mode::HalfBlocks(depth)
    }
}

/// The terminal's size in cells, and a cell's pixels where the terminal gives them, else the
/// usual 1:2 shape.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cells {
    columns: usize,
    rows: usize,
    cell: (f64, f64),
    /// Whether the terminal gave its pixels.
    pixels: bool,
}

impl Default for Cells {
    fn default() -> Cells {
        Cells {
            columns: 80,
            rows: 24,
            cell: (8.0, 16.0),
            pixels: false,
        }
    }
}

impl Cells {
    /// The size the terminal reports, where it gives one. A cell's pixels are whole, as a
    /// window may report a few pixels beyond its last cell.
    fn measured(self, size: Winsize) -> Cells {
        let (columns, rows) = (usize::from(size.ws_col), usize::from(size.ws_row));
        if columns == 0 || rows == 0 {
            return self;
        }
        let pixels = size.ws_xpixel >= size.ws_col && size.ws_ypixel >= size.ws_row;
        let cell = match pixels {
            true => (
                f64::from(size.ws_xpixel / size.ws_col),
                f64::from(size.ws_ypixel / size.ws_row),
            ),
            false => self.cell,
        };
        Cells {
            columns,
            rows,
            cell,
            pixels,
        }
    }

    /// The rows above the status line.
    /// A cell's pixels, which a terminal gives in whole ones.
    fn cell_pixels(&self) -> (u32, u32) {
        (self.cell.0 as u32, self.cell.1 as u32)
    }

    fn image_rows(&self, bar: bool) -> usize {
        self.rows.saturating_sub(usize::from(bar))
    }

    /// A sixel image's pixels: the columns', and whole bands of 6 rows short of the screen's
    /// last row, where the status line goes, and where an image would scroll the screen in
    /// most terminals.
    fn sixel_pixels(&self) -> (u32, u32) {
        let (width, height) = (self.cell.0 as usize, self.cell.1 as usize);
        let rows = self.rows.saturating_sub(1);
        (
            (self.columns * width) as u32,
            (rows * height / 6 * 6) as u32,
        )
    }

    /// The window in units of a sixteenth of a cell's height: every row, the status line's
    /// among them, or for sixels their band area alone.
    fn window(&self, mode: Mode) -> Size {
        let (width, height) = self.cell;
        if mode == Mode::Sixel {
            let (w, h) = self.sixel_pixels();
            return Size {
                width: f64::from(w) * 16.0 / height,
                height: f64::from(h) * 16.0 / height,
            };
        }
        Size {
            width: self.columns as f64 * 16.0 * width / height,
            height: self.rows as f64 * 16.0,
        }
    }

    /// The terminal's pixels to a unit, where it gives them.
    fn scale(&self) -> f64 {
        if self.pixels { self.cell.1 / 16.0 } else { 1.0 }
    }

    /// Whether a frame has room: rows above the bar, and for sixels a whole band.
    fn has_area(&self, mode: Mode, bar: bool) -> bool {
        self.columns > 0
            && self.image_rows(bar) > 0
            && (mode != Mode::Sixel || self.sixel_pixels().1 > 0)
    }
}

/// The terminal's input not yet taken as keys: a sequence a read ended inside, and at the start
/// what came during the probe, so it comes before any later read.
#[derive(Debug, Default)]
struct Input {
    rest: Vec<u8>,
}

impl Input {
    /// The keys in the rest and then `bytes`, a held key's repeats once; a sequence they end
    /// inside waits as the rest.
    fn take(&mut self, bytes: &[u8]) -> Vec<Option<Keystroke>> {
        let joined = [&self.rest[..], bytes].concat();
        let (keys, rest) = keys::keys(&joined);
        self.rest = rest.to_vec();
        keys::collapsing_repeats(&keys)
    }

    fn waiting(&self) -> bool {
        !self.rest.is_empty()
    }

    /// What the rest is once no more input came: a lone ESC the Escape key, ESC _ Alt and `_`,
    /// anything else a key without a name.
    fn idle(&mut self) -> Option<Keystroke> {
        match std::mem::take(&mut self.rest)[..] {
            [0x1b] => Some(Keystroke::new("Escape")),
            [0x1b, 0x5f] => Some(Keystroke::new("_")),
            _ => None,
        }
    }
}

/// The terminal as the viewer found it, for every way out to put back: the mode, the kitty
/// images on the alternate screen, and the folder of pixel files.
struct Saved {
    tty: File,
    mode: Termios,
    kitty: bool,
    tmux: bool,
    files: Option<PathBuf>,
}

static SAVED: Mutex<Option<Saved>> = Mutex::new(None);

/// The thread that took the terminal, which puts it back on a panic.
static OWNER: OnceLock<ThreadId> = OnceLock::new();

/// Whether the viewer has the screen, and what a panic on another thread said meanwhile, for
/// stderr once the screen is the user's again, where it would otherwise land over the frame.
/// One lock for both, so no message comes in between the screen given back and the messages
/// taken.
static PANICS: Mutex<(bool, Vec<String>)> = Mutex::new((false, Vec::new()));

fn panics() -> std::sync::MutexGuard<'static, (bool, Vec<String>)> {
    PANICS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn with_saved(f: impl FnOnce(&mut Saved)) {
    if let Some(saved) = SAVED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        f(saved);
    }
}

/// A panic on the thread that took the terminal puts the terminal back before its message; one
/// on another thread, as a worker's, while the viewer has the screen, waits for the screen to
/// be the user's, as the viewer goes on. True when the message is held.
fn hold_panic(message: &str) -> bool {
    let owner = OWNER.get() == Some(&thread::current().id());
    if owner {
        // shortcut: in tmux, the viewer's images stay in the terminal around tmux after this,
        // as delete_all sends nothing there; the ids kept where this hook reaches them would
        // let it delete them
        // try_lock, as the panic may come while the lock is held
        if let Ok(saved) = SAVED.try_lock()
            && let Some(saved) = saved.as_ref()
        {
            leave(saved);
        }
        return false;
    }
    let mut panics = panics();
    if !panics.0 {
        return false;
    }
    let name = thread::current().name().unwrap_or("<unnamed>").to_owned();
    let lines = message.lines().map(crate::printable);
    let text = format!("thread '{name}' {}", lines.collect::<Vec<_>>().join("\n"));
    panics.1.push(text);
    true
}

/// Keys as they come and without echo, C-c and C-z among them, output turning "\n" into
/// "\r\n"; then the alternate screen, with no cursor and no wrapping, so a line too long cannot
/// scroll it.
fn enter(saved: &Saved) {
    let mut raw = saved.mode.clone();
    raw.input_modes.remove(
        InputModes::BRKINT
            | InputModes::ICRNL
            | InputModes::INPCK
            | InputModes::ISTRIP
            | InputModes::IXON,
    );
    raw.output_modes
        .insert(OutputModes::OPOST | OutputModes::ONLCR);
    raw.local_modes
        .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::IEXTEN | LocalModes::ISIG);
    raw.special_codes[SpecialCodeIndex::VMIN] = 1;
    raw.special_codes[SpecialCodeIndex::VTIME] = 0;
    let _ = tcsetattr(&saved.tty, OptionalActions::Flush, &raw);
    let _ = (&saved.tty).write_all(b"\x1b[?1049h\x1b[?25l\x1b[?7l");
    panics().0 = true;
}

/// The terminal as it was found; the frame's image goes with the alternate screen, the pixel
/// files with their folder, and what other threads' panics said goes to stderr.
fn leave(saved: &Saved) {
    let images = if saved.kitty {
        kitty::delete_all(saved.tmux)
    } else {
        String::new()
    };
    let back = format!("{images}\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l");
    let _ = (&saved.tty).write_all(back.as_bytes());
    let _ = tcsetattr(&saved.tty, OptionalActions::Flush, &saved.mode);
    if let Some(files) = &saved.files {
        let _ = std::fs::remove_dir_all(files);
    }
    let held = {
        let mut panics = panics();
        panics.0 = false;
        std::mem::take(&mut panics.1)
    };
    for panic in held {
        eprintln!("{panic}");
    }
}

/// A folder of the viewer's own for pixel files, which only it reads and writes. The terminal
/// deletes a file it read (t=t) only in a temporary folder whose path holds
/// `tty-graphics-protocol`.
fn files_folder() -> Option<PathBuf> {
    let base = format!("/tmp/tty-graphics-protocol-nsxiv-{}", std::process::id());
    (0..100).find_map(|n| {
        let folder = PathBuf::from(format!("{base}-{n}"));
        // a folder made here, not one in its place, as create_dir follows no link
        let made = std::fs::DirBuilder::new().mode(0o700).create(&folder);
        made.is_ok().then_some(folder)
    })
}

/// The frame's pixels as raw RGB, or RGBA with `alpha`, in the file at `path`; false when the
/// file could not take them whole, as on a full disk, which the terminal would answer only
/// with an error the viewer does not ask for.
fn write_pixel_file(path: &Path, frame: &[u32], alpha: bool) -> bool {
    let written = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(&kitty::pixel_file(frame, alpha)));
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written.is_ok()
}

/// The terminal frontend, in the driver's loop.
pub struct Terminal {
    tty: File,
    mode: Mode,
    /// Kitty graphics through tmux, to the terminal around it, shown by placeholders.
    tmux: bool,
    /// The folder of pixel files the terminal reads itself, where it can.
    files: Option<PathBuf>,
    cells: Cells,
    /// The frame's pixels, kept between frames.
    canvas: Vec<u32>,
    /// The kitty frames the terminal holds, the one on screen, 0 for none, and the id given last;
    /// in sixel mode, the frames kept by id in `sixels`, as a sixel terminal keeps none.
    kept: Kept,
    sixels: HashMap<u32, String>,
    shown: u32,
    last_id: u32,
    /// The bytes the terminal stores for the frame on screen, kept or not.
    shown_bytes: usize,
    /// Whether a kitty frame went yet, and whether what other programs left on the screen
    /// went, as before the first frame and after the key-handler.
    drawn: bool,
    cleared: bool,
    /// Frames of the neighbors made ahead: the files the session named since the viewer last
    /// moved, the frames that would not fit, the one being made, and what tells its job is done.
    named: Vec<PathBuf>,
    skipped: Vec<FrameKey>,
    making: Option<Making>,
    made: calloop::channel::Sender<u64>,
    /// What a frame made ahead must be made for, as frames kept for an old layout or file
    /// are forgotten: it counts the times they were.
    generation: u64,
    ahead_files: u64,
    /// The current file when the viewer last moved, and whether it went forward to it.
    current: usize,
    forward: bool,
    /// herdr sends an image on to its client, across SSH for a remote one, only once it is
    /// placed, which for a frame of 2560x1600 takes over a second there: a frame sent ahead
    /// also gets a placement of one cell, under the background of the status bar's last cell,
    /// so it is on the client by the time it shows; and whether such placements are out.
    place_ahead: bool,
    placed_ahead: bool,
    /// The input not yet taken as keys, and the timer that takes it as it is.
    input: Input,
    idle: Option<RegistrationToken>,
    handle: Handle<Terminal>,
}

impl Terminal {
    fn new(tty: Tty, handle: &Handle<Terminal>) -> io::Result<Terminal> {
        // blocked here, before the session's threads start; a new font may keep the cells and
        // change their pixels, so every one lays out and redraws
        let resized = Signals::new(&[Signal::SIGWINCH])?;
        driver::insert(handle, resized, |_, _, driver| {
            let terminal = driver.frontend();
            terminal.measure();
            // kept frames fit the cells they were placed in
            terminal.forget_all();
            driver.layout();
        })?;
        let reads = Generic::new(tty.file.try_clone()?, Interest::READ, calloop::Mode::Level);
        driver::insert(handle, reads, |_, file, driver| {
            let mut bytes = [0; 4096];
            match (&**file).read(&mut bytes) {
                Ok(0) => driver.quit(), // the terminal went away
                Ok(n) => input(driver, &bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => driver.quit(),
            }
            Ok(PostAction::Continue)
        })?;
        let (made, frames) = calloop::channel::channel();
        driver::insert(handle, frames, |event, _, driver| {
            if let calloop::channel::Event::Msg(job) = event {
                driver.frontend_event(|terminal, session| {
                    terminal.ahead_done(session, job);
                    Vec::new()
                });
            }
        })?;
        let cells = Cells::default().measured(tcgetwinsize(&tty.file)?);
        // nothing fails from here, where the terminal is no longer as it was found
        let files = files_folder();
        let saved = Saved {
            tty: tty.file.try_clone()?,
            mode: tty.mode.clone(),
            kitty: false,
            tmux: false,
            files: files.clone(),
        };
        let _ = OWNER.set(thread::current().id());
        enter(&saved);
        *SAVED.lock().unwrap_or_else(PoisonError::into_inner) = Some(saved);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !hold_panic(&info.to_string()) {
                previous(info);
            }
        }));

        let (graphics, tmux, probed) = graphics(&tty.file, files.as_deref());
        let files = match files {
            Some(folder) if !graphics.files => {
                let _ = std::fs::remove_dir_all(&folder);
                None
            }
            files => files,
        };
        let colorterm = std::env::var("COLORTERM").ok();
        let term = std::env::var("TERM").ok();
        let depth = frame::color_depth(colorterm.as_deref(), term.as_deref());
        let mode = mode(graphics.kitty, graphics.sixel, cells.pixels, depth);
        let tmux = tmux && mode == Mode::Kitty;
        with_saved(|saved| {
            saved.kitty = mode == Mode::Kitty;
            saved.tmux = tmux;
            saved.files = files.clone();
        });
        // the keys typed during the probe, once the viewer started, unless a read took them
        // first, as they wait before anything read later
        if !probed.is_empty() {
            let typed = |_, _: &mut (), driver: &mut Driver<Terminal>| {
                input(driver, &[]);
                TimeoutAction::Drop
            };
            let _ = driver::insert(handle, Timer::immediate(), typed);
        }
        Ok(Terminal {
            tty: tty.file,
            mode,
            tmux,
            files,
            cells,
            canvas: Vec::new(),
            kept: Kept::default(),
            sixels: HashMap::new(),
            shown: 0,
            last_id: first_id(tmux),
            shown_bytes: 0,
            drawn: false,
            cleared: false,
            named: Vec::new(),
            skipped: Vec::new(),
            making: None,
            made,
            generation: 0,
            ahead_files: 0,
            current: 0,
            forward: true,
            place_ahead: std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "herdr"),
            placed_ahead: false,
            input: Input { rest: probed },
            idle: None,
            handle: handle.clone(),
        })
    }

    /// Where the status line shows: on the last row, which the session takes off the window
    /// while it shows, or below the sixels' band area, which leaves the last row out already.
    fn bar(&self) -> Bar {
        match self.mode {
            Mode::Sixel => Bar::Shown(0.0),
            _ => Bar::Shown(16.0),
        }
    }

    fn measure(&mut self) {
        if let Ok(size) = tcgetwinsize(&self.tty) {
            self.cells = self.cells.measured(size);
        }
    }

    fn write(&self, bytes: &[u8]) {
        // a terminal that went away hangs up, which ends the viewer
        let _ = (&self.tty).write_all(bytes);
    }

    // kitty images out of the terminal, with their pixels, but the one on screen, which goes
    // once another replaces it; kept sixel frames out of the viewer's memory, the terminal
    // getting nothing, as one without kitty graphics may print the command as text
    fn delete(&mut self, ids: &[u32]) {
        if self.mode != Mode::Kitty {
            for id in ids {
                self.sixels.remove(id);
            }
            return;
        }
        let gone = ids.iter().filter(|&&id| id != self.shown);
        let out: String = gone.map(|&id| kitty::delete(id, self.tmux)).collect();
        self.write(out.as_bytes());
    }

    // Every kept frame out of the terminal, as for another layout, and frames being made for
    // the one before.
    fn forget_all(&mut self) {
        let ids = self.kept.clear();
        self.delete(&ids);
        self.generation += 1;
        self.skipped.clear();
        self.placed_ahead = false;
    }

    // Keeps up with the viewer: the way it went, and the files named for the place it left.
    fn follow(&mut self, viewer: &Viewer) {
        let current = viewer.current();
        if current != self.current {
            self.forward = current > self.current;
            self.current = current;
            self.named.clear();
            self.skipped.clear();
        }
    }

    // The current file's neighbors, the one the viewer is going to first.
    fn neighbors(&self, viewer: &Viewer) -> Vec<PathBuf> {
        let current = viewer.current();
        let (next, back) = (current.checked_add(1), current.checked_sub(1));
        let order = if self.forward {
            [next, back]
        } else {
            [back, next]
        };
        let files = viewer.files();
        order
            .into_iter()
            .flatten()
            .filter_map(|i| Some(files.get(i)?.path.clone()))
            .collect()
    }

    // While the viewer waits for a key, a frame of the next file, then of the one before, goes
    // to the terminal in the view it will have, or as sixels into the viewer's own keep, one at
    // a time, made on a thread of its own: those the session named, not kept yet, and that fit
    // beside what must stay.
    fn prefetch(&mut self, session: &Session) {
        let viewer = session.viewer();
        let bar = viewer.bar_visible();
        // kitty frames once the screen is the viewer's, as deleting what other programs left
        // would take the frames sent ahead with it
        let (kitty, sixel) = (self.mode == Mode::Kitty, self.mode == Mode::Sixel);
        if self.making.is_some()
            || (kitty && !self.cleared)
            || !(kitty || sixel)
            || viewer.thumbnail_mode()
            || !self.cells.has_area(self.mode, bar)
        {
            return;
        }
        let neighbors = self.neighbors(viewer);
        let layout = (self.cells.columns, self.cells.image_rows(bar));
        let steps = (viewer.gamma(), viewer.contrast());
        for path in &neighbors {
            // the next file in the way the viewer goes comes first, as the one before must
            // leave room for its frame
            // shortcut: a next file the session never names, as one that does not decode,
            // keeps the one before from going ahead; word from the session of a decode that
            // failed would let it go
            if !self.named.contains(path) {
                return;
            }
            let Some(still) = session.still(path) else {
                continue;
            };
            // shortcut: a neighbor that animates gets a frame ahead too, which is never placed,
            // as a frame of an animation has no key; the session cannot tell before the move
            // whether a file animates, as any PNG may
            let modes = (self.tmux, sixel);
            let (key, crop, scale) =
                ahead::plan(&still, path.clone(), steps, self.cells.cell, layout, modes);
            if self.kept.contains(&key) || self.skipped.contains(&key) {
                continue;
            }
            // a pixel file of an opaque frame takes the least, and must fit; sixels' bytes are
            // known once they are encoded
            let (width, height) = crop.map_or((key.width, key.height), |c| (c.width, c.height));
            let least = if self.files.is_some() { 3 } else { 4 };
            let before = self.before(session, &neighbors, path);
            let (reserved, _) = self.kept.reserved(self.shown_bytes, &before);
            let limit = BUDGET.saturating_sub(reserved);
            if kitty && least * width as usize * height as usize > limit {
                self.skipped.push(key);
                continue;
            }
            self.ahead_files += 1;
            let file = self.files.as_ref();
            let job = ahead::Job {
                still,
                key,
                crop,
                scale,
                file: file.map(|folder| folder.join(format!("ahead-{}", self.ahead_files))),
                sixel: sixel.then_some(self.cells.cell_pixels()),
                limit,
                generation: self.generation,
            };
            let (done, number, key) = (self.made.clone(), self.ahead_files, job.key.clone());
            let thread = thread::Builder::new().name("frame-ahead".to_owned());
            let thread = thread.spawn(move || {
                let made = ahead::run(job);
                let _ = done.send(number);
                made
            });
            self.making = thread.ok().map(|thread| Making {
                job: number,
                key,
                generation: self.generation,
                thread,
                #[cfg(test)]
                hold: None,
            });
            return;
        }
    }

    // The neighbors before `path`, in the way the viewer goes, each with the key its frame will
    // have once the viewer is there, where the session still has its image.
    fn before(
        &self,
        session: &Session,
        neighbors: &[PathBuf],
        path: &Path,
    ) -> Vec<(PathBuf, Option<FrameKey>)> {
        let viewer = session.viewer();
        let layout = (
            self.cells.columns,
            self.cells.image_rows(viewer.bar_visible()),
        );
        let steps = (viewer.gamma(), viewer.contrast());
        let (cell, modes) = (self.cells.cell, (self.tmux, self.mode == Mode::Sixel));
        let earlier = neighbors.iter().take_while(|&neighbor| neighbor != path);
        earlier
            .map(|neighbor| {
                let still = session.still(neighbor);
                let plan =
                    |still| ahead::plan(&still, neighbor.clone(), steps, cell, layout, modes);
                (neighbor.clone(), still.map(|still| plan(still).0))
            })
            .collect()
    }

    // Job `job` of the frame-ahead thread is done: its frame, unless a move took it already.
    fn ahead_done(&mut self, session: &Session, job: u64) {
        let Some(making) = self.making.take_if(|making| making.job == job) else {
            return;
        };
        if let Ok(made) = making.thread.join() {
            self.send_ahead(session, made);
        }
    }

    // A frame made ahead, sent to the terminal, or kept as sixels, unless it went stale
    // meanwhile, as with another layout or a file that changed, or no longer fits; then the
    // next one.
    fn send_ahead(&mut self, session: &Session, made: ahead::Made) {
        let viewer = session.viewer();
        let neighbors = self.neighbors(viewer);
        let before = self.before(session, &neighbors, &made.key.path);
        let (reserved, spared) = self.kept.reserved(self.shown_bytes, &before);
        let fits = reserved + made.bytes <= BUDGET;
        let kept = self.kept.contains(&made.key);
        let (width, height, alpha, bytes) = (made.width, made.height, made.alpha, made.bytes);
        let (key, payload) = match ahead::verdict(made, self.generation, kept, fits) {
            Ok(sent) => sent,
            Err(skipped) => {
                // one that no longer fits, or could not be made, waits for another view
                self.skipped.extend(skipped);
                self.prefetch(session);
                return;
            }
        };
        let id = self.new_id();
        let bar = viewer.bar_visible();
        let rows = self.cells.rows;
        // under the status bar's last cell, whose background hides it
        let hidden = self.place_ahead && !self.tmux && bar && rows > self.cells.image_rows(bar);
        let keys = match hidden {
            true => format!("a=T,i={id},p=2,c=1,r=1,z=-1073741825,C=1"),
            false => format!("a=t,i={id}"),
        };
        let tmux = self.tmux;
        let command = match payload {
            ahead::Payload::File(file) => {
                kitty::kitty_file(&file, &keys, width, height, alpha, true, tmux).into_bytes()
            }
            ahead::Payload::Png(png) => kitty::kitty_image(&png, &keys, tmux),
            // the viewer's own, for the move to write
            ahead::Payload::Sixel(sixels) => {
                self.kept.keep(key, id, bytes);
                self.sixels.insert(id, sixels);
                let gone = self.kept.trim(&[&[id][..], &spared].concat());
                self.delete(&gone);
                self.prefetch(session);
                return;
            }
        };
        self.kept.keep(key, id, bytes);
        self.placed_ahead |= hidden;
        let mut out = Vec::new();
        if hidden {
            out.extend(format!("\x1b[{rows};{}H", self.cells.columns).bytes());
        }
        out.extend(command);
        if hidden {
            out.extend(b"\x1b[H");
        }
        // what the budget left room for stays
        for gone in self.kept.trim(&[&[self.shown, id][..], &spared].concat()) {
            out.extend(kitty::delete(gone, tmux).bytes());
        }
        self.write(&out);
        self.prefetch(session);
    }

    // The kitty images that `bytes`, after the input before them, say are gone, forgotten;
    // whether the one on screen is among them. In kitty mode only: in any other, such text is
    // the user's, as pasted, and a kept sixel frame's id names no image of the terminal's.
    fn images_gone(&mut self, bytes: &[u8]) -> bool {
        if self.mode != Mode::Kitty {
            return false;
        }
        let mut gone = false;
        for id in kitty::missing_images(&[&self.input.rest[..], bytes].concat()) {
            self.kept.forget(id);
            if id == self.shown {
                self.shown = 0;
                gone = true;
            }
        }
        gone
    }

    // An id no kept frame uses, nor the one on screen.
    fn new_id(&mut self) -> u32 {
        loop {
            self.last_id = next_id(self.last_id, self.tmux);
            if self.last_id != self.shown && !self.kept.holds(self.last_id) {
                return self.last_id;
            }
        }
    }

    // The frame on screen off it: a kept one loses its placement only, as deleting all of it
    // would free its pixels, and any other goes.
    fn hide(&mut self) -> String {
        self.shown_bytes = 0;
        match std::mem::take(&mut self.shown) {
            0 => String::new(),
            id if self.kept.holds(id) => {
                kitty::command(&format!("a=d,d=i,i={id},p=1,q=2"), self.tmux)
            }
            id => kitty::delete(id, self.tmux),
        }
    }

    // The frame, or with `bar_only` the status line alone, in synchronized output, so the
    // terminal shows it once it is whole and the bar never names an image that has yet to
    // come; terminals without it ignore the mode.
    fn redraw(&mut self, session: &mut Session, bar_only: bool) {
        let viewer = session.viewer();
        let bar = viewer.bar_visible();
        let (columns, rows) = (self.cells.columns, self.cells.image_rows(bar));
        let status = bar.then(|| {
            let (left, right) = viewer.status_text();
            let left = viewer.error_text().map_or(left, str::to_owned);
            text::status_line(&left, &right, columns)
        });
        let mut out = b"\x1b[?2026h".to_vec();
        // in tmux, what goes once the frame shows: tmux passes graphics on at once, but draws
        // text as synchronized output ends, so the frame before would go while its
        // placeholders still show
        let mut after = Vec::new();
        if !bar_only {
            out.extend(b"\x1b[H");
            self.frame(session, &mut out, &mut after, columns, rows);
        }
        if let Some(line) = status.filter(|_| self.cells.rows > 0) {
            let line = format!("\x1b[{};1H\x1b[0;7m{line}\x1b[0m", self.cells.rows);
            out.extend(line.bytes());
        }
        if self.placed_ahead && !bar {
            // without the bar's background over them, they would show
            self.placed_ahead = false;
            for id in self.kept.ids() {
                let hidden = kitty::command(&format!("a=d,d=i,i={id},p=2,q=2"), self.tmux);
                out.extend(hidden.bytes());
            }
        }
        // home, where a stray warning cannot scroll the screen
        out.extend(b"\x1b[H\x1b[?2026l");
        out.extend(after);
        self.write(&out);
    }

    // The frame above the bar, of `columns × rows` cells, on a clear canvas, so the terminal's
    // own background shows around the image.
    fn frame(
        &mut self,
        session: &mut Session,
        out: &mut Vec<u8>,
        after: &mut Vec<u8>,
        columns: usize,
        rows: usize,
    ) {
        let viewer = session.viewer();
        let bar = viewer.bar_visible();
        let image = !viewer.thumbnail_mode() && viewer.image().is_some();
        let (kitty, sixel) = (self.mode == Mode::Kitty, self.mode == Mode::Sixel);
        let cell_height = self.cells.cell.1;
        // the canvas the session laid the viewer out for
        let view = viewer.viewport().view_size();
        let pixels = frame::frame_pixels(viewer.viewport(), image, kitty, sixel, cell_height, view);
        if !self.cells.has_area(self.mode, bar) {
            // no row left for an image, as in a one-row terminal with the bar
            out.extend(self.hide().bytes());
            if !kitty {
                out.extend(b"\x1b[2J");
            }
            return;
        }
        if kitty {
            let bytes = self.kitty_frame(session, view, pixels, columns, rows);
            out.extend(bytes.show);
            match self.tmux {
                true => after.extend(bytes.hide),
                false => out.extend(bytes.hide),
            }
            return;
        }
        if sixel {
            self.sixel_frame(session, out, pixels);
            return;
        }
        let (width, height, scale) = pixels;
        let Some(_) = self.draw(session, width, height, scale, false) else {
            return;
        };
        if let Mode::HalfBlocks(depth) = self.mode {
            let cells = frame::half_blocks(&self.canvas, width, height, columns, rows, depth);
            out.extend(cells.bytes());
        }
    }

    // The view as a sixel image, on a clear screen, as the image leaves what it does not
    // cover, its clear pixels and the band short of the bar: one kept for the view, or one
    // drawn and encoded, then kept, as that takes longer than the terminal reading it. Of the
    // image view, the picture's own pixels go, from the cell at or before its corner.
    fn sixel_frame(
        &mut self,
        session: &mut Session,
        out: &mut Vec<u8>,
        (width, height, scale): (u32, u32, f64),
    ) {
        let key = FrameKey::of(session.viewer(), width, height);
        if let Some(key) = &key {
            self.wait_ahead(key);
        }
        let kept = key.as_ref().and_then(|key| self.kept.get(key));
        if let Some(sixels) = kept.and_then(|id| self.sixels.get(&id)) {
            out.extend(b"\x1b[2J");
            out.extend(sixels.bytes());
            return;
        }
        let Some(cut) = self.draw_sixels(session, (width, height, scale)) else {
            return;
        };
        out.extend(b"\x1b[2J");
        let cell = self.cells.cell_pixels();
        let Some(sixels) = frame::placed_sixels(&self.canvas, width, &cut, cell) else {
            return;
        };
        out.extend(sixels.bytes());
        if let Some(key) = key {
            self.keep_sixels(key, sixels);
        }
    }

    // The view drawn into the canvas, and the part of it to send as sixels: of the image view,
    // the picture's own pixels, else all of it.
    fn draw_sixels(
        &mut self,
        session: &mut Session,
        (width, height, scale): (u32, u32, f64),
    ) -> Option<frame::SixelCut> {
        let viewer = session.viewer();
        let image = !viewer.thumbnail_mode() && viewer.image().is_some();
        self.draw(session, width, height, scale, false)?;
        // a picture with alpha of its own keeps clear pixels, and the encoder its slower path
        let cell_height = self.cells.cell_pixels().1;
        Some(frame::sixel_cut(
            &self.canvas,
            (width, height),
            image,
            cell_height,
        ))
    }

    // A sixel frame kept for its view, within the budget, the least recently shown going; one
    // larger than all of it is not kept, as the terminal holds nothing that must stay.
    fn keep_sixels(&mut self, key: FrameKey, sixels: String) {
        if sixels.len() > BUDGET {
            return;
        }
        let id = self.new_id();
        self.kept.keep(key, id, sixels.len());
        self.sixels.insert(id, sixels);
        let gone = self.kept.trim(&[id]);
        self.delete(&gone);
    }

    // The sixel frame of `key`, where the frame-ahead thread is making it, waited for and kept,
    // as it is done sooner than one begun anew; one made for an older generation, as of a file
    // that changed since, is left to end on its own.
    fn wait_ahead(&mut self, key: &FrameKey) {
        let generation = self.generation;
        let current = |making: &mut Making| making.key == *key && making.generation == generation;
        let Some(making) = self.making.take_if(current) else {
            return;
        };
        #[cfg(test)]
        drop(making.hold);
        let Ok(made) = making.thread.join() else {
            return;
        };
        if let Some(ahead::Payload::Sixel(sixels)) = made.payload {
            self.keep_sixels(made.key, sixels);
        }
    }

    // The view drawn into the canvas, sized for it, and whether it came out opaque; with
    // `whole`, the image alone over all of it.
    fn draw(
        &mut self,
        session: &mut Session,
        width: u32,
        height: u32,
        scale: f64,
        whole: bool,
    ) -> Option<bool> {
        self.canvas.resize(width as usize * height as usize, 0);
        let mut canvas = Canvas::new(&mut self.canvas, width, height)?;
        let frame = Frame {
            clear: true,
            whole,
            kept: false,
        };
        Some(session.draw(&mut canvas, scale, frame))
    }

    // The view as kitty graphics, a frame the terminal keeps placed again or a new one sent,
    // in place of what the screen shows, and what goes once it shows: the frame there before
    // loses its placement, or goes entirely when it was not kept, with the frames past the
    // budget.
    fn kitty_frame(
        &mut self,
        session: &mut Session,
        view: Size,
        (width, height, scale): (u32, u32, f64),
        columns: usize,
        rows: usize,
    ) -> Shown {
        let tmux = self.tmux;
        let viewer = session.viewer();
        if nothing_yet(viewer, self.drawn) {
            let hide = self.hide().into_bytes();
            return Shown {
                show: hide,
                hide: Vec::new(),
            };
        }
        self.drawn = true;
        let key = FrameKey::of(viewer, width, height);
        // through tmux, whose placeholders fill their box, an image goes with its view
        let crop = match key {
            Some(_) if !tmux => {
                let cell = self.cells.cell;
                kitty::crop(viewer.viewport(), view, cell, scale, columns, rows)
            }
            _ => None,
        };
        let mut show = String::new();
        if !self.cleared {
            // what other programs left
            show += &kitty::delete_all(tmux);
            self.cleared = true;
        }
        if !tmux {
            // the frame's clear parts show the text under them, as a status line the bar left
            // or a smaller terminal moved; erasing the screen would delete the images on it,
            // so the rows go one by one
            for row in 1..=rows {
                show += &format!("\x1b[{row};1H\x1b[2K");
            }
            show += "\x1b[H";
        }
        let mut show = show.into_bytes();
        let kept = key.as_ref().and_then(|key| self.kept.get(key));
        let (id, bytes) = match kept {
            Some(id) => {
                // q=1 leaves an answer when the terminal dropped the image, so it goes again
                let placement = kitty::placement(id, crop.as_ref(), columns, rows, tmux);
                show.extend(kitty::cursor(crop.as_ref()).bytes());
                show.extend(kitty::command(&format!("a=p,{placement},q=1"), tmux).bytes());
                (id, self.kept.bytes(id).unwrap_or(0))
            }
            None => {
                // shortcut: a pan step in a zoomed image costs a frame, its view's; the whole
                // image kept once and placed through a source rectangle would cost a placement,
                // once source rectangles can be tried in real terminals, through herdr and
                // through tmux
                let (width, height) = crop.map_or((width, height), |c| (c.width, c.height));
                let Some(opaque) = self.draw(session, width, height, scale, crop.is_some()) else {
                    return Shown::default();
                };
                let id = self.new_id();
                let placement = kitty::placement(id, crop.as_ref(), columns, rows, tmux);
                let keys = format!("a=T,{placement}");
                let Some((sent, bytes)) = self.transmit(&keys, id, width, height, !opaque) else {
                    return Shown::default();
                };
                show.extend(kitty::cursor(crop.as_ref()).bytes());
                show.extend(sent);
                if let Some(key) = key {
                    self.kept.keep(key, id, bytes);
                }
                (id, bytes)
            }
        };
        if tmux {
            show.extend(placeholders(id, columns, rows).bytes());
        }
        let mut hide = match self.shown {
            shown if shown == id => String::new(),
            _ => self.hide(),
        };
        self.shown = id;
        self.shown_bytes = bytes;
        let trimmed = self.kept.trim(&[id]);
        hide.extend(trimmed.iter().map(|&id| kitty::delete(id, tmux)));
        Shown {
            show,
            hide: hide.into_bytes(),
        }
    }

    // The canvas sent as image `id` with `keys` before its own, and the bytes the terminal
    // stores for it: raw pixels in a file the terminal reads and deletes itself, kept as they
    // came, or PNG through the terminal, decoded to RGBA.
    fn transmit(
        &self,
        keys: &str,
        id: u32,
        width: u32,
        height: u32,
        alpha: bool,
    ) -> Option<(Vec<u8>, usize)> {
        let pixels = width as usize * height as usize;
        // shortcut: a pixel file the terminal fails to read after the probe said it could gives
        // a blank frame, since q=2 hides the error; q=1 on file
        // transmissions, and PNG from then on, would recover
        if let Some(folder) = &self.files {
            let path = folder.join(id.to_string());
            if write_pixel_file(&path, &self.canvas, alpha) {
                let sent = kitty::kitty_file(&path, keys, width, height, alpha, true, self.tmux);
                let channels = if alpha { 4 } else { 3 };
                return Some((sent.into_bytes(), channels * pixels));
            }
        }
        let png = frame::png(&self.canvas, width, height, alpha)?;
        Some((kitty::kitty_image(&png, keys, self.tmux), 4 * pixels))
    }
}

/// The frame the frame-ahead thread is making: its job's number, its key, the generation it is
/// made for, and the thread, which gives the frame once done; in tests, what holds the thread
/// until a move takes it.
struct Making {
    job: u64,
    key: FrameKey,
    generation: u64,
    thread: thread::JoinHandle<ahead::Made>,
    #[cfg(test)]
    hold: Option<std::sync::mpsc::Sender<()>>,
}

/// A kitty frame's bytes: what shows it, and what goes once it shows.
#[derive(Default)]
struct Shown {
    show: Vec<u8>,
    hide: Vec<u8>,
}

/// Whether a kitty frame would show nothing: before the first image decoded, where no frame
/// went yet. Once one went, the session draws the image before while the next file decodes,
/// with the viewer's image none meanwhile, which a redraw must show.
fn nothing_yet(viewer: &Viewer, drawn: bool) -> bool {
    !drawn && !viewer.thumbnail_mode() && viewer.image().is_none() && viewer.error_text().is_none()
}

/// The first id before any given: unlike another viewer's in the same terminal; in tmux, the
/// high byte from the pane, so viewers in two panes differ.
// shortcut: panes 255 apart share it; a registry in a tmux option would part them
fn first_id(tmux: bool) -> u32 {
    if !tmux {
        return (std::process::id() & 0xffff) << 8;
    }
    let pane = std::env::var("TMUX_PANE").ok();
    let pane = pane.and_then(|pane| pane.get(1..)?.parse::<u32>().ok());
    (pane.unwrap_or_else(std::process::id) % 255 + 1) << 24
}

/// The id after `last`. In tmux, its low byte goes in a 256-color foreground, from 16 up, as
/// tmux may turn the first 16 into other colors, and its high byte stays the viewer's own.
fn next_id(last: u32, tmux: bool) -> u32 {
    if !tmux {
        return last % 0xff_ffff + 1;
    }
    let low = last & 0xff;
    last & 0xff00_0000
        | if (16..255).contains(&(low + 1)) {
            low + 1
        } else {
            16
        }
}

/// Image `id` as the text tmux keeps and redraws with the pane, over `columns × rows` cells,
/// which the terminal around tmux shows as the image: its low byte is the foreground of the
/// 256 colors around the placeholders.
fn placeholders(id: u32, columns: usize, rows: usize) -> String {
    let cells = tmux::placeholder_cells(columns, rows, (id >> 24) as u8);
    format!("\x1b[38;5;{}m{cells}\x1b[39m", id & 0xff)
}

impl Frontend for Terminal {
    fn window(&self) -> (Size, f64) {
        (self.cells.window(self.mode), self.cells.scale())
    }

    fn request(&mut self, session: &mut Session, request: Request) {
        self.follow(session.viewer());
        match request {
            Request::Redraw { bar_only } => {
                self.redraw(session, bar_only);
                self.prefetch(session);
            }
            Request::Still(path) => {
                if !self.named.contains(&path) {
                    self.named.push(path);
                }
                self.prefetch(session);
            }
            Request::Forget(paths) => {
                let ids = self.kept.forget_files(&paths);
                self.delete(&ids);
                self.skipped.retain(|key| !paths.contains(&key.path));
                // a frame being made may be of one of them
                self.generation += 1;
            }
            Request::Beep => self.write(b"\x07"),
            // the terminal keeps its size, and the view fills it already
            _ => {}
        }
    }

    // the viewer's images out of the terminal, kept or shown: tmux's placeholders show an
    // image through a placement that deleting what is on screen leaves
    fn release(&mut self) {
        if self.mode == Mode::Kitty {
            let shown = std::mem::take(&mut self.shown);
            self.forget_all();
            self.delete(&[shown]);
            self.cleared = false;
        }
        with_saved(|saved| leave(saved));
    }

    // the alternate screen comes back empty, without the images it held
    fn reclaim(&mut self) {
        with_saved(|saved| enter(saved));
        // the pixel files' folder went with the screen
        let made = self.files.as_ref().is_some_and(|folder| {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(folder)
                .is_ok()
        });
        if !made {
            self.files = None;
            with_saved(|saved| saved.files = None);
        }
    }
}

// The keys in what the terminal sent after what it sent before; a sequence it ends inside
// waits for the next bytes, or for the idle time. Replies that a kitty image is gone, as when
// the terminal needed its room, forget it, and the frame on screen goes again.
fn input(driver: &mut Driver<Terminal>, bytes: &[u8]) {
    let terminal = driver.frontend();
    if let Some(token) = terminal.idle.take() {
        terminal.handle.remove(token);
    }
    let gone = terminal.images_gone(bytes);
    let keys = terminal.input.take(bytes);
    let waiting = terminal.input.waiting();
    for key in keys {
        pressed(driver, key);
    }
    if gone {
        driver.redraw();
    }
    if waiting {
        let terminal = driver.frontend();
        let idle = |_, _: &mut (), driver: &mut Driver<Terminal>| {
            let terminal = driver.frontend();
            terminal.idle = None;
            let key = terminal.input.idle();
            pressed(driver, key);
            TimeoutAction::Drop
        };
        terminal.idle = terminal
            .handle
            .insert_source(Timer::from_duration(IDLE), idle)
            .ok();
    }
}

fn pressed(driver: &mut Driver<Terminal>, key: Option<Keystroke>) {
    // a key without a name still ends a C-x prefix
    let Some(key) = key else {
        driver.event(|session| {
            session.end_prefix();
            Vec::new()
        });
        return;
    };
    // C-c quits, as in most terminal programs, where no binding and no C-x prefix took it
    if !driver.key(&key) && key == Keystroke::ctrl("c") {
        driver.quit();
    }
}

/// What the terminal shows besides text, whether it goes through tmux's passthrough, and the
/// input that came meanwhile, for the keys typed during the probe. In tmux, which takes a kitty
/// query as a title, tmux is asked what it passes on, and the terminal around it only through
/// it; seen from outside, as over SSH, a terminal multiplexer gets no query.
fn graphics(tty: &File, files: Option<&Path>) -> (kitty::Graphics, bool, Vec<u8>) {
    let none = kitty::Graphics {
        kitty: false,
        files: false,
        sixel: false,
    };
    let term = std::env::var("TERM").unwrap_or_default();
    let support = std::env::var_os("TMUX")
        .is_some_and(|tmux| !tmux.is_empty())
        .then(|| tmux::tmux_support(&tmux_answer()));
    let nested = support.is_none() && (term.starts_with("screen") || term.starts_with("tmux"));
    // shortcut: in tmux, a terminal that answers the query but has no Unicode placeholders
    // shows the placeholder cells as glyphs; the protocol's feature query
    // (a=q,i=31,e=T,U=1,C=1) gets no answer with its keys from kitty before 0.50, which has
    // placeholders, so asking waits for trials in real terminals
    let tmux = support.is_some_and(|support| support.passthrough);
    if !support.map_or(!nested, |support| support.passthrough) {
        let sixel = support.is_some_and(|support| support.sixel);
        return (kitty::Graphics { sixel, ..none }, false, Vec::new());
    }
    let (found, reply) = probe(tty, tmux, files);
    let mut found = found.unwrap_or(none);
    // tmux draws sixels itself, so what counts is what it can
    if let Some(support) = support {
        found.sixel = support.sixel;
    }
    (found, tmux, reply)
}

// tmux's answer to TMUX_FORMAT for the viewer's pane
fn tmux_answer() -> String {
    let mut command = Command::new("tmux");
    command.args(["display", "-p"]);
    if let Some(pane) = std::env::var_os("TMUX_PANE") {
        command.arg("-t").arg(pane);
    }
    command
        .arg(tmux::TMUX_FORMAT)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// A terminal with kitty graphics answers a query for a one-pixel image with OK, before the
/// device attributes any terminal reports, which name sixels where it has them; one that never
/// answers is given up after a second. A second query, for the pixel in a file in `files`,
/// tells whether the terminal reads the viewer's files.
fn probe(tty: &File, tmux: bool, files: Option<&Path>) -> (Option<kitty::Graphics>, Vec<u8>) {
    let Ok(raw) = tcgetattr(tty) else {
        return (None, Vec::new());
    };
    // reads that wait 0.1 s at most, so the deadline is kept
    let mut waiting = raw.clone();
    waiting.special_codes[SpecialCodeIndex::VMIN] = 0;
    waiting.special_codes[SpecialCodeIndex::VTIME] = 1;
    let _ = tcsetattr(tty, OptionalActions::Now, &waiting);
    let mut query = kitty::command("i=31,s=1,v=1,a=q,t=d,f=24;AAAA", tmux);
    if let Some(probe) = files.map(|folder| folder.join("probe"))
        && std::fs::write(&probe, [0, 0, 0]).is_ok()
    {
        let path = BASE64_STANDARD.encode(probe.as_os_str().as_encoded_bytes());
        query += &kitty::command(&format!("i=32,s=1,v=1,a=q,t=t,f=24;{path}"), tmux);
    }
    query += &match tmux {
        true => tmux::passthrough("\x1b[c"),
        false => "\x1b[c".to_owned(),
    };
    let _ = (&*tty).write_all(query.as_bytes());
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut reply = Vec::new();
    let answer = loop {
        if let Some(answer) = kitty::graphics(&reply) {
            break Some(answer);
        }
        if Instant::now() >= deadline {
            break None;
        }
        let mut bytes = [0; 256];
        match (&*tty).read(&mut bytes) {
            Ok(n) => reply.extend_from_slice(&bytes[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break None,
        }
    };
    let _ = tcsetattr(tty, OptionalActions::Now, &raw);
    (answer, reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn winsize(columns: u16, rows: u16, width: u16, height: u16) -> Winsize {
        Winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: width,
            ws_ypixel: height,
        }
    }

    const HALF: Mode = Mode::HalfBlocks(ColorDepth::True);

    fn key(name: &str) -> Option<Keystroke> {
        Some(Keystroke::new(name))
    }

    #[test]
    fn the_window_is_in_sixteenths_of_a_row_and_the_scale_in_the_terminals_pixels() {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        assert_eq!(cells.cell, (10.0, 20.0));
        assert!(cells.pixels);
        assert_eq!((cells.image_rows(true), cells.image_rows(false)), (29, 30));
        let size = |width, height| Size { width, height };
        // every row, the status line's among them, which the session takes off while it shows
        assert_eq!(cells.window(Mode::Kitty), size(800.0, 480.0));
        assert_eq!(cells.window(HALF), size(800.0, 480.0));
        assert_eq!(cells.scale(), 1.25);
        // a window a few pixels past its last cell, and a terminal that gives no pixels, which
        // keeps the usual 1:2 cell
        let over = Cells::default().measured(winsize(100, 30, 1007, 611));
        assert_eq!(over.cell, (10.0, 20.0));
        let unknown = Cells::default().measured(winsize(100, 30, 0, 0));
        assert_eq!((unknown.cell, unknown.pixels), ((8.0, 16.0), false));
        assert_eq!(unknown.window(HALF), size(800.0, 480.0));
        assert_eq!(unknown.scale(), 1.0);
        // a size of nothing keeps the one before
        assert_eq!(cells.measured(winsize(0, 0, 0, 0)), cells);
        let smaller = cells.measured(winsize(80, 20, 0, 0));
        assert_eq!(smaller.window(HALF), size(640.0, 320.0));
    }

    #[test]
    fn frames_take_the_terminals_pixels_and_sixels_whole_bands_short_of_the_last_row() {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        // the canvas above the bar, as the session lays it out
        let canvas = Size {
            width: 800.0,
            height: 464.0,
        };
        let viewport = nsxiv_ng_core::viewport::Viewport::default();
        assert_eq!(
            frame::frame_pixels(&viewport, false, true, false, 20.0, canvas),
            (1000, 580, 1.25)
        );
        // 29 rows of 20 pixels are 580, 96 bands and 4 rows; the band area is the window
        // whether the bar shows or not, as the last row stays out
        assert_eq!(cells.sixel_pixels(), (1000, 576));
        let sixel_window = cells.window(Mode::Sixel);
        assert_eq!(
            frame::frame_pixels(&viewport, true, false, true, 20.0, sixel_window),
            (1000, 576, 1.25)
        );
        // half blocks go at 16 a row, which they average down
        assert_eq!(
            frame::frame_pixels(&viewport, true, false, false, 20.0, canvas),
            (800, 464, 1.0)
        );
    }

    #[test]
    fn a_frame_needs_a_row_above_the_bar_and_sixels_a_whole_band() {
        let one = Cells::default().measured(winsize(100, 1, 1000, 20));
        assert!(!one.has_area(HALF, true));
        assert!(one.has_area(HALF, false));
        assert!(
            !one.has_area(Mode::Sixel, false),
            "the only row is the last"
        );
        let short = Cells::default().measured(winsize(100, 2, 1000, 10));
        assert_eq!(
            short.sixel_pixels(),
            (1000, 0),
            "a row of 5 pixels is no band"
        );
        assert!(!short.has_area(Mode::Sixel, true));
        assert!(short.has_area(HALF, true));
    }

    #[test]
    fn kitty_then_sixels_with_pixels_then_half_blocks() {
        let depth = ColorDepth::Colors256;
        assert_eq!(mode(true, true, true, depth), Mode::Kitty);
        assert_eq!(mode(true, false, false, depth), Mode::Kitty);
        assert_eq!(mode(false, true, true, depth), Mode::Sixel);
        assert_eq!(
            mode(false, true, false, depth),
            Mode::HalfBlocks(depth),
            "sixels go at the terminal's pixels, which it must give"
        );
        assert_eq!(mode(false, false, true, depth), Mode::HalfBlocks(depth));
    }

    #[test]
    fn a_lone_escape_left_idle_is_the_escape_key() {
        let mut input = Input::default();
        assert!(input.take(b"\x1b").is_empty());
        assert!(input.waiting());
        assert_eq!(input.idle(), key("Escape"));
        assert!(!input.waiting());
        assert_eq!(input.take(b"j\x1b_"), [key("j")]);
        assert_eq!(input.idle(), key("_"), "Alt and _");
        input.take(b"\x1b[1;");
        assert_eq!(input.idle(), None, "a sequence cut short has no name");
        assert_eq!(input.idle(), None);
    }

    #[test]
    fn what_came_during_the_probe_comes_before_a_later_read_whichever_fires_first() {
        // the probe's answers, a j typed meanwhile, and a Left cut off after ESC [, whose D
        // must not count alone, as D would drop the image; the device attributes have no name
        let probed = b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22cj\x1b[";
        let mut read_first = Input {
            rest: probed.to_vec(),
        };
        assert_eq!(read_first.take(b"D"), [None, key("j"), key("Left")]);
        assert!(
            read_first.take(b"").is_empty(),
            "the timer then finds nothing"
        );
        let mut timer_first = Input {
            rest: probed.to_vec(),
        };
        assert_eq!(timer_first.take(b""), [None, key("j")]);
        assert!(timer_first.waiting(), "the Left waits for its D");
        assert_eq!(timer_first.take(b"D"), [key("Left")]);
    }

    // a viewer of one image of `size` units, shown in a canvas of 800x464, the 29 rows above
    // the bar of a terminal of 100 columns of 10x20 pixels
    fn viewer_of(size: Size) -> nsxiv_ng_core::viewer::Viewer {
        use nsxiv_ng_core::file_list::FileEntry;
        use nsxiv_ng_core::viewer::{Effect, ImageInfo, Viewer};
        let file = FileEntry::new(PathBuf::from("/a.png"));
        let mut viewer = Viewer::new(Options::default(), vec![file]);
        viewer.resized(Size {
            width: 800.0,
            height: 464.0,
        });
        let request = viewer.start().into_iter().find_map(|effect| match effect {
            Effect::Load { request, .. } => Some(request),
            _ => None,
        });
        let image = ImageInfo {
            size,
            delays: vec![0.1],
            complete: true,
        };
        viewer.loaded(request.unwrap(), Ok(image));
        viewer
    }

    #[test]
    fn an_image_shown_whole_goes_as_a_crop_and_a_zoomed_one_as_its_view_kept_per_pan() {
        let canvas = Size {
            width: 800.0,
            height: 464.0,
        };
        let crop = |viewer: &nsxiv_ng_core::viewer::Viewer, scale| {
            kitty::crop(viewer.viewport(), canvas, (10.0, 20.0), scale, 100, 29)
        };
        let frame_of = |viewer: &nsxiv_ng_core::viewer::Viewer| {
            frame::frame_pixels(viewer.viewport(), true, true, false, 20.0, canvas)
        };
        let mut viewer = viewer_of(Size {
            width: 700.0,
            height: 232.0,
        });
        // at 100%, the image's own pixels, alone in its box
        let (width, height, scale) = frame_of(&viewer);
        assert_eq!((width, height, scale), (800, 464, 1.0));
        let whole = crop(&viewer, scale).expect("a crop");
        assert_eq!(
            (whole.width, whole.height, whole.columns, whole.rows),
            (700, 232, 88, 15)
        );
        let first = FrameKey::of(&viewer, width, height);
        assert!(first.is_some());
        // zoomed in, the image runs past the sides: no crop, and the view at the image's scale
        viewer.key(&Keystroke::new("+"));
        let (width, height, scale) = frame_of(&viewer);
        assert_eq!((width, height), (533, 309));
        assert_eq!(crop(&viewer, scale), None);
        // from the left edge, a pan step is another view, and a step back the same one, which
        // the terminal keeps
        viewer.key(&Keystroke::new("H"));
        let zoomed = FrameKey::of(&viewer, width, height).unwrap();
        viewer.key(&Keystroke::new("l"));
        let panned = FrameKey::of(&viewer, width, height).unwrap();
        assert_ne!(panned, zoomed);
        viewer.key(&Keystroke::new("h"));
        assert_eq!(FrameKey::of(&viewer, width, height), Some(zoomed.clone()));
        // a mark changes no pixel of the view
        viewer.key(&Keystroke::new("m"));
        assert_eq!(FrameKey::of(&viewer, width, height), Some(zoomed.clone()));
        assert_ne!(first, Some(zoomed));
    }

    #[test]
    fn only_a_frame_before_the_first_image_shows_nothing() {
        use nsxiv_ng_core::file_list::FileEntry;
        use nsxiv_ng_core::viewer::{Effect, ImageInfo};
        let files = ["/a.png", "/b.png"].map(|path| FileEntry::new(PathBuf::from(path)));
        let mut viewer = Viewer::new(Options::default(), files.to_vec());
        let load = |effects: Vec<Effect>| {
            effects.into_iter().find_map(|effect| match effect {
                Effect::Load { request, .. } => Some(request),
                _ => None,
            })
        };
        let first = load(viewer.start()).unwrap();
        assert!(
            nothing_yet(&viewer, false),
            "before the first image decoded"
        );
        let image = ImageInfo {
            size: Size {
                width: 700.0,
                height: 232.0,
            },
            delays: vec![0.1],
            complete: true,
        };
        viewer.loaded(first, Ok(image));
        assert!(!nothing_yet(&viewer, true));
        // while the next file decodes, the viewer has no image, and the session draws the one
        // before, which a redraw, as for a resize, keeps on the screen
        load(viewer.key(&Keystroke::new("j")).unwrap()).unwrap();
        assert!(viewer.image().is_none());
        assert!(!nothing_yet(&viewer, true));
        assert_eq!(FrameKey::of(&viewer, 800, 464), None, "drawn, not kept");
    }

    #[test]
    fn ids_differ_from_another_viewers_and_in_tmux_keep_their_pane_and_a_color_past_16() {
        assert_eq!(
            next_id(0x00ff_ffff, false),
            1,
            "past the 24 bits the protocol takes"
        );
        assert_eq!(next_id(41, false), 42);
        // in tmux, the low byte is a 256-color foreground from 16 to 254, the high byte the pane's
        assert_eq!(next_id(3 << 24, true), 3 << 24 | 16);
        assert_eq!(next_id(3 << 24 | 16, true), 3 << 24 | 17);
        assert_eq!(next_id(3 << 24 | 254, true), 3 << 24 | 16);
        let mut id = 7 << 24;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..239 {
            id = next_id(id, true);
            assert_eq!(id >> 24, 7);
            assert!(seen.insert(id));
        }
        assert_eq!(next_id(id, true), 7 << 24 | 16, "239 ids, then again");
    }

    #[test]
    fn placeholders_take_the_ids_low_byte_as_their_color_and_its_high_byte_in_their_cells() {
        let text = placeholders(3 << 24 | 200, 2, 1);
        assert_eq!(
            text,
            "\x1b[38;5;200m\x1b[1;1H\u{10EEEE}\u{305}\u{305}\u{310}\u{10EEEE}\x1b[39m"
        );
        assert!(placeholders(1 << 24 | 16, 0, 5).ends_with("\x1b[38;5;16m\x1b[39m"));
    }

    #[test]
    fn a_workers_panic_waits_while_the_viewer_has_the_screen() {
        // not the thread that took the screen, which puts the terminal back instead
        assert!(!hold_panic(
            "panicked at here:\nnot held, the screen is the user's"
        ));
        panics().0 = true;
        let held = thread::Builder::new()
            .name("worker".to_owned())
            .spawn(|| hold_panic("panicked at src/a.rs:1:2:\na\x1b]0;title\x07b.png"))
            .unwrap()
            .join()
            .unwrap();
        panics().0 = false;
        assert!(held);
        let panics = std::mem::take(&mut panics().1);
        assert_eq!(
            panics,
            ["thread 'worker' panicked at src/a.rs:1:2:\na?]0;title?b.png"],
            "its message cleaned, for stderr once the screen is back"
        );
    }

    // A session of the fixtures `names` laid out in `cells` for `mode`, asking for the stills of
    // the neighbors with `stills`; the requests of its start, and its wakes.
    fn fixture_session(
        names: &[&str],
        mode: Mode,
        cells: &Cells,
        stills: bool,
    ) -> (Session, Vec<Request>, mpsc::Receiver<()>) {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures");
        let files = names.iter().map(|name| FileEntry::new(fixtures.join(name)));
        let (sender, wakes) = mpsc::channel();
        let wake = move || {
            let _ = sender.send(());
        };
        let bar = match mode {
            Mode::Sixel => Bar::Shown(0.0),
            _ => Bar::Shown(16.0),
        };
        let (config, program) = (Config::default(), PathBuf::from("nsxiv-ng"));
        let files = files.collect();
        let mut session = Session::new(
            Options::default(),
            files,
            &config,
            program,
            vec![],
            bar,
            wake,
        );
        if stills {
            session.want_stills();
        }
        let mut requests = session.resize(cells.window(mode), cells.scale());
        requests.extend(session.start());
        (session, requests, wakes)
    }

    // A terminal in sixel mode with `cells`, writing to `tty`, and what its frames made ahead
    // come back through.
    fn sixel_terminal(
        tty: File,
        handle: &Handle<Terminal>,
        cells: Cells,
    ) -> (Terminal, calloop::channel::Channel<u64>) {
        let (made, frames) = calloop::channel::channel();
        let terminal = Terminal {
            tty,
            mode: Mode::Sixel,
            tmux: false,
            files: None,
            cells,
            canvas: Vec::new(),
            kept: Kept::default(),
            sixels: HashMap::new(),
            shown: 0,
            last_id: first_id(false),
            shown_bytes: 0,
            drawn: false,
            cleared: false,
            named: Vec::new(),
            skipped: Vec::new(),
            making: None,
            made,
            generation: 0,
            ahead_files: 0,
            current: 0,
            forward: true,
            place_ahead: false,
            placed_ahead: false,
            input: Input::default(),
            idle: None,
            handle: handle.clone(),
        };
        (terminal, frames)
    }

    // A folder of the test's own for the terminal's output, and the output's path.
    fn tty_file(name: &str) -> (PathBuf, PathBuf) {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("nsxiv-ng-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let tty = folder.join("tty");
        (folder, tty)
    }

    // A loop of the terminal's, for its handle.
    fn event_loop() -> calloop::EventLoop<'static, Driver<Terminal>> {
        calloop::EventLoop::try_new().unwrap()
    }

    // `requests` carried to the terminal, then those of the answers, until the viewer shows its
    // current file decoded.
    fn settle(
        terminal: &mut Terminal,
        session: &mut Session,
        wakes: &mpsc::Receiver<()>,
        mut requests: Vec<Request>,
    ) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            for request in requests {
                terminal.request(session, request);
            }
            let viewer = session.viewer();
            if viewer.image().is_some() && viewer.awaited().is_none() {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            wakes.recv_timeout(left).expect("a wake");
            requests = session.answers();
        }
    }

    fn holds(bytes: &[u8], part: &[u8]) -> bool {
        bytes.windows(part.len()).any(|window| window == part)
    }

    #[test]
    fn k_after_j_writes_the_kept_sixel_frame_and_no_kitty_command() {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        let names = ["photo.jpg", "gradient.png"];
        let (mut session, requests, wakes) = fixture_session(&names, Mode::Sixel, &cells, false);
        let (folder, tty) = tty_file("sixels-kept");
        let file = File::create(&tty).unwrap();
        let (mut terminal, _) = sixel_terminal(file, &event_loop().handle(), cells);
        settle(&mut terminal, &mut session, &wakes, requests);
        let first = terminal.kept.ids();
        assert_eq!(first.len(), 1, "the photo's frame is kept");
        let photo = terminal.sixels[&first[0]].clone();
        let j = session.key(&Keystroke::new("j")).unwrap();
        settle(&mut terminal, &mut session, &wakes, j);
        let blue = terminal.kept.ids()[1];
        let before = std::fs::read(&tty).unwrap().len();
        let k = session.key(&Keystroke::new("k")).unwrap();
        settle(&mut terminal, &mut session, &wakes, k);
        assert_eq!(
            terminal.kept.ids(),
            [blue, first[0]],
            "k shows the photo's kept frame, not one encoded again"
        );
        let written = std::fs::read(&tty).unwrap();
        assert!(holds(&written[before..], photo.as_bytes()));
        assert!(!holds(&written, b"\x1b_G"), "a kitty command went");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_sixel_frame_made_ahead_is_kept_unwritten_and_j_writes_it() {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        let names = ["photo.jpg", "gradient.png"];
        let (mut session, requests, wakes) = fixture_session(&names, Mode::Sixel, &cells, true);
        let (folder, tty) = tty_file("sixels-ahead");
        let file = File::create(&tty).unwrap();
        let (mut terminal, frames) = sixel_terminal(file, &event_loop().handle(), cells);
        settle(&mut terminal, &mut session, &wakes, requests);
        // the session names the next file once decoded ahead, which may come after the photo
        while terminal.making.is_none() {
            let left = Duration::from_secs(60);
            wakes.recv_timeout(left).expect("a wake");
            for request in session.answers() {
                terminal.request(&mut session, request);
            }
        }
        let job = frames.recv().expect("the frame made ahead");
        let written = std::fs::read(&tty).unwrap().len();
        terminal.ahead_done(&session, job);
        assert_eq!(
            std::fs::read(&tty).unwrap().len(),
            written,
            "kept, not written"
        );
        let ids = terminal.kept.ids();
        assert_eq!(ids.len(), 2, "the photo's frame and the one made ahead");
        let ahead = terminal.sixels[&ids[1]].clone();
        let j = session.key(&Keystroke::new("j")).unwrap();
        settle(&mut terminal, &mut session, &wakes, j);
        assert_eq!(terminal.kept.ids(), ids, "j encodes no frame of its own");
        let output = std::fs::read(&tty).unwrap();
        assert!(holds(&output[written..], ahead.as_bytes()));
        assert!(!holds(&output, b"\x1b_G"), "a kitty command went");
        // made again on the loop's thread, it is the same cut, in the same place
        assert!(
            ahead.starts_with("\x1b["),
            "the frame moves the cursor to its cell"
        );
        terminal.forget_all();
        terminal.redraw(&mut session, false);
        let again = std::fs::read(&tty).unwrap();
        assert!(holds(&again[output.len()..], ahead.as_bytes()));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    // A job of the next file's sixel frame, as the frame-ahead thread gets it, of `generation`
    // and with `limit` bytes; the job that thread was given already, once done, out of the way.
    fn next_job(
        terminal: &mut Terminal,
        session: &Session,
        generation: u64,
        limit: usize,
    ) -> ahead::Job {
        if let Some(making) = terminal.making.take() {
            let _ = making.thread.join();
        }
        let next = session.viewer().files()[1].path.clone();
        let still = session.still(&next).expect("the next file's still");
        let viewer = session.viewer();
        let cells = terminal.cells;
        let layout = (cells.columns, cells.image_rows(viewer.bar_visible()));
        let steps = (viewer.gamma(), viewer.contrast());
        let modes = (false, true);
        let (key, crop, scale) = ahead::plan(&still, next, steps, cells.cell, layout, modes);
        ahead::Job {
            still,
            key,
            crop,
            scale,
            file: None,
            sixel: Some(cells.cell_pixels()),
            limit,
            generation,
        }
    }

    // The photo shown in a sixel terminal of `name`'s own, and the next file named, so its
    // frame is being made ahead.
    fn sixels_ahead(name: &str) -> (Terminal, Session, mpsc::Receiver<()>, PathBuf, PathBuf) {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        let names = ["photo.jpg", "gradient.png"];
        let (mut session, requests, wakes) = fixture_session(&names, Mode::Sixel, &cells, true);
        let (folder, tty) = tty_file(name);
        let file = File::create(&tty).unwrap();
        let (mut terminal, _) = sixel_terminal(file, &event_loop().handle(), cells);
        settle(&mut terminal, &mut session, &wakes, requests);
        while terminal.making.is_none() {
            wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
            for request in session.answers() {
                terminal.request(&mut session, request);
            }
        }
        (terminal, session, wakes, folder, tty)
    }

    #[test]
    fn a_sixel_frame_made_ahead_comes_back_past_the_limit_its_job_had() {
        let (mut terminal, session, _wakes, folder, _) = sixels_ahead("sixels-limit");
        let generation = terminal.generation;
        let job = next_job(&mut terminal, &session, generation, 0);
        let made = ahead::run(job);
        assert!(made.bytes > 0);
        assert!(
            matches!(made.payload, Some(ahead::Payload::Sixel(ref s)) if s.len() == made.bytes),
            "the sixels encoded come back"
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_kitty_reply_in_sixel_mode_forgets_no_kept_frame() {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        let (folder, tty) = tty_file("sixels-reply");
        let file = File::create(&tty).unwrap();
        let (mut terminal, _) = sixel_terminal(file, &event_loop().handle(), cells);
        let view = nsxiv_ng_core::viewport::Viewport::default();
        let key = FrameKey::ahead(PathBuf::from("/a.png"), &view, (0, 0), 1000, 576);
        terminal.keep_sixels(key, "?".repeat(20));
        let id = terminal.kept.ids()[0];
        // as pasted
        let reply = format!("\x1b_Gi={id},p=1;ENOENT:No such image\x1b\\");
        assert!(!terminal.images_gone(reply.as_bytes()));
        assert_eq!(terminal.kept.ids(), [id]);
        assert!(terminal.sixels.contains_key(&id));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn an_opaque_pictures_sixels_hold_all_of_it_and_no_clear_pixel_zoomed_or_panned() {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        let names = ["photo.jpg", "gradient.png"];
        let (mut session, requests, wakes) = fixture_session(&names, Mode::Sixel, &cells, false);
        let (folder, tty) = tty_file("sixels-cut");
        let file = File::create(&tty).unwrap();
        let (mut terminal, _) = sixel_terminal(file, &event_loop().handle(), cells);
        settle(&mut terminal, &mut session, &wakes, requests);
        let (cell_width, cell_height) = cells.cell_pixels();
        // fitted, zoomed out, turned and zoomed in so it runs past the top and bottom with
        // margins at the sides, then panned down and up by uneven steps; then the next
        // picture, at its size, with margins on every side
        let steps: [&[&str]; 7] = [
            &[],
            &["-"],
            &["=", ">", "+"],
            &["d"],
            &["u"],
            &["u"],
            &["j"],
        ];
        for (step, keys) in steps.into_iter().enumerate() {
            for key in keys {
                let requests = session.key(&Keystroke::new(key)).unwrap();
                settle(&mut terminal, &mut session, &wakes, requests);
            }
            let viewer = session.viewer();
            let view = viewer.viewport();
            let pixels = frame::frame_pixels(view, true, false, true, 20.0, view.view_size());
            let cut = terminal.draw_sixels(&mut session, pixels).unwrap();
            assert!(cut.width < pixels.0, "step {step}: no margin to cut");
            let cut_frame = frame::cut_pixels(&terminal.canvas, pixels.0, &cut).unwrap();
            let clear = cut_frame.iter().filter(|&&p| p >> 24 != 255).count();
            assert_eq!(clear, 0, "step {step}: {cut:?}");
            // every pixel drawn of the picture: the drawn pixels' bounds, all but the rows a
            // last band must leave where the picture runs past the frame's bottom
            let (width, height) = (pixels.0, pixels.1);
            let at = |i: usize| ((i % width as usize) as u32, (i / width as usize) as u32);
            let opaque = terminal
                .canvas
                .iter()
                .enumerate()
                .filter(|(_, p)| **p >> 24 != 0);
            let (mut x0, mut y0, mut x1, mut y1) = (width, height, 0, 0);
            for (px, py) in opaque.map(|(i, _)| at(i)) {
                (x0, y0, x1, y1) = (x0.min(px), y0.min(py), x1.max(px + 1), y1.max(py + 1));
            }
            assert_eq!(
                (cut.x, cut.y, cut.x + cut.width),
                (x0, y0, x1),
                "step {step}"
            );
            let written_top = cut.y / cell_height * cell_height;
            let banded = written_top + (y1 - y0).div_ceil(6) * 6 <= height;
            match banded {
                true => assert_eq!(cut.y + cut.height, y1, "step {step}"),
                false => assert!(
                    y1 - (cut.y + cut.height) <= 5 && y1 == height,
                    "step {step}"
                ),
            }
            // written from the cell at or before the picture's corner, less than a cell off
            let cell = (cell_width, cell_height);
            let sixels = frame::placed_sixels(&terminal.canvas, width, &cut, cell).unwrap();
            let moved = sixels
                .strip_prefix("\x1b[")
                .and_then(|s| s.split_once('H'))
                .unwrap()
                .0;
            let (row, column) = moved.split_once(';').unwrap();
            let (row, column): (u32, u32) = (row.parse().unwrap(), column.parse().unwrap());
            let (written_x, written_y) = ((column - 1) * cell_width, (row - 1) * cell_height);
            assert!(
                written_x <= cut.x && cut.x < written_x + cell_width,
                "step {step}"
            );
            assert!(
                written_y <= cut.y && cut.y < written_y + cell_height,
                "step {step}"
            );
        }
        std::fs::remove_dir_all(&folder).unwrap();
    }

    // A PNG of `width × height` opaque gray pixels but those at `clear`, in `folder`.
    fn png_with_clear(folder: &Path, name: &str, size: (u32, u32), clear: &[(u32, u32)]) -> String {
        let (width, height) = size;
        let mut rgba = [200, 200, 200, 255].repeat(width as usize * height as usize);
        for &(x, y) in clear {
            rgba[(y * width + x) as usize * 4 + 3] = 0;
        }
        let path = folder.join(name);
        let mut encoder = ::png::Encoder::new(File::create(&path).unwrap(), width, height);
        encoder.set_color(::png::ColorType::Rgba);
        encoder.set_depth(::png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
        writer.finish().unwrap();
        path.to_str().unwrap().to_owned()
    }

    // The bounds of the frame's pixels a sixel image draws, those with alpha of 128 or more:
    // left, top, right and bottom.
    fn drawn_bounds(frame: &[u32], width: u32) -> (u32, u32, u32, u32) {
        let (mut bounds, width) = ((u32::MAX, u32::MAX, 0, 0), width as usize);
        for (i, _) in frame.iter().enumerate().filter(|(_, p)| **p >> 24 >= 128) {
            let (x, y) = ((i % width) as u32, (i / width) as u32);
            bounds = (
                bounds.0.min(x),
                bounds.1.min(y),
                bounds.2.max(x + 1),
                bounds.3.max(y + 1),
            );
        }
        bounds
    }

    // The sixels of the view now hold every pixel of the frame a sixel image draws.
    fn sends_every_drawn_pixel(terminal: &mut Terminal, session: &mut Session, what: &str) {
        let view = session.viewer().viewport();
        let cell_height = terminal.cells.cell.1;
        let pixels = frame::frame_pixels(view, true, false, true, cell_height, view.view_size());
        let cut = terminal.draw_sixels(session, pixels).unwrap();
        let edges = (cut.x, cut.y, cut.x + cut.width, cut.y + cut.height);
        assert_eq!(edges, drawn_bounds(&terminal.canvas, pixels.0), "{what}");
    }

    // A picture of 100x60 with one clear pixel at `clear` goes with all the rest of it.
    fn sends_all_but_a_clear_pixel(name: &str, clear: (u32, u32)) {
        let (folder, tty) = tty_file(name);
        let picture = png_with_clear(&folder, "picture.png", (100, 60), &[clear]);
        // cells of 8x16: the frame at a pixel a unit, so the picture at its own pixels
        let cells = Cells::default().measured(winsize(100, 30, 800, 480));
        let (mut session, requests, wakes) =
            fixture_session(&[picture.as_str()], Mode::Sixel, &cells, false);
        let file = File::create(&tty).unwrap();
        let (mut terminal, _) = sixel_terminal(file, &event_loop().handle(), cells);
        settle(&mut terminal, &mut session, &wakes, requests);
        sends_every_drawn_pixel(&mut terminal, &mut session, name);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_clear_pixel_in_a_pictures_edge_column_leaves_the_rest_of_it_in_the_sixels() {
        sends_all_but_a_clear_pixel("sixels-edge-column", (0, 30));
    }

    #[test]
    fn a_clear_pixel_in_a_pictures_edge_row_leaves_the_rest_of_it_in_the_sixels() {
        sends_all_but_a_clear_pixel("sixels-edge-row", (50, 0));
    }

    #[test]
    fn a_picture_one_pixel_high_goes_whole_in_the_sixels() {
        let (folder, tty) = tty_file("sixels-line");
        let line = png_with_clear(&folder, "line.png", (100, 1), &[]);
        // cells of 10x20: drawn at 1.25 pixels a unit, over one row
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        let (mut session, requests, wakes) =
            fixture_session(&[line.as_str()], Mode::Sixel, &cells, false);
        let file = File::create(&tty).unwrap();
        let (mut terminal, _) = sixel_terminal(file, &event_loop().handle(), cells);
        settle(&mut terminal, &mut session, &wakes, requests);
        sends_every_drawn_pixel(&mut terminal, &mut session, "one row");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_sixel_frame_larger_than_the_whole_budget_is_not_kept() {
        let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
        let (folder, tty) = tty_file("sixels-budget");
        let file = File::create(&tty).unwrap();
        let (mut terminal, _) = sixel_terminal(file, &event_loop().handle(), cells);
        let key = |name: &str| {
            let view = nsxiv_ng_core::viewport::Viewport::default();
            FrameKey::ahead(PathBuf::from(name), &view, (0, 0), 1000, 576)
        };
        terminal.keep_sixels(key("/a.png"), "?".repeat(BUDGET - 10));
        terminal.keep_sixels(key("/b.png"), "?".repeat(BUDGET + 1));
        assert_eq!(
            terminal.kept.ids().len(),
            1,
            "the frame past the budget is not kept"
        );
        assert_eq!(terminal.sixels.len(), 1);
        assert!(
            terminal.kept.contains(&key("/a.png")),
            "nor does it push out a kept one"
        );
        terminal.keep_sixels(key("/b.png"), "?".repeat(20));
        assert_eq!(
            terminal.kept.ids().len(),
            1,
            "one that fits pushes the other out"
        );
        assert_eq!(terminal.sixels.len(), 1);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    // `job` made on a thread that waits until the move that takes it lets it go, as the
    // frame-ahead thread's job `number`, so the move comes while it is unfinished.
    fn hold(terminal: &mut Terminal, job: ahead::Job, number: u64) {
        let (hold, held) = mpsc::channel::<()>();
        let (key, generation) = (job.key.clone(), job.generation);
        let thread = thread::spawn(move || {
            let _ = held.recv();
            ahead::run(job)
        });
        terminal.making = Some(Making {
            job: number,
            key,
            generation,
            thread,
            hold: Some(hold),
        });
    }

    #[test]
    fn j_waits_for_the_sixel_frame_being_made_of_its_view_and_draws_none() {
        let (mut terminal, mut session, wakes, folder, tty) = sixels_ahead("sixels-wait");
        let generation = terminal.generation;
        let job = next_job(&mut terminal, &session, generation, BUDGET);
        hold(&mut terminal, job, 1);
        // the photo's pixels, which a frame the loop drew for the move would replace
        let drawn = terminal.canvas.clone();
        let written = std::fs::read(&tty).unwrap().len();
        let j = session.key(&Keystroke::new("j")).unwrap();
        settle(&mut terminal, &mut session, &wakes, j);
        assert!(terminal.making.is_none(), "j took the job being made");
        let ids = terminal.kept.ids();
        assert_eq!(ids.len(), 2, "the photo's frame and the one made ahead");
        assert!(terminal.canvas == drawn, "j drew a frame of its own");
        let output = std::fs::read(&tty).unwrap();
        let ahead = terminal.sixels[&ids[1]].as_bytes();
        assert!(holds(&output[written..], ahead));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn j_leaves_a_stale_job_of_its_view_to_end_on_its_own() {
        let (mut terminal, mut session, wakes, folder, _) = sixels_ahead("sixels-stale");
        let generation = terminal.generation;
        let job = next_job(&mut terminal, &session, generation, BUDGET);
        let next = job.key.path.clone();
        hold(&mut terminal, job, 1);
        // the file changed under the frame being made
        terminal.request(&mut session, Request::Forget(vec![next]));
        let drawn = terminal.canvas.clone();
        let j = session.key(&Keystroke::new("j")).unwrap();
        settle(&mut terminal, &mut session, &wakes, j);
        let making = terminal.making.take().expect("the stale job is left");
        assert!(!making.thread.is_finished(), "j did not wait for it");
        assert!(terminal.canvas != drawn, "j drew its frame anew");
        drop(making.hold);
        let _ = making.thread.join();
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_job_done_that_came_late_takes_nothing_from_the_one_being_made() {
        let (mut terminal, session, _wakes, folder, _) = sixels_ahead("sixels-late");
        let generation = terminal.generation;
        let job = next_job(&mut terminal, &session, generation, BUDGET);
        hold(&mut terminal, job, 2);
        // job 1's word that it is done, after job 2 began; joining job 2 would wait for good
        terminal.ahead_done(&session, 1);
        let making = terminal.making.take().expect("job 2 is still being made");
        assert_eq!(making.job, 2);
        assert!(!making.thread.is_finished());
        drop(making.hold);
        assert!(making.thread.join().unwrap().payload.is_some());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn the_key_of_a_frame_made_ahead_is_the_one_the_viewer_has_once_there() {
        for mode in [Mode::Kitty, Mode::Sixel] {
            let cells = Cells::default().measured(winsize(100, 30, 1000, 600));
            let names = ["photo.jpg", "gradient.png"];
            let (mut session, requests, wakes) = fixture_session(&names, mode, &cells, true);
            let next = session.viewer().files()[1].path.clone();
            let deadline = Instant::now() + Duration::from_secs(60);
            let wait = |session: &mut Session| {
                let left = deadline.saturating_duration_since(Instant::now());
                wakes.recv_timeout(left).expect("a wake");
                session.answers()
            };
            let mut named = requests.contains(&Request::Still(next.clone()));
            while !named || session.viewer().image().is_none() {
                let requests = wait(&mut session);
                named |= requests.contains(&Request::Still(next.clone()));
            }
            let still = session.still(&next).expect("the next file's still");
            let viewer = session.viewer();
            let layout = (cells.columns, cells.image_rows(viewer.bar_visible()));
            let steps = (viewer.gamma(), viewer.contrast());
            let sixel = mode == Mode::Sixel;
            let modes = (false, sixel);
            let (ahead, _, _) = ahead::plan(&still, next.clone(), steps, cells.cell, layout, modes);
            drop(still);
            session.key(&Keystroke::new("j"));
            while session.viewer().image().is_none() || session.viewer().awaited().is_some() {
                wait(&mut session);
            }
            let viewer = session.viewer();
            let view = viewer.viewport();
            let pixels = frame::frame_pixels(view, true, !sixel, sixel, 20.0, view.view_size());
            assert_eq!(
                FrameKey::of(viewer, pixels.0, pixels.1),
                Some(ahead),
                "{mode:?}"
            );
        }
    }
}
