//! The event loop the Linux frontends share: one calloop loop that owns the [`Session`] and
//! carries out its requests, with the session's wake, its timer, the signals that end the
//! viewer and the watch on the current file. A frontend adds its own sources to the same loop,
//! and their callbacks get the [`Driver`], through which events go into the session.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use calloop::generic::Generic;
use calloop::ping::make_ping;
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{
    EventLoop, EventSource, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken,
};
use nsxiv_ng_core::Size;
use nsxiv_ng_core::viewer::{Keystroke, Viewer};
use nsxiv_ng_session::{EditSetup, Request, Session};
use rustix::fs::inotify;

/// What a frontend does for the driver. Its own sources' callbacks pass their events in through
/// the driver's methods.
pub trait Frontend: Sized + 'static {
    /// The window's content in logical units, the bar included, and its device scale, the
    /// pixels to a logical unit. The driver passes them to [`Session::resize`] at the start
    /// and on [`Driver::layout`].
    fn window(&self) -> (Size, f64);
    /// Carries out a request the driver leaves to the frontend: `Redraw`, `FitWindow`,
    /// `ResetWindowSize`, `ToggleFullscreen` and `Beep`.
    fn request(&mut self, session: &mut Session, request: Request);
    /// Gives the screen back to the user, before the key-handler runs and at the end.
    fn release(&mut self);
    /// Takes the screen again after the key-handler; a whole redraw follows.
    fn reclaim(&mut self);
    /// The error the viewer ends with where the loop failed, which may be the display's
    /// connection broken: by default the loop's own.
    fn lost(&self, error: io::Error) -> io::Error {
        error
    }
}

/// Edit mode as the Linux windows have it: annotation text in DejaVu Sans by name, which most
/// systems have, as fontdb reads no fontconfig aliases and its sans-serif is Arial; the
/// session's own text line; no recognition.
pub fn edit_setup() -> EditSetup {
    EditSetup {
        families: vec!["DejaVu Sans".to_owned(), "sans-serif".to_owned()],
        field: false,
        recognizer: None,
    }
}

/// The loop's handle, for a frontend's own sources, whose callbacks get the driver.
pub type Handle<F> = LoopHandle<'static, Driver<F>>;

/// Starts a key-handler with no signal blocked. The viewer's threads block SIGINT, SIGTERM and
/// SIGHUP for the loop, and a child inherits the mask: a handler whose shell keeps it, as bash
/// does where dash clears it, would hold its own SIGTERM, or a Ctrl-C, until it ended.
fn unblocked(command: &mut Command) {
    let unblock = || {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigemptyset fills in the set it is given, which then is initialized
        let empty = unsafe {
            libc::sigemptyset(set.as_mut_ptr());
            set.assume_init()
        };
        // SAFETY: the empty set as the new mask, and no old one asked for
        match unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) } {
            0 => Ok(()),
            error => Err(io::Error::from_raw_os_error(error)),
        }
    };
    // SAFETY: the closure runs in the child between fork and exec, and calls only
    // sigemptyset and pthread_sigmask, both async-signal-safe, on a set of its own
    unsafe { command.pre_exec(unblock) };
}

