//! The application, its window and the view that shows the session's frame. Everything runs
//! on the main thread, which owns the viewer; the session's threads, timers, signals and file
//! events come to it through the main queue.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::CStr;
use std::ffi::c_void;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use block2::{RcBlock, StackBlock};
use dispatch2::{DispatchObject, DispatchQueue, DispatchRetained, DispatchSource, DispatchTime};
use nsxiv_ng_core::config::{APP_NAME, ColorsSet, Config};
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::{self, Options};
use nsxiv_ng_core::viewer::Keystroke;
use nsxiv_ng_core::{Point, Rect, Size, printable};
use nsxiv_ng_render::Canvas;
use nsxiv_ng_session::{
    Bar, Button, EditSetup, Frame, Palette, Request, Scroll, Session, key_handlers,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearanceCustomization, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSBackingStoreType, NSBeep, NSColor, NSColorSpace, NSEvent,
    NSEventMask, NSEventModifierFlags, NSFocusRingType, NSFont, NSMenu, NSMenuItem, NSPasteboard,
    NSPasteboardTypePNG, NSPasteboardTypeString, NSResponder, NSScreen, NSTextField, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask, NSWindowTabbingMode,
};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, CGPoint, CGRect, CGSize,
    kCFRunLoopCommonModes,
};
use objc2_foundation::{
    NSArray, NSData, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect,
    NSSize, NSString, NSURL,
};
use objc2_quartz_core::{
    CALayer, CATransaction, kCAFilterNearest, kCAGravityBottomLeft, kCAGravityTopLeft,
};

use crate::fonts;
use crate::frame;
use crate::geometry::{fit, flip, kept_shape, placed, zoomed};
use crate::keys::key_name;
use crate::title::TitleStrip;
use crate::watch::Watch;

/// How many frames were drawn over the last one copied in, which a pan moves, for the window
/// tests to see that it did; counting them changes nothing drawn.
#[doc(hidden)]
pub static MOVED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// How many keys the window's key monitor let go on, and the code of the last, for the window
/// tests to see which keys the viewer took; counting them changes nothing done with them.
#[doc(hidden)]
pub static KEYS_PASSED: AtomicUsize = AtomicUsize::new(0);
#[doc(hidden)]
pub static LAST_KEY_PASSED: AtomicU16 = AtomicU16::new(u16::MAX);

thread_local! {
    /// The viewer, once the application has it.
    static VIEWER: RefCell<Option<Viewer>> = const { RefCell::new(None) };
    /// The window's size changed and the session has yet to lay out for it.
    static LAYOUT_DUE: Cell<bool> = const { Cell::new(false) };
    /// What starts the viewer, kept until files opened from Finder come for it to show.
    static WAITING: RefCell<Option<Start>> = const { RefCell::new(None) };
    /// The files paths opened from Finder name, as the command line's are found.
    static FILES: RefCell<Option<Files>> = const { RefCell::new(None) };
    /// The application finished launching.
    static LAUNCHED: Cell<bool> = const { Cell::new(false) };
}

/// The files a viewer shows of paths opened from Finder: a folder's images, as the command
/// line's are found, each warned of that is missing.
pub type Files = Box<dyn Fn(&[PathBuf]) -> Vec<FileEntry>>;

/// How long a viewer started with no files waits for files opened from Finder, which come
/// once the application launched, before it has nothing to show.
const OPEN_WAIT: Duration = Duration::from_secs(2);

/// What the viewer starts with but its files.
struct Start {
    options: Options,
    config: Config,
    program: PathBuf,
}

/// Runs the viewer of `files` in a window until it quits, then exits the process with status
/// 0, the marked files printed where `-o` asks. `program` is the viewer's own binary, which
/// decoding helpers run as. Returns only an error that keeps the viewer from starting.
///
/// With no files, as an application opened from Finder gets its files in an event once it
/// launched, the viewer waits [`OPEN_WAIT`] for them; with none by then it prints the usage
/// and exits with status 2, as the command line with no files does. Files opened while it
/// runs come after the current one. `found` makes the files of the paths opened.
pub fn run(
    options: Options,
    files: Vec<FileEntry>,
    config: &Config,
    program: PathBuf,
    found: Files,
) -> io::Error {
    let Some(mtm) = MainThreadMarker::new() else {
        return io::Error::other("the macOS frontend must start on the main thread");
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.setMainMenu(Some(&menu(mtm)));
    FILES.with(|cell| *cell.borrow_mut() = Some(found));
    let start = Start {
        options,
        config: config.clone(),
        program,
    };
    if files.is_empty() {
        WAITING.with(|cell| *cell.borrow_mut() = Some(start));
    } else {
        open_viewer(mtm, &delegate, start, files);
    }
    let _signals = signals();
    let _frames = frames();
    let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes the event it is about to send, alive for the call
        let event_ref = unsafe { event.as_ref() };
        if key(event_ref) {
            std::ptr::null_mut()
        } else {
            // counted first, so a test that sees the code with acquire sees the count too
            KEYS_PASSED.fetch_add(1, Ordering::Relaxed);
            LAST_KEY_PASSED.store(event_ref.keyCode(), Ordering::Release);
            event.as_ptr()
        }
    });
    // SAFETY: the block returns the event it got, or null to take it
    let _monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block)
    };
    let up = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes the event it is about to send, alive for the call
        key_up(unsafe { event.as_ref() });
        event.as_ptr()
    });
    // SAFETY: the block returns the event it got, which goes on as it would
    let _up_monitor =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyUp, &up) };
    app.run();
    io::Error::other("the application stopped")
}

