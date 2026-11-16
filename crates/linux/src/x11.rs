//! The X11 frontend: one window on x11rb's own connection, which the session draws into
//! through shared memory, with keys from the server's core keyboard mapping.
//!
//! The window takes the image's size within its monitor, and is mapped once its first frame is
//! final, so the window manager places it by the size it keeps. Each frame is drawn straight
//! into a MIT-SHM segment and put to the window, or sent in plain PutImage requests where the
//! server cannot share memory, as over the network; an expose puts the frame kept again.
//! Redraws that come while the queued events are handled, or while the server still reads the
//! last frame, give one frame once that is done.

mod clipboard;
mod frame;
mod keymap;
mod size;

use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::os::fd::AsFd;
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::ping::{Ping, make_ping};
use calloop::{Interest, Mode, PostAction};
use nsxiv_ng_core::config::{APP_NAME, Config};
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::Options;
use nsxiv_ng_core::viewer::{Keystroke, Viewer};
use nsxiv_ng_core::{Point, Rect, Size};
use nsxiv_ng_render::Canvas;
use nsxiv_ng_session::{Bar, Button, Frame, Request, Scroll, Session, key_handlers};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _,
    CreateGCAux, CreateWindowAux, EventMask, ImageFormat, KeyButMask, KeyReleaseEvent, Mapping,
    MotionNotifyEvent, PropMode, Visualid, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::X11Error;
use xkbcommon::xkb::Keysym;

use crate::driver::{self, Driver, Frontend, Handle};
use crate::keysyms;
use clipboard::Clipboard;
use frame::Segment;
use keymap::Keymap;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _NET_WM_STATE,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_NAME,
        _NET_REQUEST_FRAME_EXTENTS,
        _NET_FRAME_EXTENTS,
        UTF8_STRING,
        CLIPBOARD,
        TARGETS,
        INCR,
        IMAGE_PNG: b"image/png",
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
    }
}

/// The largest side of a window the core protocol takes.
const MAX_SIDE: u32 = 32767;

/// The connection to the X server, before the viewer's window is on it.
pub struct X11 {
    conn: RustConnection,
    screen: usize,
    visual: Visualid,
    depth: u8,
}

/// The X server `DISPLAY` names, None without one, or with a warning where it has no visual
/// that shows the canvas's pixels as they are.
pub fn open() -> Option<X11> {
    let (conn, screen) = RustConnection::connect(None).ok()?;
    let setup = conn.setup();
    let Some((visual, depth)) = frame::visual(setup, setup.roots.get(screen)?) else {
        eprintln!(
            "{APP_NAME}: the X server has no TrueColor visual of depth 24 or 32 with 8-bit red, \
             green and blue in this machine's byte order"
        );
        return None;
    };
    Some(X11 {
        conn,
        screen,
        visual,
        depth,
    })
}

impl X11 {
    /// Runs the viewer of `files` in a window until it quits, then exits the process with
    /// status 0. `program` is the viewer's own binary. Returns only an error that keeps the
    /// viewer from running.
    pub fn run(
        self,
        options: Options,
        files: Vec<FileEntry>,
        config: &Config,
        program: PathBuf,
    ) -> io::Error {
        let configured = Size {
            width: config.window_width,
            height: config.window_height,
        };
        let size = options.geometry.unwrap_or(configured);
        // -g fixes the size, which the grid then keeps
        let reset_size = options.geometry.is_none().then_some(configured);
        driver::run(
            |handle| X11Window::new(self, handle, size, reset_size).map_err(io::Error::other),
            |_, wake| {
                let handlers = key_handlers();
                let mut session =
                    Session::new(options, files, config, program, handlers, Bar::Drawn, wake);
                session.can_edit(driver::edit_setup());
                session
            },
        )
    }
}

/// A side in logical units as the window's pixels, at scale 1.
fn side(length: f64) -> u16 {
    length.round().clamp(1.0, f64::from(MAX_SIDE)) as u16
}