/// Runs a viewer until it quits, then exits the process with status 0.
///
/// `open` sets up the frontend and adds its sources through the handle. SIGINT, SIGTERM and
/// SIGHUP are blocked before it runs, and any signal it adds a source for must be too, before
/// `session` makes the session: the threads the session starts inherit the mask, so none of
/// them takes a signal meant for the loop. `session` gets the frontend, for where the bar
/// shows, and the wake to pass to [`Session::new`]. Returns only an error that keeps the
/// viewer from running, or that ended it, as a lost display, which [`Frontend::lost`] names,
/// the screen given back and no marks printed.
pub fn run<F: Frontend>(
    open: impl FnOnce(&Handle<F>) -> io::Result<F>,
    session: impl FnOnce(&F, Box<dyn Fn() + Send + Sync>) -> Session,
) -> io::Error {
    let mut event_loop = match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(e) => return e.into(),
    };
    let handle = event_loop.handle();
    let sources = (|| {
        let ending = Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP])?;
        insert(&handle, ending, |_, _, driver: &mut Driver<F>| {
            driver.quit()
        })?;
        let (ping, pinged) = make_ping()?;
        insert(&handle, pinged, |_, _, driver: &mut Driver<F>| {
            driver.event(Session::answers)
        })?;
        let watch = Watch::new()?;
        let events = Generic::new(watch.inotify.try_clone()?, Interest::READ, Mode::Level);
        insert(&handle, events, |_, _, driver: &mut Driver<F>| {
            if driver.watch.changed() {
                driver.event(Session::file_changed);
            }
            Ok(PostAction::Continue)
        })?;
        io::Result::Ok((ping, watch))
    })();
    let (ping, watch) = match sources {
        Ok(sources) => sources,
        Err(e) => return e,
    };
    let frontend = match open(&handle) {
        Ok(frontend) => frontend,
        Err(e) => return e,
    };
    let session = session(&frontend, Box::new(move || ping.ping()));
    let mut driver = Driver::new(session, frontend, handle, event_loop.get_signal(), watch);
    driver.start();
    let ran = event_loop.run(None, &mut driver, |_| {});
    driver.frontend.release();
    driver.flush_warnings();
    if let Err(e) = ran {
        return driver.frontend.lost(e.into());
    }
    let mut stdout = io::stdout().lock();
    for path in driver.quit.take().unwrap_or_default() {
        let _ = stdout.write_all(path.as_os_str().as_encoded_bytes());
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
    std::process::exit(0)
}

pub(crate) fn insert<F, S: EventSource + 'static>(
    handle: &Handle<F>,
    source: S,
    callback: impl FnMut(S::Event, &mut S::Metadata, &mut Driver<F>) -> S::Ret + 'static,
) -> io::Result<RegistrationToken> {
    handle
        .insert_source(source, callback)
        .map_err(|e| e.error.into())
}

/// One running viewer in its loop: the session, the frontend, and what the driver keeps for
/// the requests.
pub struct Driver<F> {
    session: Session,
    frontend: F,
    handle: Handle<F>,
    stop: LoopSignal,
    /// Warnings for `stderr` once the screen is the user's again.
    held: Vec<String>,
    stderr: Box<dyn Write>,
    watch: Watch,
    /// The timer that follows [`Session::deadline`], and the deadline it was set for.
    timer: Option<(RegistrationToken, Instant)>,
    /// The paths to print, once the viewer quit.
    quit: Option<Vec<PathBuf>>,
}

impl<F: Frontend> Driver<F> {
    fn new(
        mut session: Session,
        frontend: F,
        handle: Handle<F>,
        stop: LoopSignal,
        watch: Watch,
    ) -> Driver<F> {
        session.prepare_key_handler(unblocked);
        Driver {
            session,
            frontend,
            handle,
            stop,
            held: Vec::new(),
            stderr: Box::new(io::stderr()),
            watch,
            timer: None,
            quit: None,
        }
    }

    pub fn frontend(&mut self) -> &mut F {
        &mut self.frontend
    }

    pub fn viewer(&self) -> &Viewer {
        self.session.viewer()
    }

    /// Any event: `event` calls into the session, and the driver carries out the requests it
    /// returns. Nothing reaches the session once the viewer quit.
    pub fn event(&mut self, event: impl FnOnce(&mut Session) -> Vec<Request>) {
        self.frontend_event(|_, session| event(session));
    }

    /// An event of the frontend's own that needs the frontend and the session at once, as
    /// when work a frontend started on a thread of its own comes back and the next step asks
    /// the session; the driver carries out the requests `event` returns, as for [`Driver::event`],
    /// which is enough for an event that only calls into the session.
    pub fn frontend_event(&mut self, event: impl FnOnce(&mut F, &mut Session) -> Vec<Request>) {
        if self.quit.is_none() {
            let requests = event(&mut self.frontend, &mut self.session);
            self.carry(requests);
        }
    }