/// Makes the viewer of `files`, its window and its session, which starts once the
/// application launched.
fn open_viewer(mtm: MainThreadMarker, delegate: &Delegate, start: Start, files: Vec<FileEntry>) {
    let Start {
        options,
        config,
        program,
    } = start;
    let config = &config;
    let configured = Size {
        width: config.window_width,
        height: config.window_height,
    };
    let size = options.geometry.unwrap_or(configured);
    // -g fixes the size, which the grid then keeps
    let reset_size = options.geometry.is_none().then_some(configured);
    let (window, view) = window(mtm, size, delegate);
    let strip = TitleStrip::install(&window, &view);
    let layers = (CALayer::new(), CALayer::new());
    if let Some(root) = view.layer() {
        root.addSublayer(&layers.0);
        root.addSublayer(&layers.1);
    }

    let pending = Arc::new(AtomicBool::new(false));
    let wake = move || {
        // only the wake that finds none pending queues a call, which clears the flag first
        if !pending.swap(true, Ordering::AcqRel) {
            let pending = Arc::clone(&pending);
            later(move |viewer| {
                pending.store(false, Ordering::Release);
                viewer.session.answers()
            });
        }
    };
    let mut session = Session::new(
        options,
        files,
        config,
        program,
        key_handlers(),
        Bar::Drawn,
        wake,
    );
    session.can_edit(EditSetup {
        families: fonts::FAMILIES.map(str::to_owned).to_vec(),
        field: true,
        recognizer: Some(Arc::new(crate::vision::Vision)),
    });
    session.map_fonts(Arc::new(fonts::map));
    let configured = Palette {
        background: config.background,
        foreground: config.foreground,
        bar_background: config.bar_background,
        selection: config.selection_color,
        mark: config.mark_color,
        placeholder: config.bar_background,
        error: config.foreground,
    };
    let viewer = Viewer {
        session,
        configured,
        colors_set: config.colors_set,
        window,
        view,
        strip,
        image_layer: layers.0,
        bar_layer: layers.1,
        surfaces: frame::Surfaces::new(),
        reset_size,
        shown: false,
        fullscreen_due: false,
        fullscreen: false,
        due: None,
        kept: None,
        title: None,
        deadline: None,
        timer: 0,
        watch: Watch::new(),
        field: None,
        field_target: FieldTarget::new(mtm),
        fields: 0,
    };
    VIEWER.with(|cell| *cell.borrow_mut() = Some(viewer));
    if LAUNCHED.get() {
        later(Viewer::start);
    }
}

/// The files of the file URLs `urls`, found as the command line's are.
fn opened(urls: &NSArray<NSURL>) -> Vec<FileEntry> {
    let paths: Vec<PathBuf> = (0..urls.count())
        .map(|i| urls.objectAtIndex(i))
        .filter(|url| url.isFileURL())
        .map(|url| {
            // SAFETY: fileSystemRepresentation gives a NUL-terminated string that holds while
            // `url`, retained for this closure, does; its bytes are copied before it returns
            let path = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
            Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())).to_owned()
        })
        .collect();
    FILES
        .with(|cell| cell.borrow().as_ref().map(|found| found(&paths)))
        .unwrap_or_default()
}

/// One running viewer: the session and the window that shows it.
struct Viewer {
    session: Session,
    /// The config's colors, and which of them its file set, which the system's do not replace.
    configured: Palette,
    colors_set: ColorsSet,
    window: Retained<NSWindow>,
    view: Retained<CanvasView>,
    strip: Retained<TitleStrip>,
    /// The frame's rows above the bar, and the bar's rows, each a part of the surface the
    /// frame is drawn in, or an image of its own.
    image_layer: Retained<CALayer>,
    bar_layer: Retained<CALayer>,
    /// The surfaces frames are drawn in, and the one shown.
    surfaces: frame::Surfaces,
    /// The size ResetWindowSize gives the window, None where -g fixed it.
    reset_size: Option<Size>,
    /// The window is on screen, once the session's first frame was final.
    shown: bool,
    /// Fullscreen was asked for before the window showed.
    fullscreen_due: bool,
    /// The window is fullscreen, from the time it entered until it left.
    fullscreen: bool,
    /// A frame is due, of the bar alone where true, drawn once the events at hand are through.
    due: Option<bool>,
    /// The last whole frame's width and height in pixels, scale and bar rows, which a frame
    /// of the bar alone must match.
    kept: Option<(u32, u32, u64, u32)>,
    /// The file the window's title names.
    title: Option<PathBuf>,
    /// The deadline the timer is set for, and the timer's number, which an older one's call
    /// does not match.
    deadline: Option<Instant>,
    timer: u64,
    watch: Watch,
    /// An edit's text field, while it is open, and what its Return tells. Each field gets a
    /// number of its own, its tag, which a queued end of it names.
    field: Option<Retained<NSTextField>>,
    field_target: Retained<FieldTarget>,
    fields: isize,
}

/// Calls `event` with the viewer and carries out the requests it returns; gives the event
/// back where the viewer was busy, as in a call from AppKit inside one of its own.
fn with<F: FnOnce(&mut Viewer) -> Vec<Request>>(event: F) -> Result<(), F> {
    VIEWER.with(|cell| {
        let Ok(mut viewer) = cell.try_borrow_mut() else {
            return Err(event);
        };
        if let Some(viewer) = viewer.as_mut() {
            let requests = event(viewer);
            viewer.carry(requests);
        }
        Ok(())
    })
}

/// [`with`] from the main queue, and again later where the viewer is busy then, so the event
/// is never lost: a wake, a timer, a signal, the window's close.
fn later(event: impl FnOnce(&mut Viewer) -> Vec<Request> + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        if let Err(event) = with(event) {
            later(event);
        }
    });
}

/// The session lays out for the window's size once the current call is done, and the frame
/// drawn meanwhile waits for it.
fn lay_out() {
    if LAYOUT_DUE.replace(true) {
        return;
    }
    later(|viewer| {
        LAYOUT_DUE.set(false);
        let (size, scale) = viewer.size();
        let mut requests = viewer.session.resize(size, scale);
        requests.push(Request::Redraw { bar_only: false });
        requests
    });
}

/// An event of the file watch, from its vnode sources on the main queue.
pub(crate) fn heard(event: usize) {
    later(move |viewer| match viewer.watch.changed(event) {
        true => viewer.session.file_changed(),
        false => Vec::new(),
    });
}

impl Viewer {
    // The session's start, once the application launched: the system's appearance, the
    // window's size, then the first file.
    fn start(&mut self) -> Vec<Request> {
        let (size, scale) = self.size();
        let mut requests = self.follow_appearance();
        requests.extend(self.session.resize(size, scale));
        requests.extend(self.session.start());
        requests
    }