/// The X11 frontend, in the driver's loop.
pub struct X11Window {
    conn: RustConnection,
    root: u32,
    /// The whole screen, for a server without RandR's monitors.
    screen_size: Size,
    window: u32,
    gc: u32,
    depth: u8,
    atoms: Atoms,
    keymap: Keymap,
    /// The window's size in pixels, as the server last gave it.
    size: (u16, u16),
    /// The size ResetWindowSize gives the window, None where -g fixed it.
    reset_size: Option<Size>,
    /// The frame's pixels: in the segment, or for a server without one in `plain`.
    shm: bool,
    segment: Option<Segment>,
    plain: Vec<u32>,
    /// The size of the frame kept in the pixels.
    kept: Option<(u16, u16)>,
    /// A frame too large for memory was refused, which is said once.
    refused: bool,
    /// The sequence of the put the server may still read the segment for.
    busy: Option<u16>,
    /// A redraw is due once the events are handled and the server read the last frame.
    dirty: bool,
    /// The same, for the bar alone, which a whole redraw covers.
    bar_dirty: bool,
    /// The queued events are being handled.
    draining: bool,
    queue: VecDeque<Event>,
    title: String,
    mapped: bool,
    /// The window manager placed the window, as its first MapNotify tells.
    placed: bool,
    fullscreen: bool,
    /// The last FitWindow asked for, done again on leaving fullscreen.
    fit: Option<(Size, f64)>,
    /// The room the last fit was for.
    fitted_for: Option<Size>,
    /// Whether frame extents told late fit the window again.
    frame_claim: size::FrameClaim,
    /// Calls the loop back to read the events a reply took in.
    after: Ping,
    /// The server sends a held key's release only when it comes up, not before each repeat.
    detectable: bool,
    /// CLIPBOARD, while the window owns it for an edit's copy.
    clipboard: Clipboard,
    /// The time of the last key or button pressed, which a copy owns CLIPBOARD from.
    time: u32,
}