    /// A key, passed to the session; false when no binding took it.
    pub fn key(&mut self, key: &Keystroke) -> bool {
        let mut bound = true;
        self.event(|session| {
            session.key(key).unwrap_or_else(|| {
                bound = false;
                Vec::new()
            })
        });
        bound
    }

    /// Whether the session's text line is open, which takes typed characters as keys.
    pub fn typing(&self) -> bool {
        self.session.typing()
    }

    /// Ends the viewer as the q key does, so -o still prints, a C-x prefix ended first, which
    /// would give the q to the key-handler, and an edit dropped, which would take the q.
    pub fn quit(&mut self) {
        self.event(|session| {
            let mut requests = session.end_edit();
            session.end_prefix();
            requests.extend(session.key(&Keystroke::new("q")).unwrap_or_default());
            requests
        });
    }

    /// The frontend's window changed, or may have, as on a resize whose cells stay and whose
    /// pixels change: the session gets its size and scale, and a whole frame is drawn.
    pub fn layout(&mut self) {
        let (window, scale) = self.frontend.window();
        self.event(|session| session.resize(window, scale));
    }

    /// A whole frame is due, as when the window was exposed.
    pub fn redraw(&mut self) {
        self.event(|_| vec![Request::Redraw { bar_only: false }]);
    }

    fn start(&mut self) {
        let (window, scale) = self.frontend.window();
        self.event(|session| {
            let mut requests = session.resize(window, scale);
            requests.extend(session.start());
            requests
        });
    }

    // Carries out the requests in order and one redraw last, whole unless every redraw asked
    // was of the bar alone; then the timer follows the session's deadline.
    fn carry(&mut self, requests: Vec<Request>) {
        let mut queue = VecDeque::from(requests);
        let mut redraw = None;
        while let Some(request) = queue.pop_front() {
            match request {
                Request::Redraw { bar_only } => {
                    redraw = Some(redraw.unwrap_or(true) && bar_only);
                }
                Request::Warning(line) => self.held.push(line),
                Request::Watch(path) => self.watch.set(path.as_deref()),
                Request::RunKeyHandler => {
                    self.frontend.release();
                    self.flush_warnings();
                    let requests = self.session.run_key_handler();
                    self.frontend.reclaim();
                    redraw = Some(false);
                    queue.extend(requests);
                }
                Request::Quit(paths) => {
                    self.quit = Some(paths);
                    self.stop.stop();
                    return;
                }
                other => self.frontend.request(&mut self.session, other),
            }
        }
        if let Some(bar_only) = redraw {
            self.frontend
                .request(&mut self.session, Request::Redraw { bar_only });
        }
        self.follow_deadline();
    }

    fn follow_deadline(&mut self) {
        let deadline = self.session.deadline();
        if deadline == self.timer.map(|(_, at)| at) {
            return;
        }
        if let Some((token, _)) = self.timer.take() {
            self.handle.remove(token);
        }
        let Some(at) = deadline else {
            return;
        };
        let fired = |_, _: &mut (), driver: &mut Driver<F>| {
            driver.timer = None;
            driver.event(|session| session.timers(Instant::now()));
            TimeoutAction::Drop
        };
        // a timer the loop cannot take never fires, which only stops an animation or a slideshow
        if let Ok(token) = insert(&self.handle, Timer::from_deadline(at), fired) {
            self.timer = Some((token, at));
        }
    }

    // each line cleaned, as it may name a file
    fn flush_warnings(&mut self) {
        for line in self.held.drain(..) {
            let line = crate::printable(&line);
            let _ = writeln!(self.stderr, "{}: {line}", nsxiv_ng_core::config::APP_NAME);
        }
        let _ = self.stderr.flush();
    }
}

/// The inotify watches on the one file the viewer watches, through its folder, since an atomic
/// save renames a new file over the old name. A symlink is watched twice: its target, whose
/// writes change the image, and the link itself, which may be pointed elsewhere.
struct Watch {
    inotify: OwnedFd,
    /// Each folder's watch, a name in it, and the events on that name that count.
    watched: Vec<(i32, OsString, inotify::ReadFlags)>,
}