    // Carries out the requests in order, then the title and the timer follow. The redraws
    // asked make one frame due, whole unless every redraw asked was of the bar alone, as
    // Linux's driver does; [`frames`] draws it.
    fn carry(&mut self, requests: Vec<Request>) {
        let mut queue = VecDeque::from(requests);
        let mut redraw = None;
        while let Some(request) = queue.pop_front() {
            match request {
                Request::Redraw { bar_only } => {
                    redraw = Some(redraw.unwrap_or(true) && bar_only);
                }
                // the window is not the terminal, which a line could garble
                Request::Warning(line) => eprintln!("{APP_NAME}: {}", printable(&line)),
                Request::Watch(path) => self.watch.set(path.as_deref()),
                Request::RunKeyHandler => {
                    queue.extend(self.session.run_key_handler());
                    redraw = Some(false);
                }
                Request::Quit(paths) => quit(&paths),
                Request::FitWindow { image, bar } => self.fit(image, bar),
                Request::ResetWindowSize => self.reset(),
                Request::ToggleFullscreen if self.shown => self.window.toggleFullScreen(None),
                Request::ToggleFullscreen => self.fullscreen_due = !self.fullscreen_due,
                Request::Beep => NSBeep(),
                Request::EditText {
                    rect,
                    text,
                    font,
                    color,
                } => self.open_field(rect, &text, font, color),
                Request::EndText => queue.extend(self.end_field()),
                Request::CopyImage(png) => {
                    queue.extend(copied(copy(|board, (kind, _)| {
                        board.setData_forType(Some(&NSData::with_bytes(&png)), kind)
                    })));
                }
                Request::CopyText(text) => {
                    queue.extend(copied(copy(|board, (_, string)| {
                        board.setString_forType(&NSString::from_str(&text), string)
                    })));
                }
                // a terminal's
                Request::Forget(_) | Request::Still(_) => {}
            }
        }
        self.name();
        if let Some(bar_only) = redraw {
            self.due = Some(self.due.unwrap_or(true) && bar_only);
        }
        self.follow_deadline();
    }

    // The window's content in points, the bar's rows included, and its backing scale.
    fn size(&self) -> (Size, f64) {
        let bounds = self.view.bounds();
        let size = Size {
            width: bounds.size.width,
            height: bounds.size.height,
        };
        (size, self.window.backingScaleFactor())
    }

    // The current file's name as the window's title, its path as the represented file.
    fn name(&mut self) {
        let path = self
            .session
            .viewer()
            .current_entry()
            .map(|e| e.path.clone());
        if path == self.title {
            return;
        }
        if let Some(path) = &path {
            let name = path.file_name().unwrap_or(path.as_os_str());
            self.window
                .setTitle(&NSString::from_str(&name.to_string_lossy()));
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            self.window.setRepresentedURL(Some(&url));
        }
        self.title = path;
    }

    // A frame, the whole or the bar alone, given to the layers; the first, once final, shows
    // the window.
    fn draw(&mut self, bar_only: bool) {
        // the layout that is due draws the frame
        if LAYOUT_DUE.get() {
            return;
        }
        // nothing on screen before the first frame is final, so the window shows at its size
        if !self.shown && !self.session.frame_is_final() {
            return;
        }
        let (size, scale) = self.size();
        let pixels = |side: f64| (side * scale).round() as u32;
        let (width, height) = (pixels(size.width), pixels(size.height));
        if width == 0 || height == 0 {
            return;
        }
        let rows = self.session.bar_rows(height, scale);
        let kept = Some((width, height, scale.to_bits(), rows));
        CATransaction::begin();
        // a new frame replaces the one before at once, where a layer of one's own fades
        CATransaction::setDisableActions(true);
        if bar_only && rows > 0 && kept == self.kept {
            // the bar alone in an image of its own, over the frame the surface shown holds
            let mut bar = vec![0; width as usize * rows as usize];
            if let Some(mut canvas) = Canvas::new(&mut bar, width, rows) {
                self.session.draw_bar(&mut canvas, scale);
            }
            self.bar_layer.setContentsRect(WHOLE);
            show(
                &self.bar_layer,
                frame::image(bar, width as usize).as_deref(),
            );
        } else {
            let points = |pixels: u32| f64::from(pixels) / scale;
            // SAFETY: constant strings Core Animation defines
            let gravities = unsafe { [kCAGravityTopLeft, kCAGravityBottomLeft] };
            // SAFETY: as above
            let nearest = unsafe { kCAFilterNearest };
            for (layer, gravity) in [&self.image_layer, &self.bar_layer]
                .into_iter()
                .zip(gravities)
            {
                layer.setContentsScale(scale);
                layer.setContentsGravity(gravity);
                // a pixel of the frame a pixel on screen, where a part of a surface may sit
                // between the screen's pixels
                layer.setMagnificationFilter(nearest);
                layer.setMinificationFilter(nearest);
            }
            self.image_layer.setFrame(rect(Rect {
                x: 0.0,
                y: points(rows),
                width: size.width,
                height: points(height - rows),
            }));
            self.bar_layer.setFrame(rect(Rect {
                x: 0.0,
                y: 0.0,
                width: size.width,
                height: points(rows),
            }));
            self.draw_whole((width, height), rows, scale);
            self.kept = kept;
        }
        CATransaction::commit();
        if !self.shown {
            self.shown = true;
            self.window.makeKeyAndOrderFront(None);
            if std::mem::take(&mut self.fullscreen_due) {
                self.window.toggleFullScreen(None);
            }
        }
    }

    // A whole frame of `size` pixels, `rows` of them the bar's, drawn at `scale` in the first
    // surface the window server no longer uses, which both layers show a part of; where the
    // surface shown holds the last frame and a pan would move it, it is copied in first, so
    // the session draws only what came into view. Past the surfaces' bound, a new buffer an
    // image owns.
    fn draw_whole(&mut self, (width, height): (u32, u32), rows: u32, scale: f64) {
        let above = height - rows;
        if !self.draw_surface((width, height), rows, scale) {
            let mut whole = vec![0; width as usize * height as usize];
            if let Some(mut canvas) = Canvas::new(&mut whole, width, height) {
                self.session.draw(&mut canvas, scale, Frame::default());
            }
            let image = frame::image(whole, width as usize);
            let part = |top, rows| image.as_deref().and_then(|i| frame::rows(i, top, rows));
            self.image_layer.setContentsRect(WHOLE);
            self.bar_layer.setContentsRect(WHOLE);
            show(&self.image_layer, part(0, above as usize).as_deref());
            show(
                &self.bar_layer,
                part(above as usize, rows as usize)
                    .filter(|_| rows > 0)
                    .as_deref(),
            );
            self.surfaces.show(None);
        }
    }