impl X11Window {
    fn new(
        x11: X11,
        handle: &Handle<X11Window>,
        size: Size,
        reset_size: Option<Size>,
    ) -> Result<X11Window, Box<dyn Error + Send + Sync>> {
        let X11 {
            conn,
            screen,
            visual,
            depth,
        } = x11;
        let screen = &conn.setup().roots[screen];
        let (root, black) = (screen.root, screen.black_pixel);
        let screen_size = Size {
            width: f64::from(screen.width_in_pixels),
            height: f64::from(screen.height_in_pixels),
        };
        let colormap = if visual == screen.root_visual {
            screen.default_colormap
        } else {
            let colormap = conn.generate_id()?;
            conn.create_colormap(ColormapAlloc::NONE, colormap, root, visual)?;
            colormap
        };
        let window = conn.generate_id()?;
        let events = EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::EXPOSURE
            | EventMask::PROPERTY_CHANGE
            | EventMask::FOCUS_CHANGE;
        let attributes = CreateWindowAux::new()
            .background_pixel(black)
            .border_pixel(0)
            .colormap(colormap)
            .event_mask(events);
        let (width, height) = (side(size.width), side(size.height));
        conn.create_window(
            depth,
            window,
            root,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &attributes,
        )?;
        let atoms = Atoms::new(&conn)?.reply()?;
        let protocols = [atoms.WM_DELETE_WINDOW];
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.WM_PROTOCOLS,
            AtomEnum::ATOM,
            &protocols,
        )?;
        let class = format!("{APP_NAME}\0{APP_NAME}\0");
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            class.as_bytes(),
        )?;
        // the window manager sets _NET_FRAME_EXTENTS on the window before it maps, so the first
        // fit leaves room for the frame; with none, nothing answers
        let ask = ClientMessageEvent::new(32, window, atoms._NET_REQUEST_FRAME_EXTENTS, [0; 5]);
        let mask = EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT;
        conn.send_event(false, root, mask, ask)?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, window, &CreateGCAux::new())?;
        // auto-repeat then sends presses alone, and the release comes when the key does
        // shortcut: a server without XKB sends a release before each repeat, so drop_repeats
        // keeps the presses, and a release a press at its time follows is dropped as repeated
        // says; X.org, Xvfb and XWayland have it
        let repeat = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
        let mut detectable = false;
        if conn
            .xkb_use_extension(1, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.supported)
        {
            let none = 0u32.into();
            let flags = conn.xkb_per_client_flags(
                xkb::ID::USE_CORE_KBD.into(),
                repeat,
                repeat,
                none,
                none,
                none,
            );
            let flags = flags.ok().and_then(|cookie| cookie.reply().ok());
            detectable = flags.is_some_and(|reply| reply.value.contains(repeat));
        }
        let keymap = read_keymap(&conn)?;
        // segments made by the server and passed as descriptors came with version 1.2
        let shm = conn
            .shm_query_version()
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2));
        // the monitors came with version 1.5, which a server answers only once asked for
        let _ = conn.randr_query_version(1, 5).map(|cookie| cookie.reply());
        let fd = conn.stream().as_fd().try_clone_to_owned()?;
        let readable = Generic::new(fd, Interest::READ, Mode::Level);
        driver::insert(handle, readable, |_, _, driver| {
            dispatch(driver)?;
            Ok(PostAction::Continue)
        })?;
        let (after, pinged) = make_ping()?;
        // a connection lost here is found again when its descriptor reads at its end
        driver::insert(handle, pinged, |_, _, driver| {
            let _ = dispatch(driver);
        })?;
        // events the replies above took in are handled once the loop runs
        after.ping();
        Ok(X11Window {
            conn,
            root,
            screen_size,
            window,
            gc,
            depth,
            atoms,
            keymap,
            size: (width, height),
            reset_size,
            shm,
            segment: None,
            plain: Vec::new(),
            kept: None,
            refused: false,
            busy: None,
            dirty: false,
            bar_dirty: false,
            draining: false,
            queue: VecDeque::new(),
            title: String::new(),
            mapped: false,
            placed: false,
            fullscreen: false,
            fit: None,
            fitted_for: None,
            frame_claim: size::FrameClaim::default(),
            after,
            detectable,
            clipboard: Clipboard::default(),
            time: x11rb::CURRENT_TIME,
        })
    }

    /// Moves the events the connection has into the queue.
    fn fill(&mut self) -> Result<(), ConnectionError> {
        while let Some(event) = self.conn.poll_for_event()? {
            self.queue.push_back(event);
        }
        Ok(())
    }

    /// The current file's name as the window's title, where it changed.
    fn name(&mut self, viewer: &Viewer) {
        let Some(name) = viewer.current_entry().and_then(|e| e.path.file_name()) else {
            return;
        };
        let name = name.to_string_lossy();
        if name == self.title {
            return;
        }
        self.title = name.into_owned();
        let (window, title) = (self.window, self.title.as_bytes());
        let (net_name, utf8) = (self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING);
        let _ = self
            .conn
            .change_property8(PropMode::REPLACE, window, net_name, utf8, title);
        // for window managers without EWMH
        let (name, string) = (AtomEnum::WM_NAME, AtomEnum::STRING);
        let _ = self
            .conn
            .change_property8(PropMode::REPLACE, window, name, string, title);
    }

    /// Draws the frame into the segment, or the plain pixels, and puts it to the window.
    fn draw(&mut self, session: &mut Session) {
        self.bar_dirty = false;
        let (width, height) = (u32::from(self.size.0), u32::from(self.size.1));
        let Some((count, bytes)) = frame::frame_size(self.size.0, self.size.1, isize::MAX as usize)
        else {
            if !self.refused {
                self.refused = true;
                eprintln!("{APP_NAME}: a frame of {width}x{height} pixels is too large to draw");
            }
            return;
        };
        // the frame drawn last stays in the one segment, or the plain buffer, while neither is
        // made anew and the window keeps its size
        let mut kept = self.kept == Some(self.size);
        if self.shm && self.segment.as_ref().is_none_or(|s| s.len() < bytes) {
            if let Some(old) = self.segment.take() {
                let _ = self.conn.shm_detach(old.id);
            }
            self.segment = Segment::new(&self.conn, bytes);
            // a server that makes none, or cannot pass it, gets plain images from now on
            self.shm = self.segment.is_some();
            kept = false;
        }
        let pixels = match self.segment.as_mut() {
            Some(segment) => segment.pixels(count),
            None => {
                self.plain.resize(count, 0);
                &mut self.plain[..]
            }
        };
        let Some(mut canvas) = Canvas::new(pixels, width, height) else {
            return;
        };
        let frame = Frame {
            kept,
            ..Frame::default()
        };
        session.draw(&mut canvas, 1.0, frame);
        self.kept = Some(self.size);
        self.put();
    }

    /// Draws the bar alone over the frame kept, which is the window's size, and puts its rows
    /// to the window: the rest of the frame shows what it showed.
    fn draw_bar(&mut self, session: &mut Session) {
        let (width, height) = self.size;
        let rows = session.bar_rows(u32::from(height), 1.0) as u16;
        if rows == 0 {
            return;
        }
        let count = usize::from(width) * usize::from(height);
        let pixels = match self.segment.as_mut() {
            Some(segment) => segment.pixels(count),
            None => &mut self.plain[..count],
        };
        let top = height - rows;
        // a row past what a put's 16-bit destination reaches, in a window the viewer never asks
        // for but a server may give, goes with the whole frame
        if i16::try_from(top).is_err() {
            self.draw(session);
            return;
        }
        let below = &mut pixels[usize::from(top) * usize::from(width)..];
        let Some(mut canvas) = Canvas::new(below, u32::from(width), u32::from(rows)) else {
            return;
        };
        session.draw_bar(&mut canvas, 1.0);
        self.put_rows(top, rows);
    }

    /// Puts the frame kept to the window.
    fn put(&mut self) {
        if let Some((_, height)) = self.kept {
            self.put_rows(0, height);
        }
    }

    /// Puts `rows` rows of the frame kept, from row `top`, to the window.
    fn put_rows(&mut self, top: u16, rows: u16) {
        let Some((width, height)) = self.kept else {
            return;
        };
        let (window, gc, depth) = (self.window, self.gc, self.depth);
        if let Some(segment) = &self.segment {
            let format = ImageFormat::Z_PIXMAP.into();
            let put = self.conn.shm_put_image(
                window, gc, width, height, 0, top, width, rows, 0, top as i16, depth, format, true,
                segment.id, 0,
            );
            // the server tells when it has read the segment
            self.busy = put.ok().map(|cookie| cookie.sequence_number() as u16);
        } else {
            let row = usize::from(width);
            let max = self.conn.maximum_request_bytes();
            for (y, rows) in frame::pieces(width, rows, max) {
                let y = y + top;
                let start = usize::from(y) * row;
                let piece = &self.plain[start..start + usize::from(rows) * row];
                let bytes: Vec<u8> = piece.iter().flat_map(|p| p.to_ne_bytes()).collect();
                let _ = self.conn.put_image(
                    ImageFormat::Z_PIXMAP,
                    window,
                    gc,
                    width,
                    rows,
                    0,
                    y as i16,
                    0,
                    depth,
                    &bytes,
                );
            }
        }
        let _ = self.conn.flush();
    }

    /// The monitor the window is on, through RandR, else the whole screen; before the window
    /// manager placed it, the pointer's, where most window managers open a new window.
    fn monitor(&self) -> Size {
        let at = if self.placed {
            let at = self
                .conn
                .translate_coordinates(self.window, self.root, 0, 0);
            let at = at.ok().and_then(|cookie| cookie.reply().ok());
            at.map(|at| (at.dst_x, at.dst_y))
        } else {
            let at = self.conn.query_pointer(self.root);
            let at = at.ok().and_then(|cookie| cookie.reply().ok());
            at.map(|at| (at.root_x, at.root_y))
        };
        let at = at.unwrap_or((0, 0));
        let monitors: Vec<Rect> = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.monitors)
            .unwrap_or_default()
            .iter()
            .map(|m| Rect {
                x: f64::from(m.x),
                y: f64::from(m.y),
                width: f64::from(m.width),
                height: f64::from(m.height),
            })
            .collect();
        // the pointer is a point, a window its rectangle
        let (width, height) = if self.placed { self.size } else { (0, 0) };
        let window = Rect {
            x: f64::from(at.0),
            y: f64::from(at.1),
            width: f64::from(width),
            height: f64::from(height),
        };
        size::monitor(window, &monitors).map_or(self.screen_size, |m| Size {
            width: m.width,
            height: m.height,
        })
    }

    /// The room the window has on its monitor, less the window manager's frame around it.
    fn room(&self) -> Size {
        size::room(self.monitor(), &self.extents())
    }

    /// The window manager's frame around the window, as _NET_FRAME_EXTENTS tells it: left,
    /// right, top and bottom, or nothing before it is told.
    fn extents(&self) -> Vec<u32> {
        let extents = self
            .conn
            .get_property(
                false,
                self.window,
                self.atoms._NET_FRAME_EXTENTS,
                AtomEnum::CARDINAL,
                0,
                4,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        extents
            .and_then(|reply| reply.value32().map(Iterator::collect))
            .unwrap_or_default()
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.min(MAX_SIDE), height.min(MAX_SIDE));
        self.frame_claim.asked((width as u16, height as u16));
        let aux = ConfigureWindowAux::new().width(width).height(height);
        let _ = self.conn.configure_window(self.window, &aux);
    }

    /// The window to the image of the last FitWindow, unless it is fullscreen, with the
    /// image's aspect as a hint for the window manager.
    fn fit_window(&mut self) {
        let Some((image, bar)) = self.fit.filter(|_| !self.fullscreen) else {
            return;
        };
        let bar = bar.round().max(0.0);
        let extents = self.extents();
        let room = size::room(self.monitor(), &extents);
        self.fitted_for = Some(room);
        self.frame_claim.fitted(&extents);
        let Some((width, height)) = size::fit(image, bar, room) else {
            return;
        };
        let hints = size::size_hints(Some((width, height)), bar as u32);
        let _ = hints.set_normal_hints(&self.conn, self.window);
        self.resize(width, height + bar as u32);
    }

    fn copy(&mut self, data: Vec<u8>, image: bool) {
        let (window, time) = (self.window, self.time);
        let owned = (self.clipboard).own(&self.conn, &self.atoms, window, time, data, image);
        match owned {
            Ok(true) => {}
            Ok(false) => eprintln!("{APP_NAME}: cannot take the clipboard: another client has it"),
            Err(e) => eprintln!("{APP_NAME}: cannot take the clipboard: {e}"),
        }
    }

    fn toggle_fullscreen(&mut self) {
        let (state, fullscreen) = (
            self.atoms._NET_WM_STATE,
            self.atoms._NET_WM_STATE_FULLSCREEN,
        );
        if !self.mapped {
            // an unmapped window states it; the window manager reads that when it maps it
            let atoms = [fullscreen];
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                self.window,
                state,
                AtomEnum::ATOM,
                &atoms,
            );
            return;
        }
        // _NET_WM_STATE_TOGGLE from an application; the window manager answers with a
        // change of the property, which fullscreen_changed follows
        let toggle = ClientMessageEvent::new(32, self.window, state, [2, fullscreen, 0, 1, 0]);
        let mask = EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT;
        let _ = self.conn.send_event(false, self.root, mask, toggle);
    }

    /// Follows the window manager's state, which its own fullscreen shortcut changes too; the
    /// fit is done again on leaving it.
    fn fullscreen_changed(&mut self) {
        let (state, fullscreen) = (
            self.atoms._NET_WM_STATE,
            self.atoms._NET_WM_STATE_FULLSCREEN,
        );
        let Some(reply) = self
            .conn
            .get_property(false, self.window, state, AtomEnum::ATOM, 0, 64)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return;
        };
        let now = reply
            .value32()
            .is_some_and(|mut atoms| atoms.any(|atom| atom == fullscreen));
        if now != self.fullscreen {
            self.fullscreen = now;
            self.fit_window();
        }
    }
}