impl Watch {
    fn new() -> io::Result<Watch> {
        let inotify =
            inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)?;
        Ok(Watch {
            inotify,
            watched: Vec::new(),
        })
    }

    /// Watches `path`, or nothing. A watch is added again on every request, as a save may have
    /// put a new file under the name, or a link may point elsewhere now.
    fn set(&mut self, path: Option<&Path>) {
        use inotify::ReadFlags as Read;
        let mut names = Vec::new();
        if let Some(path) = path {
            if path.is_symlink() {
                // a link replaced by a rename, as `ln -sfn` does, or removed and made again
                names.push((path.to_owned(), Read::MOVED_TO | Read::CREATE));
                if let Ok(target) = std::fs::canonicalize(path) {
                    names.push((target, Read::CLOSE_WRITE | Read::MOVED_TO));
                }
            } else {
                names.push((path.to_owned(), Read::CLOSE_WRITE | Read::MOVED_TO));
            }
        }
        let flags = inotify::WatchFlags::CLOSE_WRITE
            | inotify::WatchFlags::MOVED_TO
            | inotify::WatchFlags::CREATE;
        let watched: Vec<_> = names
            .into_iter()
            .filter_map(|(path, counts)| {
                let name = path.file_name()?.to_owned();
                let folder = match path.parent() {
                    Some(folder) if !folder.as_os_str().is_empty() => folder,
                    _ => Path::new("."),
                };
                let wd = inotify::add_watch(&self.inotify, folder, flags).ok()?;
                Some((wd, name, counts))
            })
            .collect();
        for (old, _, _) in std::mem::replace(&mut self.watched, watched) {
            if !self.watched.iter().any(|(wd, _, _)| *wd == old) {
                let _ = inotify::remove_watch(&self.inotify, old);
            }
        }
    }

    /// Whether the events that came name a watched file as it counts; reads them all.
    fn changed(&mut self) -> bool {
        let mut buffer = [std::mem::MaybeUninit::uninit(); 4096];
        let mut events = inotify::Reader::new(&self.inotify, &mut buffer);
        let mut changed = false;
        while let Ok(event) = events.next() {
            let Some(file) = event.file_name() else {
                continue;
            };
            changed |= self.watched.iter().any(|(wd, name, counts)| {
                event.wd() == *wd
                    && file.to_bytes() == name.as_encoded_bytes()
                    && event.events().intersects(*counts)
            });
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsxiv_ng_core::config::Config;
    use nsxiv_ng_core::file_list::FileEntry;
    use nsxiv_ng_core::options::Options;
    use nsxiv_ng_session::{Bar, Button, EditSetup};
    use std::time::Duration;

    struct Fake {
        window: Size,
        log: Vec<String>,
        /// A file the key-handler makes, and whether it was there at each release and reclaim.
        mark: PathBuf,
        marks: Vec<bool>,
    }

    impl Frontend for Fake {
        fn window(&self) -> (Size, f64) {
            (self.window, 1.25)
        }
        fn request(&mut self, _: &mut Session, request: Request) {
            self.log.push(format!("{request:?}"));
        }
        fn release(&mut self) {
            self.log.push("release".to_owned());
            self.marks.push(self.mark.exists());
        }
        fn reclaim(&mut self) {
            self.log.push("reclaim".to_owned());
            self.marks.push(self.mark.exists());
        }
    }

    const WINDOW: Size = Size {
        width: 800.0,
        height: 480.0,
    };

    fn driver(
        event_loop: &EventLoop<'static, Driver<Fake>>,
        files: Vec<FileEntry>,
        key_handlers: Vec<PathBuf>,
    ) -> Driver<Fake> {
        let program = PathBuf::from("nsxiv-ng");
        let config = Config::default();
        let options = Options::default();
        let bar = Bar::Shown(16.0);
        let session = Session::new(options, files, &config, program, key_handlers, bar, || {});
        let fake = Fake {
            window: WINDOW,
            log: Vec::new(),
            mark: PathBuf::from("/nonexistent/mark"),
            marks: Vec::new(),
        };
        let watch = Watch::new().unwrap();
        Driver::new(
            session,
            fake,
            event_loop.handle(),
            event_loop.get_signal(),
            watch,
        )
    }

    fn missing() -> Vec<FileEntry> {
        (0..3)
            .map(|i| FileEntry::new(PathBuf::from(format!("/nonexistent/{i}.png"))))
            .collect()
    }

    // a folder of the test's own under the repository's target folder
    fn folder(name: &str) -> PathBuf {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("nsxiv-ng-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn requests_go_in_order_with_one_redraw_last_and_warnings_held() {
        let event_loop = EventLoop::try_new().unwrap();
        let mut driver = driver(&event_loop, missing(), vec![]);
        driver.carry(vec![
            Request::Redraw { bar_only: true },
            Request::Warning("a.png: broken".to_owned()),
            Request::Beep,
            Request::ToggleFullscreen,
            Request::Redraw { bar_only: true },
        ]);
        assert_eq!(
            driver.frontend.log,
            ["Beep", "ToggleFullscreen", "Redraw { bar_only: true }"]
        );
        assert_eq!(
            driver.held,
            ["a.png: broken"],
            "held for when the screen is free"
        );
        driver.frontend.log.clear();
        driver.carry(vec![
            Request::Redraw { bar_only: true },
            Request::Redraw { bar_only: false },
        ]);
        assert_eq!(
            driver.frontend.log,
            ["Redraw { bar_only: false }"],
            "the bar alone only when every redraw was of the bar"
        );
    }

    #[test]
    fn the_key_handler_gets_the_screen_after_the_warnings_and_dies_of_its_own_sigterm() {
        let folder = folder("handler");
        let (handler, mark, warnings) = (
            folder.join("key-handler"),
            folder.join("mark"),
            folder.join("warnings"),
        );
        // it notes the warnings written so far and its key, then sends itself SIGTERM, which
        // must end it, though the viewer's thread blocks SIGTERM
        let script = format!(
            "#!/bin/sh\n{{ cat '{}'; echo \"$1\"; }} >'{}'\nkill -TERM $$\necho survived >>'{}'\n",
            warnings.display(),
            mark.display(),
            mark.display()
        );
        std::fs::write(&handler, script).unwrap();
        std::fs::set_permissions(
            &handler,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let _blocked = Signals::new(&[Signal::SIGTERM]).unwrap();
        let event_loop = EventLoop::try_new().unwrap();
        let photo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/photo.jpg");
        let files = vec![FileEntry::new(photo)];
        let mut driver = driver(&event_loop, files, vec![handler]);
        driver.frontend.mark = mark.clone();
        driver.stderr = Box::new(std::fs::File::create(&warnings).unwrap());
        driver.start();
        driver.carry(vec![Request::Warning("held".to_owned())]);
        driver.frontend.log.clear();
        assert!(driver.key(&Keystroke::ctrl("x")));
        assert!(driver.key(&Keystroke::new("a")));
        assert_eq!(driver.frontend.log[..2], ["release", "reclaim"]);
        assert_eq!(
            driver.frontend.log.last().map(String::as_str),
            Some("Redraw { bar_only: false }"),
            "the screen comes back drawn anew"
        );
        assert_eq!(driver.frontend.marks, [false, true], "it ran in between");
        assert!(driver.held.is_empty());
        assert_eq!(
            std::fs::read_to_string(&mark).unwrap(),
            "nsxiv-ng: held\na\n",
            "the warnings were written before it ran, and SIGTERM ended it"
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_frontends_own_event_gets_the_frontend_and_the_session_and_its_requests_are_carried() {
        let event_loop = EventLoop::try_new().unwrap();
        let mut driver = driver(&event_loop, missing(), vec![]);
        driver.frontend_event(|fake, session| {
            fake.log
                .push(format!("{} files", session.viewer().files().len()));
            vec![Request::Redraw { bar_only: true }, Request::Beep]
        });
        assert_eq!(
            driver.frontend.log,
            ["3 files", "Beep", "Redraw { bar_only: true }"]
        );
    }

    #[test]
    fn quit_ends_the_loop_and_nothing_reaches_the_session_after() {
        let event_loop = EventLoop::try_new().unwrap();
        let mut driver = driver(&event_loop, missing(), vec![]);
        let marked = vec![PathBuf::from("/a.png")];
        driver.carry(vec![
            Request::Quit(marked.clone()),
            Request::Beep,
            Request::Redraw { bar_only: false },
        ]);
        assert_eq!(driver.quit, Some(marked));
        assert!(driver.frontend.log.is_empty());
        driver.event(|_| panic!("an event after the end"));
        driver.frontend_event(|_, _| panic!("a frontend's event after the end"));
        assert!(
            driver.key(&Keystroke::new("j")),
            "keys after the end change nothing"
        );
        driver.redraw();
        assert!(driver.frontend.log.is_empty());
    }

    #[test]
    fn a_signal_ends_the_viewer_as_q_does_even_after_c_x() {
        let event_loop = EventLoop::try_new().unwrap();
        let mut driver = driver(&event_loop, missing(), vec![]);
        driver.start();
        assert!(driver.key(&Keystroke::ctrl("x")));
        driver.quit();
        assert_eq!(driver.quit, Some(Vec::new()));
    }

    #[test]
    fn a_signal_ends_the_viewer_while_an_annotations_words_are_typed() {
        let event_loop = EventLoop::try_new().unwrap();
        let photo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/photo.jpg");
        let mut driver = driver(&event_loop, vec![FileEntry::new(photo)], vec![]);
        driver.session.can_edit(EditSetup {
            families: Vec::new(),
            field: false,
            recognizer: None,
        });
        driver.start();
        for _ in 0..600 {
            if driver.viewer().image().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            driver.event(Session::answers);
        }
        // x, t and a click: the session's own line takes the keys
        driver.key(&Keystroke::new("x"));
        driver.key(&Keystroke::new("t"));
        let at = nsxiv_ng_core::Point { x: 100.0, y: 100.0 };
        driver.event(|session| {
            let mut requests = session.press(Button::Left, at, 0);
            requests.extend(session.release(Button::Left, at));
            requests
        });
        assert!(driver.typing());
        driver.quit();
        assert_eq!(driver.quit, Some(Vec::new()));
    }

    #[test]
    fn a_layout_resizes_the_session_and_redraws_whole_and_the_bar_is_the_sessions() {
        let event_loop = EventLoop::try_new().unwrap();
        let mut driver = driver(&event_loop, missing(), vec![]);
        driver.start();
        // the bar, which starts hidden, shown
        driver.key(&Keystroke::new("b"));
        let size = |width, height| Size { width, height };
        assert_eq!(driver.viewer().viewport().view_size(), size(800.0, 464.0));
        driver.frontend.log.clear();
        // the cells stay and their pixels change
        driver.layout();
        assert_eq!(driver.frontend.log, ["Redraw { bar_only: false }"]);
        driver.frontend.window = size(640.0, 320.0);
        driver.layout();
        assert_eq!(driver.viewer().viewport().view_size(), size(640.0, 304.0));
        // the session takes the bar's room off itself
        driver.key(&Keystroke::new("b"));
        assert_eq!(driver.viewer().viewport().view_size(), size(640.0, 320.0));
        driver.frontend.log.clear();
        driver.redraw();
        assert_eq!(driver.frontend.log, ["Redraw { bar_only: false }"]);
    }

    #[test]
    fn the_timer_follows_the_sessions_deadline() {
        let event_loop = EventLoop::try_new().unwrap();
        let mut driver = driver(&event_loop, missing(), vec![]);
        driver.start();
        // a slideshow counts down once its image is in, here a file that is not there
        let waited = Instant::now();
        while driver.viewer().awaited().is_some() {
            assert!(waited.elapsed() < Duration::from_secs(60), "no answer");
            std::thread::sleep(Duration::from_millis(10));
            driver.event(Session::answers);
        }
        assert_eq!(driver.timer, None);
        driver.key(&Keystroke::new("s"));
        let deadline = driver.session.deadline();
        assert!(deadline.is_some());
        assert_eq!(driver.timer.map(|(_, at)| at), deadline);
        driver.key(&Keystroke::new("s"));
        assert_eq!(driver.timer, None, "a stopped slideshow sets no timer");
    }

    #[test]
    fn held_warnings_reach_stderr_without_control_characters() {
        let folder = folder("warnings");
        let warnings = folder.join("warnings");
        let event_loop = EventLoop::try_new().unwrap();
        let mut driver = driver(&event_loop, missing(), vec![]);
        driver.stderr = Box::new(std::fs::File::create(&warnings).unwrap());
        let named = "/a\x1b]52;c;eA==\x07b\u{9b}.png: not an image".to_owned();
        driver.carry(vec![Request::Warning(named)]);
        driver.flush_warnings();
        assert_eq!(
            std::fs::read_to_string(&warnings).unwrap(),
            "nsxiv-ng: /a?]52;c;eA==?b?.png: not an image\n"
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_watched_link_changes_when_it_points_elsewhere_or_its_target_is_written() {
        use std::os::unix::fs::symlink;
        let folder = folder("link");
        let links = folder.join("links");
        std::fs::create_dir_all(&links).unwrap();
        let (a, b) = (folder.join("a.png"), folder.join("b.png"));
        put(&a, b"a");
        put(&b, b"b");
        let (latest, other, new) = (links.join("latest"), links.join("other"), links.join("new"));
        symlink(&a, &latest).unwrap();
        symlink(&a, &other).unwrap();
        let mut watch = Watch::new().unwrap();
        watch.set(Some(&latest));
        std::fs::write(&a, b"a again").unwrap();
        assert!(soon(&mut watch), "the target written");
        // another link in the same folder pointed elsewhere, as ln -sfn does
        symlink(&b, &new).unwrap();
        std::fs::rename(&new, &other).unwrap();
        assert!(!watch.changed(), "another link");
        symlink(&b, &new).unwrap();
        std::fs::rename(&new, &latest).unwrap();
        assert!(
            soon(&mut watch),
            "the link pointed elsewhere by a rename over it"
        );
        // the session asks again after the reload, which follows the link anew
        watch.set(Some(&latest));
        std::fs::write(&a, b"a once more").unwrap();
        assert!(!watch.changed(), "the old target");
        std::fs::write(&b, b"b again").unwrap();
        assert!(soon(&mut watch), "the new target written");
        std::fs::remove_file(&latest).unwrap();
        symlink(&a, &latest).unwrap();
        assert!(soon(&mut watch), "the link removed and made again");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    // A write's event comes when the file is closed for the last time, which a child another
    // test forks meanwhile delays until it runs its program, as it holds the descriptor till
    // then: a change is waited for, and the files the test starts with come by rename, which
    // has no descriptor to hold.
    fn soon(watch: &mut Watch) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if watch.changed() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn put(path: &Path, content: &[u8]) {
        let made = path.with_extension("new");
        std::fs::write(&made, content).unwrap();
        std::fs::rename(&made, path).unwrap();
    }

    #[test]
    fn the_watch_tells_a_rewrite_or_a_rename_over_the_file_from_others() {
        let folder = folder("watch");
        let (file, other) = (folder.join("a.png"), folder.join("b.png"));
        put(&file, b"old");
        let mut watch = Watch::new().unwrap();
        watch.set(Some(&file));
        assert!(!watch.changed(), "nothing yet");
        std::fs::write(&other, b"other").unwrap();
        assert!(!watch.changed(), "another file in the folder");
        std::fs::write(&file, b"new").unwrap();
        assert!(soon(&mut watch), "written");
        // a save that renames a new file over the name, after the watch was asked for again
        watch.set(Some(&file));
        std::fs::rename(&other, &file).unwrap();
        assert!(soon(&mut watch), "replaced");
        watch.set(None);
        std::fs::write(&file, b"again").unwrap();
        assert!(!watch.changed(), "no longer watched");
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