    // Draws the frame into a surface the window server no longer uses and shows it there;
    // false, having shown nothing, where none is free or it cannot be drawn into.
    fn draw_surface(&mut self, (width, height): (u32, u32), rows: u32, scale: f64) -> bool {
        let above = height - rows;
        let Some(next) = self.surfaces.free((width, height)) else {
            return false;
        };
        // the last frame is kept only where it was copied in
        let kept = match self.surfaces.shown() {
            Some(shown) if self.session.would_move((width, height), scale) => {
                frame::copy(self.surfaces.get(shown), self.surfaces.get(next))
            }
            _ => false,
        };
        let session = &mut self.session;
        let mut drawn = false;
        let locked = frame::draw_into(self.surfaces.get(next), |pixels| {
            if let Some(mut canvas) = Canvas::new(pixels, width, height) {
                let frame = Frame {
                    kept,
                    ..Frame::default()
                };
                session.draw(&mut canvas, scale, frame);
                drawn = true;
            }
        });
        if !(locked && drawn) {
            return false;
        }
        if kept {
            MOVED_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        // the rows above the bar, and the bar's, of the surface, in the unit square of its
        // contents, which counts up from its last row
        let share = |rows: u32| f64::from(rows) / f64::from(height);
        let part = |top: u32, rows: u32| {
            let bottom = 1.0 - share(top + rows);
            CGRect::new(CGPoint::new(0.0, bottom), CGSize::new(1.0, share(rows)))
        };
        self.image_layer.setContentsRect(part(0, above));
        self.bar_layer.setContentsRect(part(above, rows));
        let surface = self.surfaces.get(next);
        // SAFETY: an IOSurface is a Core Foundation object, which is an Objective-C object,
        // the kind a layer's contents takes
        let contents = unsafe { &*(surface as *const _ as *const AnyObject) };
        // SAFETY: a layer's contents is an IOSurface, or an image
        unsafe {
            self.image_layer.setContents(Some(contents));
            self.bar_layer.setContents((rows > 0).then_some(contents));
        }
        self.surfaces.show(Some(next));
        true
    }

    // The frame that is due, if any.
    fn draw_due(&mut self) -> Vec<Request> {
        if let Some(bar_only) = self.due.take() {
            self.draw(bar_only);
        }
        Vec::new()
    }

    // Moves the timer to the session's deadline, where it changed.
    fn follow_deadline(&mut self) {
        let deadline = self.session.deadline();
        if deadline == self.deadline {
            return;
        }
        self.deadline = deadline;
        self.timer += 1;
        let Some(at) = deadline else {
            return;
        };
        let timer = self.timer;
        let wait = at.saturating_duration_since(Instant::now());
        // a deadline too far for the clock never fires, which only stops a slideshow
        if let Ok(when) = DispatchTime::try_from(wait) {
            let _ = DispatchQueue::main().after(when, move || {
                later(move |viewer| {
                    if viewer.timer != timer {
                        return Vec::new();
                    }
                    viewer.deadline = None;
                    viewer.session.timers(Instant::now())
                });
            });
        }
    }

    // The window sized to the image: inside the visible frame of
    // its screen, its title bar included, unless fullscreen.
    fn fit(&mut self, image: Size, bar: f64) {
        if self.fullscreen() {
            return;
        }
        let Some(visible) = self.visible() else {
            return;
        };
        let chrome = self.chrome();
        let room = Size {
            width: visible.width - chrome.width,
            height: visible.height - chrome.height,
        };
        if let Some(content) = fit(image, bar, room) {
            self.resize(content, visible);
        }
    }

    // The window's size from the config, for the grid, unless -g fixed it or fullscreen.
    fn reset(&mut self) {
        let Some(size) = self.reset_size.filter(|_| !self.fullscreen()) else {
            return;
        };
        if let Some(visible) = self.visible() {
            self.resize(size, visible);
        }
    }

    // The window to `content`, its center kept, inside `visible`.
    fn resize(&mut self, content: Size, visible: Rect) {
        let chrome = self.chrome();
        let size = Size {
            width: content.width + chrome.width,
            height: content.height + chrome.height,
        };
        let old = self.window.frame();
        let frame = placed(from_rect(old), size, visible);
        self.window.setFrame_display(rect(frame), true);
        if from_rect(self.window.frame()) != from_rect(old) {
            lay_out();
        }
    }

    fn fullscreen(&self) -> bool {
        self.fullscreen
            || self
                .window
                .styleMask()
                .contains(NSWindowStyleMask::FullScreen)
    }

    // The colors for the window's appearance now: those the config file set, and the system's
    // for the rest, so the window follows the appearance, in the frame's sRGB; one of the
    // system's that is not opaque laid over the background.
    fn palette(&self) -> Palette {
        let (set, configured) = (self.colors_set, self.configured);
        let palette = Cell::new(configured);
        let block = StackBlock::new(|| {
            let pick = |own: bool, configured: [u8; 3], system: &dyn Fn() -> [u8; 3]| {
                if own { configured } else { system() }
            };
            let background = pick(set.background, configured.background, &|| {
                srgb(&NSColor::windowBackgroundColor(), None)
            });
            let over = |color: Retained<NSColor>| srgb(&color, Some(background));
            palette.set(Palette {
                background,
                foreground: pick(set.foreground, configured.foreground, &|| {
                    over(NSColor::labelColor())
                }),
                bar_background: pick(set.bar_background, configured.bar_background, &|| {
                    over(NSColor::controlBackgroundColor())
                }),
                selection: pick(set.selection_color, configured.selection, &|| {
                    over(NSColor::controlAccentColor())
                }),
                mark: pick(set.mark_color, configured.mark, &|| {
                    over(NSColor::systemYellowColor())
                }),
                // the system's faint label color, where the Linux frontends take the bar's
                placeholder: pick(set.bar_background, configured.placeholder, &|| {
                    over(NSColor::quaternaryLabelColor())
                }),
                // and its secondary label color for what went wrong, where they take the text's
                error: pick(set.foreground, configured.error, &|| {
                    over(NSColor::secondaryLabelColor())
                }),
            });
        });
        self.view
            .effectiveAppearance()
            .performAsCurrentDrawingAppearance(&block);
        palette.get()
    }

    // The colors for the appearance now, and a whole frame in them.
    fn follow_appearance(&mut self) -> Vec<Request> {
        let palette = self.palette();
        self.session.set_palette(palette)
    }

    // The frame a user's resize to `proposed` gives: the image's shape kept where the session
    // shows an image with a fitted size and the window is not fullscreen, else as proposed.
    fn will_resize(&self, proposed: NSSize) -> NSSize {
        let Some(image) = self.session.viewer().fitted_size() else {
            return proposed;
        };
        if self.fullscreen() {
            return proposed;
        }
        let chrome = self.chrome();
        let content = |frame: NSSize| Size {
            width: frame.width - chrome.width,
            height: frame.height - chrome.height,
        };
        let old = content(self.window.frame().size);
        let kept = kept_shape(image, self.session.bar(), old, content(proposed));
        NSSize::new(kept.width + chrome.width, kept.height + chrome.height)
    }

    // The frame the zoom button gives in place of AppKit's `default`: the largest of the
    // image's shape inside it, where the session shows an image with a fitted size and the
    // window is not fullscreen. So the zoom's second press finds the window at the frame it
    // gave, and goes back to the one before.
    fn standard_frame(&self, default: NSRect) -> NSRect {
        let Some(image) = self.session.viewer().fitted_size() else {
            return default;
        };
        if self.fullscreen() {
            return default;
        }
        zoomed(image, self.session.bar(), self.chrome(), from_rect(default)).map_or(default, rect)
    }

    // Out of fullscreen, or a try at it that failed, the size for what the session shows
    // now: the grid's, or the image's, or none.
    fn left_fullscreen(&mut self) -> Vec<Request> {
        self.fullscreen = false;
        self.strip.fullscreen(false);
        if self.session.viewer().thumbnail_mode() {
            self.reset();
        } else if let Some(image) = self.session.viewer().fitted_size() {
            // the viewer's own bar, not an edit's status line, which leaving the edit takes
            // away without a fit
            let bar = if self.session.viewer().bar_visible() {
                self.session.bar()
            } else {
                0.0
            };
            self.fit(image, bar);
        }
        Vec::new()
    }

    // The visible frame of the window's screen, or before it is on one, the main screen's.
    fn visible(&self) -> Option<Rect> {
        let mtm = MainThreadMarker::from(&*self.window);
        let screen = self.window.screen().or_else(|| NSScreen::mainScreen(mtm))?;
        Some(from_rect(screen.visibleFrame()))
    }

    // What the window's frame adds to its content: the title bar.
    fn chrome(&self) -> Size {
        let content = NSRect::new(NSPoint::ZERO, NSSize::new(100.0, 100.0));
        let frame = self.window.frameRectForContentRect(content);
        Size {
            width: frame.size.width - 100.0,
            height: frame.size.height - 100.0,
        }
    }

    // Ends the viewer as the q key does, so -o still prints, a C-x prefix ended first, which
    // would give the q to the key-handler, and an edit dropped, as Command-q drops it.
    fn quit(&mut self) -> Vec<Request> {
        if let Some(field) = self.field.take() {
            field.removeFromSuperview();
        }
        let _ = self.session.end_edit();
        self.session.end_prefix();
        self.session.key(&Keystroke::new("q")).unwrap_or_default()
    }

    // An edit's text field over `rect`, in the canvas's points from its top left: bold system
    // font, the text's color over black at 35%, bordered, no focus
    // ring, and the keys its own until Return or Escape.
    fn open_field(&mut self, rect: Rect, text: &str, font: f64, [r, g, b]: [u8; 3]) {
        if let Some(old) = self.field.take() {
            old.removeFromSuperview();
        }
        let mtm = MainThreadMarker::from(&*self.window);
        let height = self.view.bounds().size.height;
        let origin = NSPoint::new(rect.x, height - rect.y - rect.height);
        let frame = NSRect::new(origin, NSSize::new(rect.width, rect.height));
        let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
        field.setStringValue(&NSString::from_str(text));
        field.setFont(Some(&NSFont::boldSystemFontOfSize(font)));
        let channel = |c: u8| f64::from(c) / 255.0;
        let color =
            NSColor::colorWithSRGBRed_green_blue_alpha(channel(r), channel(g), channel(b), 1.0);
        field.setTextColor(Some(&color));
        field.setBackgroundColor(Some(&NSColor::blackColor().colorWithAlphaComponent(0.35)));
        field.setDrawsBackground(true);
        field.setBordered(true);
        field.setFocusRingType(NSFocusRingType::None);
        // SAFETY: the target lives as long as the viewer, and takes the action's selector
        unsafe {
            field.setTarget(Some(&self.field_target));
            field.setAction(Some(sel!(ended:)));
        }
        self.fields += 1;
        field.setTag(self.fields);
        self.view.addSubview(&field);
        self.window.makeFirstResponder(Some(&field));
        self.field = Some(field);
    }

    // The end of the text field numbered `tag`, by its Return, which comes queued: nothing
    // where that field has gone since, and another may be open.
    fn field_returned(&mut self, tag: isize) -> Vec<Request> {
        match &self.field {
            Some(field) if field.tag() == tag => self.end_field(),
            _ => Vec::new(),
        }
    }

    // The text field's end: its text to the session, which goes on with a press it held.
    fn end_field(&mut self) -> Vec<Request> {
        let Some(field) = self.field.take() else {
            return Vec::new();
        };
        let text = field.stringValue().to_string();
        field.removeFromSuperview();
        self.window.makeFirstResponder(None);
        self.session.text_entered(text)
    }
}

// `color` in the frame's sRGB bytes, one that is not opaque laid over `under` first; the
// color as opaque where there is nothing under it.
fn srgb(color: &NSColor, under: Option<[u8; 3]>) -> [u8; 3] {
    let Some(color) = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()) else {
        return under.unwrap_or_default();
    };
    let alpha = under.map_or(1.0, |_| color.alphaComponent());
    let under = under.unwrap_or_default();
    let channel = |value: f64, under: u8| {
        let mixed = value * alpha + f64::from(under) / 255.0 * (1.0 - alpha);
        (mixed * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [
        channel(color.redComponent(), under[0]),
        channel(color.greenComponent(), under[1]),
        channel(color.blueComponent(), under[2]),
    ]
}

// Prints the paths to stdout, a line each, and ends the process with status 0.
fn quit(paths: &[PathBuf]) -> ! {
    let mut stdout = io::stdout().lock();
    for path in paths {
        let _ = stdout.write_all(path.as_os_str().as_encoded_bytes());
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
    std::process::exit(0)
}

// Gives `layer` the image to show, or none.
fn show(layer: &CALayer, image: Option<&objc2_core_graphics::CGImage>) {
    // SAFETY: a CGImage is a Core Foundation object, which is an Objective-C object, the kind
    // a layer's contents takes
    let contents = image.map(|image| unsafe { &*(image as *const _ as *const AnyObject) });
    // SAFETY: a layer's contents is a CGImage, or nothing
    unsafe { layer.setContents(contents) };
}

/// The unit square of a layer's contents: all of them.
const WHOLE: CGRect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1.0, 1.0));

fn rect(rect: Rect) -> CGRect {
    CGRect::new(
        CGPoint::new(rect.x, rect.y),
        CGSize::new(rect.width, rect.height),
    )
}

fn from_rect(rect: CGRect) -> Rect {
    Rect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

// A key the window's own events give; true where the viewer
// took it. A key with Command goes on to the menu, and it and a key without a name end a C-x
// prefix; a repeat that waited in the queue is dropped, as a held key outruns large decodes.
// While an edit's text field is open it has every key but Escape, which ends it; in an edit,
// Command with z, Shift-z, s and c are the edit's undo, redo, save and copy.
fn key(event: &NSEvent) -> bool {
    let mut taken = false;
    let _ = with(|viewer| {
        let mtm = MainThreadMarker::from(&*viewer.window);
        if event.window(mtm).as_deref() != Some(&*viewer.window) {
            return Vec::new();
        }
        if viewer.field.is_some() {
            taken = event.keyCode() == ESCAPE;
            return match taken {
                true => viewer.end_field(),
                false => Vec::new(),
            };
        }
        let flags = event.modifierFlags();
        let characters = event.charactersIgnoringModifiers().map(|c| c.to_string());
        if flags.contains(NSEventModifierFlags::Command) && viewer.session.editing() {
            let shift = flags.contains(NSEventModifierFlags::Shift);
            let lower = characters.as_deref().map(str::to_lowercase);
            let edit_key = match lower.as_deref() {
                Some("z") if shift => Some("Z"),
                Some(name @ ("z" | "s" | "c")) => Some(name),
                _ => None,
            };
            if let Some(name) = edit_key {
                taken = true;
                let requests = viewer.session.key(&Keystroke::ctrl(name));
                return requests.unwrap_or_default();
            }
        }
        // an edit takes every key but Command's, bound or not, and neither beeps for one it has
        // no use for nor drops a repeat
        let editing = viewer.session.editing() && !flags.contains(NSEventModifierFlags::Command);
        let name = key_name(event.keyCode(), characters.as_deref().unwrap_or(""));
        let Some(name) = name.filter(|_| !flags.contains(NSEventModifierFlags::Command)) else {
            viewer.session.end_prefix();
            taken = editing;
            return Vec::new();
        };
        let waited = NSProcessInfo::processInfo().systemUptime() - event.timestamp();
        if event.isARepeat() && waited > 0.1 && !editing {
            taken = true;
            return Vec::new();
        }
        let ctrl = flags.contains(NSEventModifierFlags::Control);
        let requests = viewer.session.key(&Keystroke { key: name, ctrl });
        taken = requests.is_some() || editing;
        requests.unwrap_or_default()
    });
    taken
}

/// Escape's virtual key code.
const ESCAPE: u16 = 53;

/// Space's virtual key code.
const SPACE: u16 = 49;

// A key that came up: space's reaches the session, where an edit moves a shape while it is held.
fn key_up(event: &NSEvent) {
    if event.keyCode() == SPACE {
        let _ = with(|viewer| viewer.session.key_released(&Keystroke::new(" ")));
    }
}

// Puts on the general pasteboard what `put` puts, in place of what it held; `put` gets the
// pasteboard and its types for PNG and for a string. False where it
// failed, as where the session has no pasteboard, as one logged in over ssh alone.
fn copy(put: impl FnOnce(&NSPasteboard, (&NSString, &NSString)) -> bool) -> bool {
    // SAFETY: generalPasteboard takes nothing, and gives the pasteboard, or nil where the
    // session has no pasteboard server, which the generated binding would take for a pasteboard
    let board: Option<Retained<NSPasteboard>> =
        unsafe { msg_send![NSPasteboard::class(), generalPasteboard] };
    let Some(board) = board else {
        return false;
    };
    board.clearContents();
    // SAFETY: a constant string AppKit defines
    let string = unsafe { NSPasteboardTypeString };
    put(&board, (png_type(), string))
}

fn png_type() -> &'static NSString {
    // SAFETY: a constant string AppKit defines
    unsafe { NSPasteboardTypePNG }
}

// What a copy that failed asks: a warning and a beep.
fn copied(done: bool) -> Vec<Request> {
    match done {
        true => Vec::new(),
        false => vec![
            Request::Warning("the pasteboard cannot be reached".to_owned()),
            Request::Beep,
        ],
    }
}

// SIGINT, SIGTERM and SIGHUP end the viewer as q does, on the main queue. Each has a handler
// that does nothing, not an ignored disposition, which a child would keep across exec: the
// key-handler and the decoding helpers start with them at their default.
fn signals() -> Vec<DispatchRetained<DispatchSource>> {
    extern "C" fn nothing(_: libc::c_int) {}
    extern "C" fn caught(_: *mut c_void) {
        later(Viewer::quit);
    }
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
        .into_iter()
        .map(|signal| {
            // SAFETY: a zeroed sigaction is a valid one to fill in
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = nothing as extern "C" fn(libc::c_int) as usize;
            // so no call another thread is in fails with EINTR for it
            action.sa_flags = libc::SA_RESTART;
            // SAFETY: the action is filled in, and its handler is async-signal-safe
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
            // SAFETY: a signal source takes the signal's number, and no mask; by kqueue it
            // sees the signal whatever its disposition
            let source = unsafe {
                DispatchSource::new(
                    (&raw const dispatch2::_dispatch_source_type_signal).cast_mut(),
                    signal as usize,
                    0,
                    Some(DispatchQueue::main()),
                )
            };
            source.set_event_handler_f(caught);
            source.activate();
            source
        })
        .collect()
}

// One frame for the events that wait together: the frame due is drawn as the main run loop is
// about to wait, which it does only once the events and calls queued are through, as AppKit's
// own display of views that need it. In the run loop's common modes, which hold AppKit's
// default, modal panel and event tracking modes, so a live resize and a menu's tracking draw
// too; a mode outside them, which a caller runs the loop in for itself, draws nothing until
// the loop is back in one of them.
fn frames() -> Option<CFRetained<CFRunLoopObserver>> {
    unsafe extern "C-unwind" fn waiting(
        _: *mut CFRunLoopObserver,
        _: CFRunLoopActivity,
        _: *mut c_void,
    ) {
        // a viewer busy below a run loop it runs draws at the next wait
        let _ = with(Viewer::draw_due);
    }
    // SAFETY: the default allocator, a callout that takes the observer's arguments, and no
    // context, which it does not read
    let observer = unsafe {
        CFRunLoopObserver::new(
            None,
            CFRunLoopActivity::BeforeWaiting.0,
            true,
            0,
            Some(waiting),
            std::ptr::null_mut(),
        )
    }?;
    // SAFETY: a constant string Core Foundation defines
    let common = unsafe { kCFRunLoopCommonModes };
    CFRunLoop::main()?.add_observer(Some(&observer), common);
    Some(observer)
}

// The application's menu: About, and Quit on Cmd-Q; and Edit, for an edit's text field.
fn menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let item = |title: &str, action, key: &str| {
        // SAFETY: each action is a method of NSApplication's, or of the text field's, which a
        // menu item with no target sends to the first responder that takes it
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        }
    };
    let app_menu = NSMenu::new(mtm);
    let about = format!("About {APP_NAME}");
    app_menu.addItem(&item(&about, sel!(orderFrontStandardAboutPanel:), ""));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(&format!("Quit {APP_NAME}"), sel!(terminate:), "q"));
    let main = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    app_item.setSubmenu(Some(&app_menu));
    main.addItem(&app_item);
    // Edit, whose actions go to the first responder, an edit's text field while it is open:
    // without them the field takes no Command-x, Command-c, Command-v or Command-a. Outside
    // the field the window's key monitor takes edit mode's Command-c first
    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));
    edit_menu.addItem(&item("Cut", sel!(cut:), "x"));
    edit_menu.addItem(&item("Copy", sel!(copy:), "c"));
    edit_menu.addItem(&item("Paste", sel!(paste:), "v"));
    edit_menu.addItem(&item("Select All", sel!(selectAll:), "a"));
    let edit_item = NSMenuItem::new(mtm);
    edit_item.setSubmenu(Some(&edit_menu));
    main.addItem(&edit_item);
    main
}