impl Frontend for X11Window {
    fn window(&self) -> (Size, f64) {
        let (width, height) = self.size;
        let size = Size {
            width: f64::from(width),
            height: f64::from(height),
        };
        (size, 1.0)
    }

    fn request(&mut self, session: &mut Session, request: Request) {
        match request {
            Request::Redraw { bar_only } => {
                self.name(session.viewer());
                // a frame of the bar alone needs the rest of the frame kept as it shows
                let bar_only = bar_only && self.kept == Some(self.size);
                if !self.mapped {
                    // the window maps once its first frame is final, so the window manager
                    // first sees it at the size it keeps, with fullscreen stated where -f asked,
                    // once the viewer has its first image; nothing is put before, and the expose
                    // the map brings asks for the frame
                    if session.frame_is_final() {
                        // a frame the window manager told of after the fit takes its room
                        if self.fitted_for.is_some_and(|room| room != self.room()) {
                            self.fit_window();
                        }
                        self.mapped = true;
                        let _ = self.conn.map_window(self.window);
                    }
                } else if self.draining || self.busy.is_some() {
                    if bar_only {
                        self.bar_dirty = true;
                    } else {
                        self.dirty = true;
                    }
                } else if bar_only {
                    self.draw_bar(session);
                } else {
                    self.draw(session);
                }
            }
            Request::FitWindow { image, bar } => {
                self.fit = Some((image, bar));
                self.fit_window();
            }
            Request::ResetWindowSize => {
                self.fit = None;
                let _ = size::size_hints(None, 0).set_normal_hints(&self.conn, self.window);
                if let Some(size) = self.reset_size.filter(|_| !self.fullscreen) {
                    self.resize(side(size.width).into(), side(size.height).into());
                }
            }
            Request::ToggleFullscreen => self.toggle_fullscreen(),
            Request::Beep => {
                let _ = self.conn.bell(0);
            }
            // an edit's copy, which CLIPBOARD holds until another client takes it or the
            // viewer quits
            Request::CopyImage(png) => self.copy(png, true),
            Request::CopyText(text) => self.copy(text.into_bytes(), false),
            _ => {}
        }
        // a reply may have taken events in, which the connection's descriptor no longer shows
        if !self.draining {
            self.after.ping();
        }
        let _ = self.conn.flush();
    }

    // the window stays while the key-handler runs, as nsxiv keeps its own
    fn release(&mut self) {}

    fn reclaim(&mut self) {}
}

fn read_keymap(conn: &RustConnection) -> Result<Keymap, ReplyError> {
    let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    let modifiers = conn.get_modifier_mapping()?.reply()?;
    let (per, keysyms) = (map.keysyms_per_keycode, map.keysyms);
    Ok(Keymap::new(min, per, keysyms, &modifiers.keycodes))
}

// Handles every event queued, those that come meanwhile too, and draws the one frame they
// asked for; an error where the connection broke, which ends the loop.
fn dispatch(driver: &mut Driver<X11Window>) -> io::Result<()> {
    let x = driver.frontend();
    x.draining = true;
    let lost = loop {
        let x = driver.frontend();
        if let Err(e) = x.fill() {
            break Some(e);
        }
        match x.queue.pop_front() {
            Some(event) => handle(driver, event),
            None => break None,
        }
    };
    let x = driver.frontend();
    x.draining = false;
    if let Some(e) = lost {
        // the server went away, or closed the connection, with the window; as Xlib's handler
        // of a broken connection says, which ends a program with status 1
        let line = format!("lost the connection to the X server: {e}");
        return Err(io::Error::other(line));
    }
    if x.busy.is_none() {
        if x.dirty {
            x.dirty = false;
            driver.redraw();
        } else if x.bar_dirty {
            x.bar_dirty = false;
            driver.event(|_| vec![Request::Redraw { bar_only: true }]);
        }
    }
    let _ = driver.frontend().conn.flush();
    Ok(())
}