// The window, not yet on screen, with `size` as its content, and its view, backed by a layer.
fn window(
    mtm: MainThreadMarker,
    size: Size,
    delegate: &Delegate,
) -> (Retained<NSWindow>, Retained<CanvasView>) {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let content = NSRect::new(NSPoint::ZERO, NSSize::new(size.width, size.height));
    // SAFETY: a new window, with a style and a backing AppKit takes
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the window is held here, so AppKit must not release it as it closes
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(APP_NAME));
    window.setTabbingMode(NSWindowTabbingMode::Disallowed);
    // the frame's own space, so Core Animation shows its image as it is, where it otherwise
    // converts each frame to the display's on the CPU, on the main thread, in its commit
    window.setColorSpace(Some(&NSColorSpace::sRGBColorSpace()));
    window.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    let view = CanvasView::new(mtm, content);
    view.setWantsLayer(true);
    window.setContentView(Some(&view));
    window.center();
    (window, view)
}

define_class!(
    /// What the application and its window tell the viewer.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NsxivNgDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _: &NSNotification) {
            let mtm = MainThreadMarker::from(self);
            #[allow(deprecated)] // activate() is macOS 14's, and the viewer runs on 13
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
            LAUNCHED.set(true);
            if WAITING.with(|cell| cell.borrow().is_none()) {
                later(Viewer::start);
                return;
            }
            // a viewer with no files to show by then shows the usage, as the command line's
            let when = DispatchTime::try_from(OPEN_WAIT).unwrap_or(DispatchTime::NOW);
            let _ = DispatchQueue::main().after(when, || {
                if WAITING.with(|cell| cell.borrow().is_some()) {
                    eprintln!("{}", options::usage());
                    std::process::exit(2);
                }
            });
        }

        // Files opened from Finder, or by `open`: at launch the files the viewer starts with,
        // as the command line's would be, and while it runs files after the current one,
        // which keeps its list and marks, as one window views them all.
        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _: &NSApplication, urls: &NSArray<NSURL>) {
            let files = opened(urls);
            if let Some(start) = WAITING.with(|cell| cell.borrow_mut().take()) {
                if files.is_empty() {
                    eprintln!("{APP_NAME}: no images to display");
                    std::process::exit(1);
                }
                open_viewer(MainThreadMarker::from(self), self, start, files);
                return;
            }
            if files.is_empty() {
                return;
            }
            let paths = files.into_iter().map(|file| file.path).collect();
            later(move |viewer| viewer.session.open_after_current(paths));
        }

        // Cmd-Q, and the system's request to quit for a logout, a restart or a shutdown, end
        // the viewer as q does, which prints the marks and exits within the call.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _: &NSApplication) -> NSApplicationTerminateReply {
            match with(Viewer::quit) {
                // a viewer that did not end, having ended already, lets the application end
                Ok(()) => NSApplicationTerminateReply::TerminateNow,
                // A viewer busy in a call below this one on the stack quits once that returns,
                // which a cancel lets happen; a logout asked for in that instant is cancelled.
                // An answer to end now would end with the marks unprinted. An answer to wait,
                // as seen on macOS 27.2: terminate: did not return, and neither a block queued
                // on the main queue before it nor one queued with a delay from inside this
                // method ran in the minute after. No call the viewer makes is known to run
                // the event loop under it, so this may be unreachable: toggleFullScreen:, as
                // seen there, returns after some 25 ms with windowWillEnterFullScreen: and one
                // windowDidResize: delivered inside it, and a block queued before it runs
                // after it.
                Err(quit) => {
                    later(quit);
                    NSApplicationTerminateReply::TerminateCancel
                }
            }
        }
    }

    unsafe impl NSWindowDelegate for Delegate {
        // a closed window ends the viewer as q does
        #[unsafe(method(windowShouldClose:))]
        fn should_close(&self, _: &NSWindow) -> bool {
            later(Viewer::quit);
            false
        }

        // a user's resize, as AppKit calls it for no other
        #[unsafe(method(windowWillResize:toSize:))]
        fn will_resize(&self, _: &NSWindow, proposed: NSSize) -> NSSize {
            VIEWER.with(|cell| match cell.try_borrow().as_deref() {
                Ok(Some(viewer)) => viewer.will_resize(proposed),
                _ => proposed,
            })
        }

        #[unsafe(method(windowWillUseStandardFrame:defaultFrame:))]
        fn standard_frame(&self, _: &NSWindow, default: NSRect) -> NSRect {
            VIEWER.with(|cell| match cell.try_borrow().as_deref() {
                Ok(Some(viewer)) => viewer.standard_frame(default),
                _ => default,
            })
        }

        #[unsafe(method(windowDidResize:))]
        fn did_resize(&self, _: &NSNotification) {
            lay_out();
        }

        // a key that comes up while the window takes no keys never reaches it, so space is up
        // for the session as the window stops taking them, and a shape drawn stops moving
        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _: &NSNotification) {
            later(|viewer| viewer.session.key_released(&Keystroke::new(" ")));
        }

        // a move to a screen of another scale: the session gets the scale, and a frame for it
        #[unsafe(method(windowDidChangeBackingProperties:))]
        fn did_change_backing_properties(&self, _: &NSNotification) {
            lay_out();
        }

        #[unsafe(method(windowDidEnterFullScreen:))]
        fn did_enter_full_screen(&self, _: &NSNotification) {
            later(|viewer| {
                viewer.fullscreen = true;
                viewer.strip.fullscreen(true);
                Vec::new()
            });
        }

        #[unsafe(method(windowDidFailToEnterFullScreen:))]
        fn did_fail_to_enter_full_screen(&self, _: &NSWindow) {
            later(Viewer::left_fullscreen);
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn did_exit_full_screen(&self, _: &NSNotification) {
            later(Viewer::left_fullscreen);
        }
    }
);