fn point(x: i16, y: i16) -> Point {
    Point {
        x: f64::from(x),
        y: f64::from(y),
    }
}

fn handle(driver: &mut Driver<X11Window>, event: Event) {
    let x = driver.frontend();
    match event {
        Event::KeyPress(key) => {
            x.time = key.time;
            let state = u16::from(key.state);
            let keysym = Keysym::new(x.keymap.keysym(key.detail, state));
            // the text line takes a key by the character it types, which X11 names otherwise
            // past ASCII, as é eacute
            let typed = keysyms::typed(keysym).filter(|_| driver.typing());
            if let Some(name) = typed.or_else(|| keysyms::key_name(keysym)) {
                let ctrl = state & u16::from(KeyButMask::CONTROL) != 0;
                driver.key(&Keystroke { key: name, ctrl });
            }
            let typing = driver.typing();
            let x = driver.frontend();
            let _ = x.fill();
            drop_repeats(&mut x.queue, key.detail, typing);
        }
        // the keys held come up elsewhere, and never here: space, held to move a shape being
        // drawn, is up for the session, so a drag after resizes it again
        Event::FocusOut(_) => {
            driver.event(|session| session.key_released(&Keystroke::new(" ")));
        }
        // space's release, which ends moving a shape being drawn in an edit
        Event::KeyRelease(key) => {
            // where the server sends a release before each repeat, a held key is still down
            if !x.detectable {
                let _ = x.fill();
                if repeated(&x.queue, &key) {
                    return;
                }
            }
            let keysym = Keysym::new(x.keymap.keysym(key.detail, u16::from(key.state)));
            if let Some(name) = keysyms::key_name(keysym) {
                driver.event(|session| session.key_released(&Keystroke::new(&name)));
            }
        }
        Event::ButtonPress(press) => {
            x.time = press.time;
            let (at, time) = (point(press.event_x, press.event_y), press.time);
            match press.detail {
                1 => driver.event(|session| session.press(Button::Left, at, time)),
                3 => driver.event(|session| session.press(Button::Right, at, time)),
                // a wheel's steps, button 5 toward the user
                4 => driver.event(|session| session.scroll(Scroll::Steps(-1))),
                5 => driver.event(|session| session.scroll(Scroll::Steps(1))),
                _ => {}
            }
        }
        Event::ButtonRelease(release) => {
            let at = point(release.event_x, release.event_y);
            match release.detail {
                1 => driver.event(|session| session.release(Button::Left, at)),
                3 => driver.event(|session| session.release(Button::Right, at)),
                _ => {}
            }
        }
        Event::MotionNotify(motion) => {
            let motion = latest_motion(&mut x.queue, motion);
            let at = point(motion.event_x, motion.event_y);
            driver.event(|session| session.pointer_moved(at));
        }
        Event::MapNotify(map) if map.window == x.window && !x.placed => {
            x.placed = true;
            // a window manager that placed it on a monitor of another size than the pointer's
            // gets it fitted again, once
            if x.fitted_for.is_some_and(|room| room != x.room()) {
                x.fit_window();
            }
        }
        Event::ConfigureNotify(configure) if configure.window == x.window => {
            let size = (configure.width, configure.height);
            x.frame_claim.configured(size, x.size);
            if size != x.size {
                x.size = size;
                driver.layout();
            }
        }
        Event::Expose(expose) if expose.window == x.window && expose.count == 0 => {
            // a frame to come covers it otherwise
            if !x.dirty {
                if x.kept == Some(x.size) {
                    x.put();
                } else {
                    driver.redraw();
                }
            }
        }
        Event::PropertyNotify(property)
            if property.window == x.window && property.atom == x.atoms._NET_WM_STATE =>
        {
            x.fullscreen_changed();
        }
        // a window manager that told its frame only after the map gets the window fitted to it,
        // once
        Event::PropertyNotify(property)
            if property.window == x.window && property.atom == x.atoms._NET_FRAME_EXTENTS =>
        {
            let extents = x.extents();
            if x.frame_claim.told(&extents, x.fit.is_some(), x.fullscreen) {
                x.fit_window();
            }
        }
        // a client reading the clipboard INCR deleted the last piece
        Event::PropertyNotify(property) if property.window != x.window => {
            let _ = x.clipboard.property(&x.conn, &property);
        }
        Event::SelectionRequest(request)
            if request.owner == x.window && request.selection == x.atoms.CLIPBOARD =>
        {
            let _ = x.clipboard.request(&x.conn, &x.atoms, &request);
        }
        Event::SelectionClear(clear) if clear.selection == x.atoms.CLIPBOARD => {
            x.clipboard.lost(clear.time);
        }
        // a client reading the clipboard went
        Event::DestroyNotify(destroyed) if destroyed.window != x.window => {
            x.clipboard.destroyed(destroyed.window);
        }
        Event::ClientMessage(message)
            if message.type_ == x.atoms.WM_PROTOCOLS
                && message.data.as_data32()[0] == x.atoms.WM_DELETE_WINDOW =>
        {
            driver.quit();
        }
        Event::MappingNotify(mapping) if mapping.request != Mapping::POINTER => {
            if let Ok(keymap) = read_keymap(&x.conn) {
                x.keymap = keymap;
            }
        }
        Event::ShmCompletion(done) if x.busy == Some(done.sequence) => x.busy = None,
        Event::Error(error) => {
            // a put that failed reads nothing more
            if x.busy == Some(error.sequence) {
                x.busy = None;
            }
            // a client reading the clipboard went before its window's end could be watched
            if error.error_kind == x11rb::protocol::ErrorKind::Window
                && x.clipboard.destroyed(error.bad_value)
            {
                return;
            }
            // the window is not the terminal, so this garbles nothing
            eprintln!("{APP_NAME}: {}", crate::printable(&error_line(&error)));
        }
        _ => {}
    }
}

/// An error the server sent, as Xlib names one.
fn error_line(error: &X11Error) -> String {
    let request = error.request_name.unwrap_or("request");
    format!(
        "X error {:?} in {request} ({}.{}), value {:#x}",
        error.error_kind, error.major_opcode, error.minor_opcode, error.bad_value
    )
}

/// Drops the presses of the key just handled that queued meanwhile, as a held key outruns
/// large decodes; past other events, but not past the key's release or a change of focus,
/// which may take the release elsewhere, so taps still count. While the session's text line
/// is open, `typing`, each press types or deletes a character, and all stay.
fn drop_repeats(queue: &mut VecDeque<Event>, keycode: u8, typing: bool) {
    if typing {
        return;
    }
    let mut ended = false;
    queue.retain(|event| match event {
        Event::FocusIn(_) | Event::FocusOut(_) => {
            ended = true;
            true
        }
        Event::KeyRelease(key) if key.detail == keycode => {
            ended = true;
            true
        }
        Event::KeyPress(key) => ended || key.detail != keycode,
        _ => true,
    });
}

/// Whether the release of a key is one before a repeat: the press of the same key at the same
/// time comes next, as a server without detectable auto-repeat sends them.
fn repeated(queue: &VecDeque<Event>, release: &KeyReleaseEvent) -> bool {
    matches!(queue.front(), Some(Event::KeyPress(press))
        if press.detail == release.detail && press.time == release.time)
}