define_class!(
    /// What an edit's text field tells as Return ends it.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NsxivNgFieldTarget"]
    struct FieldTarget;

    impl FieldTarget {
        #[unsafe(method(ended:))]
        fn ended(&self, sender: &AnyObject) {
            // SAFETY: an action's sender is the control that sent it, here a text field
            let tag: isize = unsafe { msg_send![sender, tag] };
            later(move |viewer| viewer.field_returned(tag));
        }
    }
);

impl FieldTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's init, on an object just allocated
        unsafe { msg_send![super(this), init] }
    }
}

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's init, on an object just allocated
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    /// The view of the session's frame, which takes the pointer.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NsxivNgCanvasView"]
    struct CanvasView;

    impl CanvasView {
        // AppKit's count of the clicks, at the user's own double-click speed
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let clicks = u32::try_from(event.clickCount()).ok();
            self.pointer(event, |session, at, time| {
                session.press_counted(Button::Left, at, time, clicks)
            });
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.pointer(event, |session, at, _| session.pointer_moved(at));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.pointer(event, |session, at, _| session.release(Button::Left, at));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.pointer(event, |session, at, time| session.press(Button::Right, at, time));
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.pointer(event, |session, at, _| session.release(Button::Right, at));
        }

        // A wheel's steps, one a notch, away from the user zooming in, and a trackpad's
        // precise deltas, which pan the image or scroll the grid: the image pans by (dx, -dy)
        // and the grid scrolls by -dy, as the fingers move the content, where the session's
        // distance pans by (-dx, dy) and scrolls by dy. A gesture's begin and end move nothing,
        // and would cost the grid a frame each. With Control or Option, either zooms the image
        // about the pointer, by a share of the delta, never below 0.2 a step; the grid does not
        // zoom.
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let (dx, dy) = (event.scrollingDeltaX(), event.scrollingDeltaY());
            let precise = event.hasPreciseScrollingDeltas();
            let keys = NSEventModifierFlags::Control | NSEventModifierFlags::Option;
            let zoom = event.modifierFlags().intersects(keys);
            self.pointer(event, |session, at, _| {
                if zoom && !session.viewer().thumbnail_mode() {
                    if dy == 0.0 {
                        return Vec::new();
                    }
                    let share = if precise { 0.01 } else { 0.1 };
                    return session.zoom_at((1.0 + dy * share).max(0.2), at);
                }
                let scroll = if precise {
                    if dx == 0.0 && dy == 0.0 {
                        return Vec::new();
                    }
                    Scroll::Distance { dx: -dx, dy: -dy }
                } else if dy != 0.0 {
                    Scroll::Steps(if dy > 0.0 { -1 } else { 1 })
                } else {
                    return Vec::new();
                };
                session.scroll(scroll)
            });
        }

        // the system's appearance changed, or the application's: the colors follow it
        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn did_change_appearance(&self) {
            later(Viewer::follow_appearance);
        }

        // a trackpad's pinch zooms about the pointer
        #[unsafe(method(magnifyWithEvent:))]
        fn magnify(&self, event: &NSEvent) {
            let factor = 1.0 + event.magnification();
            self.pointer(event, |session, at, _| session.zoom_at(factor, at));
        }
    }
);

impl CanvasView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSView's designated initializer, on a view just allocated
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    // A pointer event in the session's points, from the top left of the frame, y down, at
    // the event's time in milliseconds, which wrap.
    fn pointer(
        &self,
        event: &NSEvent,
        call: impl FnOnce(&mut Session, Point, u32) -> Vec<Request>,
    ) {
        let at = self.convertPoint_fromView(event.locationInWindow(), None);
        let at = flip(Point { x: at.x, y: at.y }, self.bounds().size.height);
        let time = (event.timestamp() * 1000.0) as u64 as u32;
        let _ = with(|viewer| call(&mut viewer.session, at, time));
    }
}