/// The latest of a run of motions: the queued ones up to the next event of another kind.
fn latest_motion(queue: &mut VecDeque<Event>, mut motion: MotionNotifyEvent) -> MotionNotifyEvent {
    while let Some(Event::MotionNotify(next)) = queue.front() {
        motion = *next;
        queue.pop_front();
    }
    motion
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{FocusInEvent, KeyPressEvent};

    fn press(keycode: u8) -> Event {
        Event::KeyPress(KeyPressEvent {
            detail: keycode,
            ..KeyPressEvent::default()
        })
    }

    fn release(keycode: u8) -> Event {
        Event::KeyRelease(KeyPressEvent {
            detail: keycode,
            ..KeyPressEvent::default()
        })
    }

    fn motion(x: i16) -> Event {
        Event::MotionNotify(MotionNotifyEvent {
            event_x: x,
            ..MotionNotifyEvent::default()
        })
    }

    // each event as its kind and keycode, or its x for a motion
    fn kinds(queue: &VecDeque<Event>) -> Vec<String> {
        queue
            .iter()
            .map(|event| match event {
                Event::KeyPress(key) => format!("press {}", key.detail),
                Event::KeyRelease(key) => format!("release {}", key.detail),
                Event::FocusOut(_) => "focus out".to_owned(),
                Event::MotionNotify(m) => format!("motion {}", m.event_x),
                _ => "other".to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_held_keys_queued_presses_go_until_its_release() {
        let mut queue = VecDeque::from([press(44), motion(5), press(44), press(45), press(44)]);
        drop_repeats(&mut queue, 44, false);
        assert_eq!(kinds(&queue), ["motion 5", "press 45"], "past other events");
        let mut queue = VecDeque::from([press(44), release(44), press(44), press(44)]);
        drop_repeats(&mut queue, 44, false);
        assert_eq!(
            kinds(&queue),
            ["release 44", "press 44", "press 44"],
            "taps after the release count"
        );
        let focus = Event::FocusOut(FocusInEvent::default());
        let mut queue = VecDeque::from([press(44), focus, press(44)]);
        drop_repeats(&mut queue, 44, false);
        assert_eq!(kinds(&queue), ["focus out", "press 44"]);
        let mut queue = VecDeque::from([release(45), press(44)]);
        drop_repeats(&mut queue, 44, false);
        assert_eq!(kinds(&queue), ["release 45"], "another key's release");
    }

    #[test]
    fn a_release_before_a_repeat_is_told_from_the_keys_coming_up() {
        let at = |keycode, time| KeyPressEvent {
            detail: keycode,
            time,
            ..KeyPressEvent::default()
        };
        let queue = |events: Vec<KeyPressEvent>| -> VecDeque<Event> {
            events.into_iter().map(Event::KeyPress).collect()
        };
        assert!(repeated(&queue(vec![at(65, 5)]), &at(65, 5)));
        assert!(
            !repeated(&queue(vec![at(65, 9)]), &at(65, 5)),
            "a later press"
        );
        assert!(
            !repeated(&queue(vec![at(66, 5)]), &at(65, 5)),
            "another key"
        );
        assert!(!repeated(&VecDeque::new(), &at(65, 5)), "nothing after");
    }

    #[test]
    fn a_held_keys_queued_presses_all_type_into_the_line() {
        let mut queue = VecDeque::from([press(38), press(38), motion(5), press(38)]);
        drop_repeats(&mut queue, 38, true);
        assert_eq!(
            kinds(&queue),
            ["press 38", "press 38", "motion 5", "press 38"]
        );
    }

    #[test]
    fn of_a_run_of_motions_the_latest_counts() {
        let mut queue = VecDeque::from([motion(2), motion(3), press(44), motion(4)]);
        let MotionNotifyEvent { event_x, .. } = latest_motion(
            &mut queue,
            MotionNotifyEvent {
                event_x: 1,
                ..MotionNotifyEvent::default()
            },
        );
        assert_eq!(event_x, 3);
        assert_eq!(kinds(&queue), ["press 44", "motion 4"]);
        let MotionNotifyEvent { event_x, .. } =
            latest_motion(&mut VecDeque::new(), MotionNotifyEvent::default());
        assert_eq!(event_x, 0, "alone");
    }

    #[test]
    fn an_x_error_names_its_kind_its_request_and_its_value() {
        let error = X11Error {
            error_kind: x11rb::protocol::ErrorKind::Match,
            error_code: 8,
            sequence: 7,
            bad_value: 0x40_0001,
            minor_opcode: 0,
            major_opcode: 12,
            extension_name: None,
            request_name: Some("ConfigureWindow"),
        };
        assert_eq!(
            error_line(&error),
            "X error Match in ConfigureWindow (12.0), value 0x400001"
        );
    }

    #[test]
    fn a_side_is_whole_pixels_the_protocol_takes() {
        assert_eq!(side(800.4), 800);
        assert_eq!(side(0.0), 1);
        assert_eq!(side(1e9), 32767);
    }
}
