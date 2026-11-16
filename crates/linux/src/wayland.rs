//! The Wayland frontend: an xdg-shell toplevel that the session draws into shared-memory
//! buffers in place, at the compositor's fractional scale where it offers one, through
//! smithay-client-toolkit, with the compositor's connection a source in the driver's loop.
//!
//! The compositor owns the window's size. The viewer picks a side only where the compositor
//! leaves it open, as for a new floating window. The toolkit's handlers run on the driver, so
//! each passes its event straight into the session.

mod clipboard;
mod input;
mod kwin;
mod size;
mod sway;
mod title;

use std::fs::File;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use calloop::PostAction;
use calloop::channel::{self, Channel};
use calloop::timer::{TimeoutAction, Timer};

use nsxiv_ng_core::config::{APP_NAME, Config};
use nsxiv_ng_core::file_list::FileEntry;
use nsxiv_ng_core::options::Options;
use nsxiv_ng_core::viewer::{Keystroke, Viewer};
use nsxiv_ng_core::viewport::ScaleMode;
use nsxiv_ng_core::{Point, Rect, Size};
use nsxiv_ng_render::Canvas;
use nsxiv_ng_session::{Bar, Button, Frame, Request, Session, key_handlers};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{
    CopyPasteSource, DataSourceHandler,
};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::backend::WaylandError;
use smithay_client_toolkit::reexports::client::globals::{GlobalList, registry_queue_init};
use smithay_client_toolkit::reexports::client::protocol::{
    wl_buffer, wl_callback, wl_data_device, wl_data_device_manager, wl_data_source, wl_display,
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use smithay_client_toolkit::reexports::client::{
    Connection, DispatchError, EventQueue, Proxy, QueueHandle,
};
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, BTN_RIGHT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec,
    ThemedPointer,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{
    self as xdg_window, DecorationMode, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shm::raw::RawPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};

use crate::driver::{self, Driver, Frontend, Handle};
use crate::keysyms;
use input::Axes;
use size::{Applied, Desktop, HELD, Hug, Room, Sizing};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
use title::Press;
use xkbcommon::xkb;

/// The buffers the compositor may hold at once; a frame past them waits for one to come back.
const BUFFERS: usize = 3;

/// Where a pointer is when it left the window: on no canvas, so a release there navigates
/// nowhere.
const NOWHERE: Point = Point { x: -1.0, y: -1.0 };

/// The probe's popup, as large as can be; one no compositor trimmed found no work area.
const PROBE: i32 = 1 << 20;

/// The room a probe that found no work area leaves, around the window's top-left corner.
const UNLIMITED: Rect = Rect {
    x: -1e9,
    y: -1e9,
    width: 2e9,
    height: 2e9,
};

/// What the compositor's helper thread runs, one at a time.
enum Job {
    /// sway's placement of the window that went from one size to another, which moves it, or
    /// only asks for the room its workspace gives.
    Place {
        from: (u32, u32),
        to: (u32, u32),
        moves: bool,
    },
    /// The KWin script, for the session's runtime folder.
    KWin(PathBuf),
}

/// The connection to the Wayland compositor, before the viewer's window is on it.
pub struct Wayland {
    connection: Connection,
    globals: GlobalList,
    queue: EventQueue<Driver<Window>>,
}

/// The session's Wayland compositor, None without one, or with a warning where it lacks what
/// the viewer needs: xdg-shell, shared memory and `wl_compositor` 4.
pub fn open() -> Option<Wayland> {
    let display = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
    if !display && std::env::var_os("WAYLAND_SOCKET").is_none() {
        return None;
    }
    let connection = Connection::connect_to_env().ok()?;
    let (globals, queue) = registry_queue_init(&connection).ok()?;
    let has = |interface: &str, version: u32| {
        globals.contents().with_list(|list| {
            list.iter()
                .any(|g| g.interface == interface && g.version >= version)
        })
    };
    // damage_buffer came with wl_compositor 4
    if !(has("wl_compositor", 4) && has("wl_shm", 1) && has("xdg_wm_base", 1)) {
        eprintln!(
            "{APP_NAME}: the Wayland compositor lacks xdg-shell, wl_shm or wl_compositor 4; \
             trying X11"
        );
        return None;
    }
    Some(Wayland {
        connection,
        globals,
        queue,
    })
}

impl Wayland {
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
        let geometry = options.geometry;
        let default = Size {
            width: config.window_width,
            height: config.window_height,
        };
        driver::run(
            |handle| Window::new(self, handle, geometry, default),
            |_, wake| {
                let bar = Bar::Drawn;
                let mut session =
                    Session::new(options, files, config, program, key_handlers(), bar, wake);
                session.can_edit(driver::edit_setup());
                session
            },
        )
    }
}

/// The viewer's window, in the driver's loop.
struct Window {
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    /// For the pointer's cursor surface and the room probe's.
    compositor: CompositorState,
    shell: XdgShell,
    shm: Shm,
    window: xdg_window::Window,
    desktop: Desktop,
    /// Where the compositor offers fractional scaling and viewports both, the window's.
    viewport: Option<WpViewport>,
    /// The fractional scale in 120ths, with the viewport; and the outputs' integer scale.
    scale120: Option<u32>,
    integer_scale: i32,
    sizing: Sizing,
    /// The compositor's bounds for the window, from the last configure that gave them.
    bounds: Option<(u32, u32)>,
    /// The last configure's states had the window maximized, fullscreen, resizing or tiled.
    held: bool,
    /// The room probe's popup while it runs, what the compositor made of it, and how many
    /// probes ran, which tells a probe's timeout from a later probe's.
    probe: Option<Popup>,
    probed: Option<(i32, i32, i32, i32)>,
    probes: u32,
    /// A probe waits for the window's size to reach the compositor, as the popup's anchor must
    /// lie inside the window the compositor has.
    probe_after_commit: bool,
    /// The work area around the window, during the resize that waited for it.
    room: Option<Rect>,
    /// The thread that runs swaymsg and dbus-send, where the compositor is sway or KWin, and
    /// the runtime folder for the KWin script, which loads once the window shows.
    helper: Option<mpsc::Sender<Job>>,
    kwin: Option<PathBuf>,
    /// The size before a burst of resizes, until sway places the window, whether it moves the
    /// window or only asks for the room, and the room sway's workspace gives a floating
    /// window's content.
    sway_from: Option<(u32, u32)>,
    sway_moves: bool,
    sway_room: Option<Size>,
    /// The size -g fixed, and the config's, which the grid goes back to.
    geometry: Option<Size>,
    default: Size,
    /// The rows of the bar, from the last fit the session asked for.
    bar: f64,
    /// The compositor decorates nothing, as GNOME and Weston, so the window draws its own
    /// title bar and resizes by its own edges.
    csd: bool,
    /// The title bar shows: the window draws it, and is not fullscreen.
    titled: bool,
    /// The title bar's height in logical units, 0 until the fonts are in, from the session.
    title_height: f64,
    /// The last configure's states had the window maximized.
    maximized: bool,
    /// Where the pointer is, in the window's logical units, the cursor it shows there, and the
    /// time of the last click on the title, which a second one may make a double click.
    pointer_at: Point,
    pointer_in: bool,
    cursor: CursorIcon,
    last_title_click: Option<u32>,
    configured: bool,
    /// The window's size in logical units when its buffer on the surface was drawn, so a
    /// configure that changes nothing is acked alone only where that buffer fits it.
    attached: Option<(u32, u32)>,
    /// The session lays out again once the loop is done with the current events, and draws.
    layout_due: bool,
    buffers: Vec<Buffer>,
    /// The last frame committed, which the bar alone may be drawn over.
    kept: Option<Kept>,
    /// A frame waits for the compositor to give a buffer back.
    waiting: bool,
    /// The last buffer asked for could not be made, which was said once.
    failed: bool,
    title: String,
    /// The seat input comes from, and its devices.
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Keyboards released, each kept until the sync sent after its release is done, so the
    /// server has deleted it by then and the last reference goes here, in dispatch. A
    /// workaround: dropping the toolkit's repeat data removes its timer from the loop
    /// (smithay-client-toolkit 0.21.1, src/seat/keyboard/repeat.rs), and where the last
    /// reference went with the deletion, read in calloop-wayland-source's
    /// before_handle_events, calloop has its sources borrowed (calloop 0.14.5,
    /// src/loop_logic.rs, the loop over those hooks): a panic.
    released: Vec<(wl_callback::WlCallback, wl_keyboard::WlKeyboard)>,
    display: wl_display::WlDisplay,
    /// The pointer, which shows the theme's cursor over the window, as a client sets its own.
    pointer: Option<ThemedPointer>,
    ctrl: bool,
    /// The keyboard's own keymap state, for a repeated key's symbol under the modifiers held
    /// now.
    xkb: Option<xkb::State>,
    /// The 120ths of a wheel step turned and not yet taken.
    wheel_rest: i32,
    handle: Handle<Window>,
    qh: QueueHandle<Driver<Window>>,
    /// The compositor's connection, which keeps the protocol error that broke it.
    connection: Connection,
    /// The clipboard: the compositor's data devices, the seat's, and the source of the last
    /// copy with its data, until the compositor cancels it or another copy takes its place.
    data_devices: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    source: Option<(CopyPasteSource, Rc<[u8]>)>,
    /// The serial of the last press, or compositor repeat, of the keyboard that has the
    /// window's focus, which a copy sets the selection with; none once it left, or the seat
    /// changed.
    serial: Option<u32>,
    /// The key held, the text its press typed and the modifiers then, which a compositor's
    /// repeat of it types again while they hold.
    pressed_text: Option<Held>,
    /// The modifiers now: depressed, latched, locked, and the layout.
    modifiers: Modifiers4,
}

/// The modifiers as the compositor gives them: depressed, latched, locked, and the layout.
type Modifiers4 = [u32; 4];

/// A key held, as its press left it.
struct Held {
    code: u32,
    text: Option<String>,
    modifiers: Modifiers4,
}

impl Window {
    fn new(
        wayland: Wayland,
        handle: &Handle<Window>,
        geometry: Option<Size>,
        default: Size,
    ) -> io::Result<Window> {
        let Wayland {
            connection,
            globals,
            queue,
        } = wayland;
        let qh = queue.handle();
        let display = connection.display();
        let ours = connection.clone();
        let compositor = CompositorState::bind(&globals, &qh).map_err(io::Error::other)?;
        let shell = XdgShell::bind(&globals, &qh).map_err(io::Error::other)?;
        let shm = Shm::bind(&globals, &qh).map_err(io::Error::other)?;
        let surface = compositor.create_surface(&qh);
        let fractional = globals.bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, Scaling);
        let viewporter = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, Scaling);
        let viewport = match (fractional, viewporter) {
            (Ok(fractional), Ok(viewporter)) => {
                fractional.get_fractional_scale(&surface, &qh, Scaling);
                Some(viewporter.get_viewport(&surface, &qh, Scaling))
            }
            _ => None,
        };
        // KDE and sway draw a title bar, niri a border; the configure says whether they will
        let window = shell.create_window(surface, WindowDecorations::RequestServer, &qh);
        window.set_title(APP_NAME);
        window.set_app_id(APP_NAME);
        // with no buffer: asks the compositor for the first configure, once the session has
        // started, so fullscreen with -f is asked for before it
        handle.insert_idle(|driver| driver.frontend().window.commit());
        // A protocol error leaves the source reading the stored error with the socket still
        // readable, and dispatching no event, so the loop would turn without end: a connection
        // that holds an error fails the dispatch instead, which ends the loop.
        let broken = connection.clone();
        let source = WaylandSource::new(connection, queue);
        driver::insert(handle, source, move |_, queue, driver| {
            match broken.backend().last_error() {
                Some(error) => Err(DispatchError::Backend(error)),
                None => queue.dispatch_pending(driver),
            }
        })?;
        // sessions spell these differently: Wayfire, wayfire, ubuntu:GNOME
        // shortcut: a SWAYSOCK another compositor inherited, as labwc started from a sway
        // session, reads as sway, so the room goes unprobed and the window may grow off the
        // screen; take sway only once its tree names the viewer's window, and
        // probe until then, if that ever matters
        let sway = std::env::var_os("SWAYSOCK").is_some_and(|socket| Path::new(&socket).exists());
        let names = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        let desktop = Desktop::new(&names, sway);
        // dbus-send finds the session's bus where the session names it, else in the runtime
        // folder
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|a| !a.is_empty())
            || runtime
                .as_ref()
                .is_some_and(|runtime| runtime.join("bus").exists());
        let kwin = runtime.filter(|_| desktop.kde && bus);
        let helper = match desktop.sway || kwin.is_some() {
            true => Some(helper(handle)?),
            false => None,
        };
        Ok(Window {
            registry: RegistryState::new(&globals),
            seats: SeatState::new(&globals, &qh),
            outputs: OutputState::new(&globals, &qh),
            compositor,
            shell,
            shm,
            window,
            desktop,
            // shortcut: until the surface enters an output, its first frame, and the thumbnails
            // asked for before, go at scale 1; wait for the first preferred
            // scale where the compositor offers one, if a frame drawn twice ever matters
            scale120: viewport.is_some().then_some(120),
            viewport,
            integer_scale: 1,
            sizing: Sizing::new(geometry.unwrap_or(default)),
            bounds: None,
            held: false,
            probe: None,
            probed: None,
            probes: 0,
            probe_after_commit: false,
            room: None,
            helper,
            kwin,
            sway_from: None,
            sway_moves: false,
            sway_room: None,
            geometry,
            default,
            bar: 0.0,
            csd: false,
            titled: false,
            title_height: 0.0,
            maximized: false,
            pointer_at: NOWHERE,
            pointer_in: false,
            cursor: CursorIcon::Default,
            last_title_click: None,
            configured: false,
            attached: None,
            layout_due: false,
            buffers: Vec::new(),
            kept: None,
            waiting: false,
            failed: false,
            title: String::new(),
            seat: None,
            keyboard: None,
            released: Vec::new(),
            display,
            pointer: None,
            ctrl: false,
            xkb: None,
            wheel_rest: 0,
            handle: handle.clone(),
            data_devices: DataDeviceManagerState::bind(&globals, &qh).ok(),
            data_device: None,
            source: None,
            serial: None,
            pressed_text: None,
            modifiers: [0; 4],
            qh,
            connection: ours,
        })
    }

    fn scale(&self) -> f64 {
        size::device_scale(self.scale120, self.integer_scale)
    }

    // The rows of the title bar the window draws itself where it shows, else 0.
    fn title_rows(&self) -> f64 {
        if self.titled { self.title_height } else { 0.0 }
    }

    // The rows above and below the image that are not canvas: the title bar's and the bar's.
    fn chrome(&self) -> f64 {
        self.title_rows() + self.bar
    }

    // The edge at `at`, where the window resizes by its own edges: where it draws its own
    // decorations and nothing holds its size.
    fn edge(&self, at: Point) -> Option<ResizeEdge> {
        if !self.csd || self.held {
            return None;
        }
        let (width, height) = self.sizing.size;
        title::edge(at, f64::from(width), f64::from(height))
    }

    // A client sets its own cursor on Wayland: the edge's where the pointer is, else the
    // default, set again on `entering`.
    fn show_cursor(&mut self, connection: &Connection, entering: bool) {
        let icon = title::cursor(self.edge(self.pointer_at));
        if self.pointer_in && (entering || icon != self.cursor) {
            if let Some(pointer) = &self.pointer {
                let _ = pointer.set_cursor(connection, icon);
            }
            self.cursor = icon;
        }
    }

    // What a press on the title bar or an edge does, which the compositor carries out with the
    // press's serial; None where the press is the viewer's.
    fn decoration_pressed(
        &mut self,
        right: bool,
        at: Point,
        time: u32,
        serial: u32,
    ) -> Option<Press> {
        let seat = self.seat.clone()?;
        let width = f64::from(self.sizing.size.0);
        let edge = self.edge(at);
        let press = title::press(
            right,
            at,
            width,
            self.title_rows(),
            edge,
            self.last_title_click,
            time,
        )?;
        let toplevel = &self.window;
        match press {
            Press::Resize(edge) => toplevel.resize(&seat, serial, edge),
            Press::Move => {
                self.last_title_click = Some(time);
                toplevel.move_(&seat, serial);
            }
            Press::Menu => toplevel.show_window_menu(&seat, serial, (at.x as i32, at.y as i32)),
            Press::Maximize => {
                self.last_title_click = None;
                match self.maximized {
                    true => toplevel.unset_maximized(),
                    false => toplevel.set_maximized(),
                }
            }
            // the viewer quits, as on q
            Press::Close => {}
        }
        Some(press)
    }

    // The room the window may grow into: the compositor's bounds, sway's workspace, else the
    // largest output, one that is going left out.
    fn max_size(&self, gone: Option<&wl_output::WlOutput>) -> Size {
        use wl_output::Transform;
        if self.bounds.is_none()
            && let Some(room) = self.sway_room
        {
            return room;
        }
        let outputs: Vec<size::Output> = self
            .outputs
            .outputs()
            .filter(|output| Some(output) != gone)
            .filter_map(|output| self.outputs.info(&output))
            .map(|info| size::Output {
                mode: info
                    .modes
                    .iter()
                    .find(|mode| mode.current)
                    .map_or((0, 0), |mode| mode.dimensions),
                turned: matches!(
                    info.transform,
                    Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
                ),
                scale: info.scale_factor,
                logical: info.logical_size,
            })
            .collect();
        let fractional = self
            .scale120
            .filter(|&scale| scale != 120)
            .map(|scale| f64::from(scale) / 120.0);
        size::max_size(self.bounds, &outputs, fractional)
    }

    // Takes a configure, already acked, the decorations already taken from it; true when the
    // window's size changed. Where the decorations changed, the window fits its image again.
    fn configure(&mut self, configure: &WindowConfigure, hug: Option<Hug>, refit: bool) -> bool {
        let side = |given: Option<NonZeroU32>| given.map_or(0, NonZeroU32::get);
        let requested = (side(configure.new_size.0), side(configure.new_size.1));
        let binds = configure.state.intersects(self.desktop.binding());
        self.held = configure.state.intersects(HELD);
        self.bounds = configure.suggested_bounds;
        let left = self.sizing.configure(
            requested,
            binds,
            configure.is_resizing(),
            configure.is_fullscreen(),
        );
        self.configured = true;
        self.maximized = configure.is_maximized();
        let max = self.max_size(None);
        if (left || refit)
            && let Some(hug) = hug
        {
            self.sizing.fit(hug.image, hug.rows, max);
        }
        let changed = self.apply(max, hug);
        if self.sway_room.is_none() {
            self.sway_resized(self.sizing.size, true);
        }
        changed
    }

    // The size rule, with the room the window knows around it; true when the size changed. A
    // window that grows itself where the room is unknown probes it first, and the probe's
    // answer runs the rule again.
    fn apply(&mut self, max: Size, hug: Option<Hug>) -> bool {
        // a window not shown yet grows freely, as the compositor places it once it shows
        let room = match self.room {
            Some(area) => Room::Found(area, self.desktop.keeps_top_left),
            None if self.desktop.probes_room() && self.attached.is_some() => Room::Unknown,
            None => Room::Kept,
        };
        let old = self.sizing.size;
        match self.sizing.apply(max, hug, room) {
            Applied::Same => false,
            Applied::Probe => {
                if self.attached == Some(old) {
                    self.start_probe();
                } else {
                    self.probe_after_commit = true;
                }
                false
            }
            Applied::Changed => {
                let from = self.sizing.from;
                if self.sizing.open() && from.0 > 0 {
                    self.sway_resized(from, true);
                }
                true
            }
        }
    }

    // Most compositors give a client no way to move its window, and few keep a growing window
    // on screen. A client can still ask how much room it has: an unmapped popup centered on the
    // window, as large as can be and allowed only to shrink, comes back trimmed to the work
    // area of the window's screen, and the window grows only as far as that leaves room.
    fn start_probe(&mut self) {
        let Some((width, height)) = self.attached.filter(|_| self.probe.is_none()) else {
            return;
        };
        let Ok(positioner) = XdgPositioner::new(&self.shell) else {
            return;
        };
        positioner.set_size(PROBE, PROBE);
        positioner.set_anchor_rect(0, 0, width as i32, height as i32);
        positioner.set_anchor(Anchor::None);
        positioner.set_gravity(Gravity::None);
        positioner.set_constraint_adjustment(
            ConstraintAdjustment::ResizeX | ConstraintAdjustment::ResizeY,
        );
        // with no buffer: asks only for the popup's configure
        let parent = self.window.xdg_surface();
        let Ok(popup) = Popup::new(parent, &positioner, &self.qh, &self.compositor, &self.shell)
        else {
            return;
        };
        self.probe = Some(popup);
        self.probed = None;
        self.probes += 1;
        // a compositor that never answers gets no cap
        let probe = self.probes;
        let timeout = move |_, _: &mut (), driver: &mut Driver<Window>| {
            let window = driver.frontend();
            if window.probe.is_some() && window.probes == probe {
                probe_answered(driver);
            }
            TimeoutAction::Drop
        };
        let _ = driver::insert(
            &self.handle,
            Timer::from_duration(Duration::from_millis(250)),
            timeout,
        );
    }

    // sway keeps a floating window's top-left corner as it resizes itself, so its IPC places
    // the window, from the size `old` before a burst of resizes within 50 ms, which share one
    // placement, where it `moves`, else asks only for the room. A placement counts from the
    // window's first frame: sway places the window itself where it shows it.
    fn sway_resized(&mut self, old: (u32, u32), moves: bool) {
        if !self.desktop.sway || self.held || self.attached.is_none() {
            return;
        }
        self.sway_moves |= moves;
        if self.sway_from.is_some() {
            return;
        }
        self.sway_from = Some(old);
        let place = |_, _: &mut (), driver: &mut Driver<Window>| {
            let window = driver.frontend();
            let moves = std::mem::take(&mut window.sway_moves);
            // fullscreen or maximized meanwhile: sway draws no title bar or borders then
            if let Some(from) = window.sway_from.take()
                && !window.held
                && let Some(jobs) = &window.helper
            {
                let to = window.sizing.size;
                let _ = jobs.send(Job::Place { from, to, moves });
            }
            TimeoutAction::Drop
        };
        let _ = driver::insert(
            &self.handle,
            Timer::from_duration(Duration::from_millis(50)),
            place,
        );
    }

    // Sizes the window to the image, unless the compositor holds it fullscreen; true when the
    // size changed. Without an image, as in the grid or with -g, the rule still holds the size
    // to the room, which may have come or shrunk.
    fn fit(&mut self, hug: Option<Hug>, gone: Option<&wl_output::WlOutput>) -> bool {
        let max = self.max_size(gone);
        if let Some(fitted) = hug {
            self.sizing.fit(fitted.image, fitted.rows, max);
        }
        self.configured && !self.sizing.fullscreen && self.apply(max, hug)
    }

    // The session lays out for the new size once the current events are carried out, and the
    // frame drawn meanwhile waits for it.
    fn lay_out(&mut self) {
        if self.layout_due {
            return;
        }
        self.layout_due = true;
        self.handle.insert_idle(|driver| {
            driver.frontend().layout_due = false;
            driver.layout();
        });
    }

    fn set_title(&mut self, viewer: &Viewer) {
        let Some(entry) = viewer.current_entry() else {
            return;
        };
        let name = match entry.path.file_name() {
            Some(name) => name.to_string_lossy(),
            None => entry.path.to_string_lossy(),
        };
        if name != self.title {
            self.title = name.into_owned();
            self.window.set_title(&self.title);
        }
    }

    // A frame, drawn by the session into a buffer the compositor does not hold, at the window's
    // size times its scale, then committed.
    fn draw(&mut self, session: &mut Session) {
        self.set_title(session.viewer());
        let (width, height) = self.sizing.size;
        if !self.configured || self.layout_due || width == 0 || height == 0 {
            return;
        }
        // no buffer before the first frame is final, so the compositor first sees the window at
        // the size it keeps: the first image's with the bars' rows
        if self.attached.is_none() && !session.frame_is_final() {
            return;
        }
        let scale = self.scale();
        let (w, h) = (pixels(width, scale), pixels(height, scale));
        if w == 0 || h == 0 {
            return;
        }
        let Some(index) = self.free_buffer(w, h) else {
            return;
        };
        let top = self.title_top(h, scale);
        let name = self.title_name().to_owned();
        let buffer = &mut self.buffers[index];
        let Some(pixels) = buffer.pixels() else {
            return;
        };
        let (title, below) = pixels.split_at_mut(w as usize * top as usize);
        if let Some(mut canvas) = Canvas::new(title, w, top).filter(|_| top > 0) {
            session.draw_title(&mut canvas, &name, scale);
        }
        let Some(mut canvas) = Canvas::new(below, w, h - top) else {
            return;
        };
        // no promise that the canvas holds the last frame: the buffer drawn into is one the
        // compositor gave back, which holds an older frame or none
        session.draw(&mut canvas, scale, Frame::default());
        let surface = self.window.wl_surface();
        match &self.viewport {
            Some(viewport) => viewport.set_destination(width as i32, height as i32),
            None => surface.set_buffer_scale(scale as i32),
        }
        surface.attach(Some(&buffer.buffer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        surface.commit();
        buffer.busy = true;
        self.kept = Some(Kept {
            buffer: buffer.buffer.clone(),
            scale,
            top,
            rows: session.bar_rows(h - top, scale),
            title: name,
        });
        let first = self.attached.is_none();
        self.attached = Some((width, height));
        if first {
            self.shown();
        }
        // the growth that waited for this size to reach the compositor
        if std::mem::take(&mut self.probe_after_commit) {
            self.start_probe();
        }
    }

    // The bar alone over the last frame, and the title bar where its name changed, for a redraw
    // of the bar alone: in the buffer that holds that frame where the compositor gave it back,
    // else in a free one the frame is copied into. False where the whole frame is to be drawn,
    // as the last frame is not the one to draw over: of another size, scale, title bar or bar,
    // a layout or a whole frame pending, the bar hidden, or none drawn yet. The window asks for
    // no frame callbacks, so none is awaited.
    fn draw_bar(&mut self, session: &mut Session) -> bool {
        self.set_title(session.viewer());
        let (width, height) = self.sizing.size;
        let pending = self.layout_due || self.waiting;
        if !self.configured || pending || self.attached != Some((width, height)) {
            return false;
        }
        let scale = self.scale();
        let (w, h) = (pixels(width, scale), pixels(height, scale));
        if w == 0 || h == 0 {
            return false;
        }
        let top = self.title_top(h, scale);
        let rows = session.bar_rows(h - top, scale);
        let name = self.title_name().to_owned();
        let Some(kept) = &self.kept else {
            return false;
        };
        if (kept.scale, kept.top, kept.rows) != (scale, top, rows) || rows == 0 {
            return false;
        }
        let (last, retitled) = (kept.buffer.clone(), top > 0 && kept.title != name);
        let holds = |b: &Buffer| b.buffer == last && (b.width, b.height) == (w, h);
        let Some(at) = self.buffers.iter().position(holds) else {
            return false;
        };
        let index = match self.buffers[at].busy {
            false => at,
            true => {
                // drawn whole when the compositor gives a buffer back, where none is free
                let Some(index) = self.free_buffer(w, h) else {
                    return true;
                };
                let Some(at) = self.buffers.iter().position(|b| b.buffer == last) else {
                    return false;
                };
                let (from, to) = match at < index {
                    true => {
                        let (left, right) = self.buffers.split_at_mut(index);
                        (&mut left[at], &mut right[0])
                    }
                    false => {
                        let (left, right) = self.buffers.split_at_mut(at);
                        (&mut right[0], &mut left[index])
                    }
                };
                let (Some(from), Some(to)) = (from.pixels(), to.pixels()) else {
                    return false;
                };
                to.copy_from_slice(from);
                index
            }
        };
        let buffer = &mut self.buffers[index];
        let Some(pixels) = buffer.pixels() else {
            return false;
        };
        let (title, below) = pixels.split_at_mut(w as usize * top as usize);
        let bar = &mut below[(h - top - rows) as usize * w as usize..];
        let Some(mut canvas) = Canvas::new(bar, w, rows) else {
            return false;
        };
        session.draw_bar(&mut canvas, scale);
        if let Some(mut canvas) = Canvas::new(title, w, top).filter(|_| retitled) {
            session.draw_title(&mut canvas, &name, scale);
        }
        let surface = self.window.wl_surface();
        surface.attach(Some(&buffer.buffer), 0, 0);
        surface.damage_buffer(0, (h - rows) as i32, w as i32, rows as i32);
        if retitled {
            surface.damage_buffer(0, 0, w as i32, top as i32);
        }
        surface.commit();
        buffer.busy = true;
        self.kept = Some(Kept {
            buffer: buffer.buffer.clone(),
            scale,
            top,
            rows,
            title: name,
        });
        true
    }

    // A buffer `w` × `h` the compositor does not hold, made where none is and fewer than
    // BUFFERS are; None where the frame waits for one to come back, or none could be made.
    fn free_buffer(&mut self, w: u32, h: u32) -> Option<usize> {
        self.buffers
            .retain(|b| b.busy || (b.width, b.height) == (w, h));
        if let Some(free) = self.buffers.iter().position(|b| !b.busy) {
            self.failed = false;
            return Some(free);
        }
        if self.buffers.len() >= BUFFERS {
            // drawn when the compositor gives a buffer back
            self.waiting = true;
            return None;
        }
        match Buffer::new(&self.shm, w, h, &self.qh) {
            Ok(buffer) => {
                self.failed = false;
                self.buffers.push(buffer);
                Some(self.buffers.len() - 1)
            }
            Err(e) => {
                // once for a streak of failures, not for every frame
                if !self.failed {
                    let line = format!("cannot make a buffer for the window: {e}");
                    eprintln!("{APP_NAME}: {}", crate::printable(&line));
                    self.failed = true;
                }
                self.waiting = waits(self.buffers.len());
                None
            }
        }
    }

    // The title bar's rows at the top of a frame `h` pixels high at `scale`, at least one row
    // left to the session; none where it does not show.
    fn title_top(&self, h: u32, scale: f64) -> u32 {
        match self.titled {
            true => ((self.title_height * scale).round() as u32).min(h - 1),
            false => 0,
        }
    }

    fn title_name(&self) -> &str {
        match self.title.is_empty() {
            true => APP_NAME,
            false => self.title.as_str(),
        }
    }

    // The window's first frame is on the surface: sway, which centered it, tells the room its
    // workspace gives, with no move, and KWin gets the script that keeps it on screen.
    fn shown(&mut self) {
        self.sway_resized(self.sizing.size, false);
        if let (Some(jobs), Some(runtime)) = (&self.helper, self.kwin.take()) {
            let _ = jobs.send(Job::KWin(runtime));
        }
    }

    // True when there was a keyboard, whose keys held come up with it.
    fn release_keyboard(&mut self) -> bool {
        (self.xkb, self.serial, self.pressed_text) = (None, None, None);
        let Some(keyboard) = self.keyboard.take() else {
            return false;
        };
        if keyboard.version() >= 3 {
            keyboard.release();
            let done = self.display.sync(&self.qh, Synced);
            self.released.push((done, keyboard));
        }
        true
    }

    /// An edit's copy, `data` offered as `types`, the selection of the seat from the key
    /// pressed last; it takes the place of the copy before.
    fn copy(&mut self, data: Vec<u8>, types: &[&str]) {
        let (Some(manager), Some(device)) = (&self.data_devices, &self.data_device) else {
            eprintln!("{APP_NAME}: cannot copy: the compositor gives the window no clipboard");
            return;
        };
        // the compositor takes a selection only with the serial of an input the window has the
        // focus for, as a copy that waited for the fonts may no longer have
        let Some(serial) = self.serial else {
            eprintln!("{APP_NAME}: cannot take the clipboard without the keyboard's focus");
            return;
        };
        let source = manager.create_copy_paste_source(&self.qh, types.iter().copied());
        source.set_selection(device, serial);
        // the source before goes as it drops
        self.source = Some((source, data.into()));
    }

    // True when the pointer went, which ends a drag; it is released as it drops.
    fn release_pointer(&mut self) -> bool {
        self.pointer_in = false;
        self.pointer.take().is_some()
    }
}

impl Frontend for Window {
    // the session's canvas, the bar's rows in it, lies below the title bar
    fn window(&self) -> (Size, f64) {
        let window = self.sizing.window();
        let height = (window.height - self.title_rows()).max(0.0);
        (Size { height, ..window }, self.scale())
    }

    fn request(&mut self, session: &mut Session, request: Request) {
        // the title bar's height comes with the fonts, and the session's canvas changes with it
        let title_height = session.title_height();
        if title_height != self.title_height {
            self.title_height = title_height;
            if refits(self.titled, &request) {
                let hug = fitted(session.viewer(), self.chrome());
                self.fit(hug, None);
            }
            if self.titled {
                self.lay_out();
            }
        }
        match request {
            Request::Redraw { bar_only } => {
                if !(bar_only && self.draw_bar(session)) {
                    self.draw(session);
                }
            }
            Request::FitWindow { image, bar } => {
                self.bar = bar;
                let down = session.viewer().viewport().scale_mode() == ScaleMode::Down;
                let rows = self.chrome();
                if self.fit(Some(Hug { image, rows, down }), None) {
                    self.lay_out();
                }
            }
            Request::ResetWindowSize => {
                if self.geometry.is_some() {
                    return;
                }
                self.sizing.reset(self.default);
                let hug = fitted(session.viewer(), self.chrome());
                let max = self.max_size(None);
                if self.configured && !self.sizing.fullscreen && self.apply(max, hug) {
                    self.lay_out();
                }
            }
            // before the first configure this asks to start in fullscreen
            Request::ToggleFullscreen => match self.sizing.fullscreen {
                true => self.window.unset_fullscreen(),
                false => self.window.set_fullscreen(None),
            },
            Request::CopyImage(png) => self.copy(png, &clipboard::IMAGE),
            Request::CopyText(text) => self.copy(text.into_bytes(), &clipboard::TEXT),
            // Wayland has no bell a client may ring
            _ => {}
        }
    }

    // the key-handler runs while the window stays as it is
    fn release(&mut self) {}

    fn reclaim(&mut self) {}

    // says it lost the connection, with the reason the connection keeps, a protocol error's
    // text included, which the loop's error names only by its errno
    fn lost(&self, error: io::Error) -> io::Error {
        lost_connection(self.connection.backend().last_error(), error)
    }
}

/// The error the viewer ends with where the loop failed: where the connection holds an error,
/// one that says it was lost and why, else the loop's own.
fn lost_connection(broken: Option<WaylandError>, error: io::Error) -> io::Error {
    let why = match broken {
        Some(WaylandError::Io(e)) => e.to_string(),
        Some(WaylandError::Protocol(e)) => e.to_string(),
        None => return error,
    };
    io::Error::other(format!(
        "lost the connection to the Wayland compositor: {why}"
    ))
}

/// Whether a new height of the title bar fits the window to its image again: where the title
/// shows, as its rows are the image's no longer, unless the request at hand is a fit that
/// counts them itself. The session asks for no fit of its own with the bar hidden.
fn refits(titled: bool, request: &Request) -> bool {
    titled && !matches!(request, Request::FitWindow { .. })
}

// The image the window fits and hugs, none in the grid or with -g, with `rows` that are not
// canvas.
fn fitted(viewer: &Viewer, rows: f64) -> Option<Hug> {
    let image = viewer.fitted_size()?;
    let down = viewer.viewport().scale_mode() == ScaleMode::Down;
    Some(Hug { image, rows, down })
}

// The image the window fits and hugs now, with the title bar's rows and the bar's of the last
// fit.
fn shown(driver: &mut Driver<Window>) -> Option<Hug> {
    let rows = driver.frontend().chrome();
    fitted(driver.viewer(), rows)
}

// The scale or the room on the screens changed: the window fits its image again, and the
// session gets the new scale with a whole frame.
fn rescaled(driver: &mut Driver<Window>, connection: &Connection) {
    let hug = shown(driver);
    let window = driver.frontend();
    window.fit(hug, None);
    // the theme's cursor at the new scale
    window.show_cursor(connection, true);
    // on another output, sway's workspace may leave another room
    if window.sizing.size.0 > 0 {
        window.sway_resized(window.sizing.size, true);
    }
    driver.layout();
}

// The probe's popup came back, trimmed to the work area or not, or never came: the window
// grows within that room, relative to its top-left corner, then forgets it, as it may move
// before it grows again.
fn probe_answered(driver: &mut Driver<Window>) {
    let window = driver.frontend();
    if window.probe.take().is_none() {
        return;
    }
    let area = window
        .probed
        .take()
        .filter(|&(_, _, width, height)| width < PROBE && height < PROBE)
        .map_or(UNLIMITED, |(x, y, width, height)| Rect {
            x: f64::from(x),
            y: f64::from(y),
            width: f64::from(width),
            height: f64::from(height),
        });
    let hug = shown(driver);
    let window = driver.frontend();
    window.room = Some(area);
    let max = window.max_size(None);
    let changed = window.apply(max, hug);
    window.room = None;
    if changed {
        driver.layout();
    }
}

// sway placed the window, whose content has `room` in its workspace.
fn sway_answered(driver: &mut Driver<Window>, room: Size) {
    let window = driver.frontend();
    if window.sway_room == Some(room) || window.held {
        return;
    }
    window.sway_room = Some(room);
    let hug = shown(driver);
    if driver.frontend().fit(hug, None) {
        driver.layout();
    }
}

// The thread that runs the compositor's programs one at a time, so the loop never waits on
// them; sway's answers come back into the loop.
fn helper(handle: &Handle<Window>) -> io::Result<mpsc::Sender<Job>> {
    let (rooms, answers): (channel::Sender<Size>, Channel<Size>) = channel::channel();
    driver::insert(handle, answers, |event, _, driver| {
        if let channel::Event::Msg(room) = event {
            sway_answered(driver, room);
        }
    })?;
    let (jobs, queue) = mpsc::channel();
    let pid = std::process::id();
    std::thread::Builder::new()
        .name("compositor".to_owned())
        .spawn(move || {
            for job in queue {
                match job {
                    Job::Place { from, to, moves } => {
                        if let Some(room) = sway::place(pid, from, to, moves) {
                            let _ = rooms.send(room);
                        }
                    }
                    Job::KWin(runtime) => kwin::load(pid, &runtime),
                }
            }
        })?;
    Ok(jobs)
}

// The room on the screens may have changed; `gone` is an output about to go.
fn screens_changed(driver: &mut Driver<Window>, gone: Option<&wl_output::WlOutput>) {
    let hug = shown(driver);
    let window = driver.frontend();
    let changed = window.fit(hug, gone);
    // sway's workspace may leave another room, whether or not the fit changed the size
    if window.sizing.size.0 > 0 {
        window.sway_resized(window.sizing.size, true);
    }
    if changed {
        driver.layout();
    }
}

// A key pressed, or repeated, which types `utf8`: the session's text line takes the character
// a key types, composed with the dead keys and Compose before it, where the locale has their
// table; other keys go by their names.
fn pressed(driver: &mut Driver<Window>, keysym: Keysym, utf8: Option<&str>) {
    if driver.typing() {
        match typing(keysym, utf8) {
            Some(Some(typed)) => {
                let ctrl = driver.frontend().ctrl;
                driver.key(&Keystroke { key: typed, ctrl });
                return;
            }
            Some(None) => return,
            None => {}
        }
    }
    match keystroke(keysym, driver.frontend().ctrl) {
        Some(Some(key)) => {
            driver.key(&key);
        }
        Some(None) => driver.event(|session| {
            session.end_prefix();
            Vec::new()
        }),
        None => {}
    }
}

/// The text a compositor's repeat of key `code` types under `modifiers`: what its press typed,
/// composed as it was, while the modifiers are the press's; else the key's text under the
/// modifiers held `now`, as the toolkit's own repeat takes it once they change, so a release of
/// Shift or Control while the key is held repeats it plain.
fn repeat_text(
    held: Option<&Held>,
    code: u32,
    modifiers: Modifiers4,
    now: Option<String>,
) -> Option<String> {
    match held {
        Some(held) if held.code == code && held.modifiers == modifiers => held.text.clone(),
        _ => now,
    }
}

/// What a key does while the session's text line is open: it types the one character of its
/// text; it types nothing where it has no text though its symbol is a character, as a step of
/// a compose sequence or a dead key, whose text comes with the key that ends it; None for a key
/// that goes by its name, as Return, BackSpace, Delete, Escape, the arrows, Home and End.
fn typing(keysym: Keysym, utf8: Option<&str>) -> Option<Option<String>> {
    match typed(utf8) {
        Some(text) => Some(Some(text)),
        None if keysyms::typed(keysym).is_some() => Some(None),
        None => None,
    }
}

/// The one character a key types, as the session's text line takes it: none for a key that
/// types several, or none, or a control character, as Return and BackSpace do, which go by
/// their names.
fn typed(utf8: Option<&str>) -> Option<String> {
    let mut chars = utf8?.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Some(c.to_string()),
        _ => None,
    }
}

/// A key's keysym as the viewer takes it: named for the bindings, with Control from the
/// modifiers; Some(None) for a key without a name, which still ends a C-x prefix; None for a
/// modifier alone, which ends none, as C-x C-a may press Control again.
fn keystroke(keysym: Keysym, ctrl: bool) -> Option<Option<Keystroke>> {
    match keysyms::key_name(keysym) {
        Some(name) if ctrl => Some(Some(Keystroke::ctrl(&name))),
        Some(name) => Some(Some(Keystroke::new(&name))),
        None if keysym.is_modifier_key() => None,
        None => Some(None),
    }
}

// The keys held no longer come up here, as when the keyboard left the window or went: space,
// held to move a shape being drawn, is up for the session, so a drag after resizes it again.
fn keys_up(driver: &mut Driver<Window>) {
    driver.event(|session| session.key_released(&Keystroke::new(" ")));
}

// The button no longer comes up here, as when the pointer left or went: a drag ends.
fn end_drag(driver: &mut Driver<Window>) {
    driver.event(|session| session.release(Button::Left, NOWHERE));
}

// Input from one seat, the first with a keyboard or a pointer, with the devices it has now:
// keyboards and mice come and go, and so may seats. `gone` is a seat about to go.
fn follow_seats(driver: &mut Driver<Window>, gone: Option<&wl_seat::WlSeat>) {
    let window = driver.frontend();
    let seats: Vec<(wl_seat::WlSeat, bool, bool)> = window
        .seats
        .seats()
        .filter(|seat| Some(seat) != gone)
        .map(|seat| {
            let info = window.seats.info(&seat);
            let (keyboard, pointer) =
                info.map_or((false, false), |i| (i.has_keyboard, i.has_pointer));
            (seat, keyboard, pointer)
        })
        .collect();
    let inputs: Vec<_> = seats
        .iter()
        .map(|(seat, keyboard, pointer)| (seat.clone(), *keyboard || *pointer))
        .collect();
    let seat = input::input_seat(window.seat.as_ref(), &inputs);
    let (mut lost_keyboard, mut lost_pointer) = (false, false);
    if seat != window.seat {
        lost_keyboard = window.release_keyboard();
        lost_pointer = window.release_pointer();
        (window.data_device, window.serial) = (None, None);
        window.seat.clone_from(&seat);
    }
    let (keyboard, pointer) = seats
        .iter()
        .find(|(s, _, _)| Some(s) == seat.as_ref())
        .map_or((false, false), |(_, keyboard, pointer)| {
            (*keyboard, *pointer)
        });
    if let Some(seat) = &seat {
        if keyboard && window.keyboard.is_none() {
            let repeat = Box::new(
                |driver: &mut Driver<Window>, from: &wl_keyboard::WlKeyboard, event: KeyEvent| {
                    // the toolkit's timer outlives a keyboard unplugged while a key is held
                    let window = driver.frontend();
                    if window.keyboard.as_ref() != Some(from) {
                        return;
                    }
                    // the toolkit keeps the symbol of the press; the modifiers held now decide
                    let code = xkb::Keycode::new(event.raw_code + 8);
                    let keysym = window
                        .xkb
                        .as_ref()
                        .map_or(event.keysym, |state| state.key_get_one_sym(code));
                    // the toolkit's text of the key under the modifiers held now, each repeat
                    // typing it again
                    pressed(driver, keysym, event.utf8.as_deref());
                },
            );
            let handle = window.handle.clone();
            // shortcut: a keyboard of a seat before version 4 does not repeat, as the toolkit
            // never gives its own timer the rate it makes up for one, where 25 a second after
            // 600 ms would serve; no compositor still in use binds such a seat, so it stays
            // until one does
            window.keyboard = window
                .seats
                .get_keyboard_with_repeat(&window.qh, seat, None, handle, repeat)
                .ok();
        }
        if window.data_device.is_none()
            && let Some(manager) = &window.data_devices
        {
            window.data_device = Some(manager.get_data_device(&window.qh, seat));
        }
        if pointer && window.pointer.is_none() {
            let cursor = window.compositor.create_surface(&window.qh);
            let shm = window.shm.wl_shm();
            let theme = ThemeSpec::default();
            window.pointer = window
                .seats
                .get_pointer_with_theme::<_, ()>(&window.qh, seat, shm, cursor, theme)
                .ok();
        }
    }
    if !keyboard {
        lost_keyboard |= window.release_keyboard();
    }
    if !pointer {
        lost_pointer |= window.release_pointer();
    }
    if lost_keyboard {
        keys_up(driver);
    }
    if lost_pointer {
        end_drag(driver);
    }
}

/// Whether a frame that found no free buffer, and could not make one, waits for the compositor
/// to give one of the `held` buffers back: only a release draws it then. With none held no
/// release comes, and the next redraw tries again.
fn waits(held: usize) -> bool {
    held > 0
}

/// A side in logical units as pixels at `scale`.
fn pixels(side: u32, scale: f64) -> u32 {
    (f64::from(side) * scale).round() as u32
}

/// The last frame committed: the buffer it is in, the scale it was drawn at, the rows of its
/// title bar and its bar, and the name in its title bar.
struct Kept {
    buffer: wl_buffer::WlBuffer,
    scale: f64,
    top: u32,
    rows: u32,
    title: String,
}

/// A buffer the compositor shows, over memory both sides map, which the session draws into in
/// place.
struct Buffer {
    pool: RawPool,
    buffer: wl_buffer::WlBuffer,
    width: u32,
    height: u32,
    /// The compositor holds it, until it gives it back.
    busy: bool,
}

impl Buffer {
    fn new(
        shm: &Shm,
        width: u32,
        height: u32,
        qh: &QueueHandle<Driver<Window>>,
    ) -> io::Result<Buffer> {
        let stride = width * 4;
        let mut pool =
            RawPool::new(stride as usize * height as usize, shm).map_err(io::Error::other)?;
        // the canvas's premultiplied ARGB words, shown opaque
        let format = wl_shm::Format::Xrgb8888;
        let (w, h) = (width as i32, height as i32);
        let buffer = pool.create_buffer(0, w, h, stride as i32, format, Released, qh);
        Ok(Buffer {
            pool,
            buffer,
            width,
            height,
            busy: false,
        })
    }

    // the canvas's pixels, rows top to bottom
    fn pixels(&mut self) -> Option<&mut [u32]> {
        // a mapping starts on a page, so its bytes are aligned u32s
        let bytes: &mut [u8] = self.pool.mmap();
        let pixels: &mut [u32] = bytemuck::try_cast_slice_mut(bytes).ok()?;
        pixels.get_mut(..self.width as usize * self.height as usize)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// The data of a buffer, whose release lets the frame that waited for one be drawn.
struct Released;

impl Dispatch2<wl_buffer::WlBuffer, Driver<Window>> for Released {
    fn event(
        &self,
        driver: &mut Driver<Window>,
        buffer: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &Connection,
        _: &QueueHandle<Driver<Window>>,
    ) {
        let window = driver.frontend();
        if let Some(released) = window.buffers.iter_mut().find(|b| &b.buffer == buffer) {
            released.busy = false;
        }
        if std::mem::take(&mut window.waiting) {
            driver.redraw();
        }
    }
}

/// The data of the sync sent after a keyboard's release, whose end lets the keyboard go.
struct Synced;

impl Dispatch2<wl_callback::WlCallback, Driver<Window>> for Synced {
    fn event(
        &self,
        driver: &mut Driver<Window>,
        callback: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &Connection,
        _: &QueueHandle<Driver<Window>>,
    ) {
        let released = &mut driver.frontend().released;
        released.retain(|(done, _)| done != callback);
    }
}

/// The data of the fractional scale and the viewport, which the toolkit leaves to clients.
struct Scaling;

impl Dispatch2<WpFractionalScaleManagerV1, Driver<Window>> for Scaling {
    fn event(
        &self,
        _: &mut Driver<Window>,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<Driver<Window>>,
    ) {
    }
}

impl Dispatch2<WpViewporter, Driver<Window>> for Scaling {
    fn event(
        &self,
        _: &mut Driver<Window>,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<Driver<Window>>,
    ) {
    }
}

impl Dispatch2<WpViewport, Driver<Window>> for Scaling {
    fn event(
        &self,
        _: &mut Driver<Window>,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<Driver<Window>>,
    ) {
    }
}

impl Dispatch2<WpFractionalScaleV1, Driver<Window>> for Scaling {
    fn event(
        &self,
        driver: &mut Driver<Window>,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        connection: &Connection,
        _: &QueueHandle<Driver<Window>>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            driver.frontend().scale120 = Some(scale);
            rescaled(driver, connection);
        }
    }
}

impl CompositorHandler for Driver<Window> {
    fn scale_factor_changed(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        let window = self.frontend();
        // the cursor's surface follows its own outputs
        if surface != window.window.wl_surface() {
            return;
        }
        window.integer_scale = factor;
        if window.viewport.is_none() {
            rescaled(self, connection);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    // on another output at the same scale, the room may still differ, as sway's workspace does
    fn surface_enter(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
        if surface == self.frontend().window.wl_surface() {
            rescaled(self, connection);
        }
    }

    fn surface_leave(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
        if surface == self.frontend().window.wl_surface() {
            rescaled(self, connection);
        }
    }
}

impl OutputHandler for Driver<Window> {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.frontend().outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        screens_changed(self, None);
    }

    // a monitor turned on its side leaves the window other room
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        screens_changed(self, None);
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        screens_changed(self, Some(&output));
    }
}

impl WindowHandler for Driver<Window> {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &xdg_window::Window) {
        self.quit();
    }

    fn configure(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        _: &xdg_window::Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        // the configure says whether the compositor decorates the window; without a decoration
        // manager it never does
        let window = self.frontend();
        let (csd, rows) = (window.csd, window.title_rows());
        window.csd = configure.decoration_mode == DecorationMode::Client;
        window.titled = window.csd && !configure.is_fullscreen();
        let refit = window.csd != csd;
        let rows_changed = window.title_rows() != rows;
        let hug = shown(self);
        let changed = self.frontend().configure(&configure, hug, refit);
        // no resize cursor once the window is fullscreen or tiled
        self.frontend().show_cursor(connection, false);
        if changed || rows_changed {
            self.layout();
        } else {
            // every configure wants a commit, even one that changes nothing; where the buffer
            // on the surface is of an older size, the frame that waits for a buffer answers it
            let window = self.frontend();
            if window.attached == Some(window.sizing.size) {
                window.window.commit();
            }
        }
    }
}

impl ShmHandler for Driver<Window> {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.frontend().shm
    }
}

impl SeatHandler for Driver<Window> {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.frontend().seats
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
        follow_seats(self, None);
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
        follow_seats(self, None);
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        follow_seats(self, Some(&seat));
    }
}

impl KeyboardHandler for Driver<Window> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    // the toolkit stops the key's repeat, and no key is held for the window
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        let window = self.frontend();
        (window.pressed_text, window.serial) = (None, None);
        keys_up(self);
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        let window = self.frontend();
        window.serial = Some(serial);
        window.pressed_text = Some(Held {
            code: event.raw_code,
            text: event.utf8.clone(),
            modifiers: window.modifiers,
        });
        pressed(self, event.keysym, event.utf8.as_deref());
    }

    // a compositor's own repeat, from wl_keyboard 10, whose text the toolkit leaves out: the
    // text the toolkit's own repeat would type
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        let window = self.frontend();
        window.serial = Some(serial);
        let code = xkb::Keycode::new(event.raw_code + 8);
        let now = window.xkb.as_ref().map(|state| state.key_get_utf8(code));
        let (held, modifiers) = (window.pressed_text.as_ref(), window.modifiers);
        let text =
            (event.utf8.clone()).or_else(|| repeat_text(held, event.raw_code, modifiers, now));
        pressed(self, event.keysym, text.as_deref());
    }

    // space's release, which ends moving a shape being drawn in an edit; a held key repeats
    // without one, so a held space keeps it moving
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let window = self.frontend();
        if window
            .pressed_text
            .as_ref()
            .is_some_and(|held| held.code == event.raw_code)
        {
            window.pressed_text = None;
        }
        if let Some(name) = keysyms::key_name(event.keysym) {
            self.event(|session| session.key_released(&Keystroke::new(&name)));
        }
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        raw: RawModifiers,
        layout: u32,
    ) {
        let window = self.frontend();
        window.ctrl = modifiers.ctrl;
        window.modifiers = [raw.depressed, raw.latched, raw.locked, layout];
        if let Some(state) = &mut window.xkb {
            state.update_mask(raw.depressed, raw.latched, raw.locked, 0, 0, layout);
        }
    }

    // a state of the window's own, for a repeated key's symbol
    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let text = keymap.as_string();
        let format = xkb::KEYMAP_FORMAT_TEXT_V1;
        let keymap =
            xkb::Keymap::new_from_string(&context, text, format, xkb::KEYMAP_COMPILE_NO_FLAGS);
        self.frontend().xkb = keymap.map(|keymap| xkb::State::new(&keymap));
    }
}

impl DataSourceHandler for Driver<Window> {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
        _: Option<String>,
    ) {
    }

    // a client reads the copy: the data goes into its pipe as the pipe takes it, in the loop,
    // so a large one keeps no frame waiting; a client that closes the pipe early ends it
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wl_data_source::WlDataSource,
        _: String,
        pipe: WritePipe,
    ) {
        let window = self.frontend();
        let Some((_, data)) = (window.source.as_ref()).filter(|(s, _)| s.inner() == source) else {
            return;
        };
        let data = Rc::clone(data);
        if rustix::fs::fcntl_setfl(&pipe, rustix::fs::OFlags::NONBLOCK).is_err() {
            return;
        }
        let mut sent = 0;
        let _ = driver::insert(&window.handle, pipe, move |(), pipe, _| {
            let mut pipe: &File = pipe;
            match clipboard::feed(&mut pipe, &data, &mut sent, clipboard::PER_WAKEUP) {
                clipboard::Fed::More => PostAction::Continue,
                clipboard::Fed::Done | clipboard::Fed::Gone => PostAction::Remove,
            }
        });
    }

    // another copy, of this viewer's or another client's, took the selection: the source goes as
    // it drops
    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wl_data_source::WlDataSource,
    ) {
        let window = self.frontend();
        if window
            .source
            .as_ref()
            .is_some_and(|(s, _)| s.inner() == source)
        {
            window.source = None;
        }
    }

    fn dnd_dropped(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
    ) {
    }

    fn dnd_finished(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
    ) {
    }

    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
        _: wl_data_device_manager::DndAction,
    ) {
    }
}

// the window offers the clipboard, and takes nothing from it nor from a drag
impl DataDeviceHandler for Driver<Window> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {}

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
    ) {
    }

    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
    ) {
    }

    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
    ) {
    }
}

impl DataOfferHandler for Driver<Window> {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: wl_data_device_manager::DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: wl_data_device_manager::DndAction,
    ) {
    }
}

impl PopupHandler for Driver<Window> {
    // the popup's place and size, trimmed to the work area or not
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        configure: PopupConfigure,
    ) {
        let window = self.frontend();
        if window.probe.as_ref() == Some(popup) {
            let (x, y) = configure.position;
            window.probed = Some((x, y, configure.width, configure.height));
            probe_answered(self);
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if self.frontend().probe.as_ref() == Some(popup) {
            probe_answered(self);
        }
    }
}

impl PointerHandler for Driver<Window> {
    fn pointer_frame(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        use wl_pointer::AxisSource;
        let button = |code| match code {
            BTN_LEFT => Some(Button::Left),
            BTN_RIGHT => Some(Button::Right),
            _ => None,
        };
        let mut axes = Axes::default();
        for event in events {
            // surface-local logical units, y downward, as the session takes them
            let at = Point {
                x: event.position.0,
                y: event.position.1,
            };
            // the session counts from below the title bar
            let window = self.frontend();
            let canvas = Point {
                y: at.y - window.title_rows(),
                ..at
            };
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    window.pointer_at = at;
                    window.pointer_in = true;
                    let entering = matches!(event.kind, PointerEventKind::Enter { .. });
                    window.show_cursor(connection, entering);
                    self.event(|session| session.pointer_moved(canvas));
                }
                // the cursor is the compositor's until the next enter
                PointerEventKind::Leave { .. } => {
                    window.pointer_in = false;
                    end_drag(self);
                }
                PointerEventKind::Press {
                    button: code,
                    time,
                    serial,
                } => {
                    let Some(button) = button(code) else {
                        continue;
                    };
                    let right = button == Button::Right;
                    match window.decoration_pressed(right, at, time, serial) {
                        Some(Press::Close) => self.quit(),
                        Some(_) => {}
                        None => self.event(|session| session.press(button, canvas, time)),
                    }
                }
                PointerEventKind::Release { button: code, .. } => {
                    if let Some(button) = button(code) {
                        self.event(|session| session.release(button, canvas));
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    source,
                    ..
                } => {
                    axes.dx += horizontal.absolute;
                    axes.dy += vertical.absolute;
                    axes.discrete += vertical.discrete;
                    axes.value120 += vertical.value120;
                    axes.smooth |=
                        matches!(source, Some(AxisSource::Finger | AxisSource::Continuous));
                }
            }
        }
        let grid = self.viewer().thumbnail_mode();
        if let Some(scroll) = input::scroll(axes, &mut self.frontend().wheel_rest, grid) {
            self.event(|session| session.scroll(scroll));
        }
    }
}

impl ProvidesRegistryState for Driver<Window> {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.frontend().registry
    }

    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Driver<Window>);
delegate_dispatch2!(Driver<Window>);

#[cfg(test)]
mod tests {
    use super::*;
    use smithay_client_toolkit::reexports::client::backend::protocol::ProtocolError;

    #[test]
    fn only_an_error_the_connection_holds_is_a_lost_connection() {
        let poll = || io::Error::other("poll failed");
        assert_eq!(lost_connection(None, poll()).to_string(), "poll failed");
        let pipe = WaylandError::Io(io::Error::from(io::ErrorKind::BrokenPipe));
        assert_eq!(
            lost_connection(Some(pipe), poll()).to_string(),
            "lost the connection to the Wayland compositor: broken pipe"
        );
        let protocol = WaylandError::Protocol(ProtocolError {
            code: 1,
            object_id: 1,
            object_interface: "wl_display".to_owned(),
            message: "closed".to_owned(),
        });
        assert_eq!(
            lost_connection(Some(protocol), poll()).to_string(),
            "lost the connection to the Wayland compositor: Protocol error 1 on object \
             wl_display@1: closed"
        );
    }

    #[test]
    fn a_title_that_comes_with_the_fonts_fits_the_window_again_once() {
        let redraw = Request::Redraw { bar_only: false };
        assert!(refits(true, &redraw), "with the bar hidden no fit comes");
        assert!(!refits(false, &redraw), "no title, no rows taken");
        let fit = Request::FitWindow {
            image: Size {
                width: 300.0,
                height: 200.0,
            },
            bar: 0.0,
        };
        assert!(!refits(true, &fit), "the fit at hand counts the title");
    }

    #[test]
    fn a_frame_without_a_buffer_waits_only_for_a_release_that_will_come() {
        assert!(!waits(0), "no buffer held, as on the first frame");
        assert!(waits(1));
        assert!(waits(BUFFERS));
    }

    #[test]
    fn a_modifier_alone_is_nothing_and_a_key_without_a_name_ends_a_prefix() {
        let x = Keysym::new(0x78);
        assert_eq!(keystroke(x, true), Some(Some(Keystroke::ctrl("x"))));
        assert_eq!(keystroke(x, false), Some(Some(Keystroke::new("x"))));
        let control = Keysym::new(0xffe3);
        assert_eq!(keystroke(control, true), None, "Control_L");
        assert_eq!(
            keystroke(Keysym::new(0xfe03), false),
            None,
            "ISO_Level3_Shift"
        );
        assert_eq!(keystroke(Keysym::new(0), false), Some(None), "no symbol");
    }

    /// The text the toolkit gives each key of `keysyms` fed through a compose table of
    /// `table`, as its keyboard computes it for a press (smithay-client-toolkit 0.21.1,
    /// src/seat/keyboard/mod.rs): a sequence's steps none, its end the composed text, and a
    /// key of no sequence its own character.
    fn composed(table: &str, keysyms: &[Keysym]) -> Vec<Option<String>> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let (format, flags) = (xkb::compose::FORMAT_TEXT_V1, xkb::compose::COMPILE_NO_FLAGS);
        let table = xkb::compose::Table::new_from_buffer(&context, table, "C", format, flags);
        let mut state = xkb::compose::State::new(&table.unwrap(), xkb::compose::STATE_NO_FLAGS);
        let own = |keysym: Keysym| keysyms::typed(keysym).or(Some(String::new()));
        (keysyms.iter())
            .map(|&keysym| match state.feed(keysym) {
                xkb::FeedResult::Ignored => None,
                xkb::FeedResult::Accepted => match state.status() {
                    xkb::Status::Composed => state.utf8(),
                    xkb::Status::Nothing => own(keysym),
                    _ => None,
                },
            })
            .collect()
    }

    #[test]
    fn a_compose_sequence_types_its_character_alone() {
        let table = "<Multi_key> <o> <e> : \"œ\"\n<Multi_key> <x> <x> : \"×\"\n";
        let (compose, o, e, x) = (
            Keysym::new(0xff20),
            Keysym::new(0x6f),
            Keysym::new(0x65),
            Keysym::new(0x78),
        );
        for (keys, character) in [([compose, o, e], "œ"), ([compose, x, x], "×")] {
            let text = composed(table, &keys);
            let typed: Vec<_> = (keys.iter().zip(&text))
                .map(|(&keysym, text)| typing(keysym, text.as_deref()))
                .collect();
            // Multi_key goes by its name, which the line types nothing of; the steps type
            // nothing; the last key the character
            assert_eq!(
                typed,
                [None, Some(None), Some(Some(character.to_owned()))],
                "{character}"
            );
        }
        // a key of no sequence types its character, and Return goes by its name
        assert_eq!(
            typing(o, composed(table, &[o])[0].as_deref()),
            Some(Some("o".to_owned()))
        );
        assert_eq!(typing(Keysym::new(0xff0d), Some("\r")), None);
        assert_eq!(typing(Keysym::new(0xff51), None), None, "Left");
    }

    #[test]
    fn a_compositors_repeat_types_what_the_press_typed() {
        let (none, shift, control) = ([0; 4], [1, 0, 0, 0], [4, 0, 0, 0]);
        let held = |text: &str, modifiers| Held {
            code: 26,
            text: Some(text.to_owned()),
            modifiers,
        };
        let now = |text: &str| Some(text.to_owned());
        // a composed é, its modifiers as they were
        let composed = held("é", none);
        assert_eq!(
            repeat_text(Some(&composed), 26, none, now("e")).as_deref(),
            Some("é")
        );
        // another key's repeat, or one with no press seen, types its own text now
        assert_eq!(
            repeat_text(Some(&composed), 27, none, now("e")).as_deref(),
            Some("e")
        );
        assert_eq!(repeat_text(None, 26, none, now("e")).as_deref(), Some("e"));
        // Shift released with a held: a, as it is now
        let shifted = held("A", shift);
        assert_eq!(
            repeat_text(Some(&shifted), 26, shift, now("A")).as_deref(),
            Some("A")
        );
        assert_eq!(
            repeat_text(Some(&shifted), 26, none, now("a")).as_deref(),
            Some("a")
        );
        // Control released with a held: a, where the press's text was Control-a's
        let controlled = held("\u{1}", control);
        assert_eq!(
            repeat_text(Some(&controlled), 26, none, now("a")).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn a_key_types_one_character_and_one_of_control_none() {
        assert_eq!(
            typed(Some("é")).as_deref(),
            Some("é"),
            "composed, or of a layout"
        );
        assert_eq!(typed(Some(" ")).as_deref(), Some(" "));
        assert_eq!(typed(Some("\r")), None, "Return");
        assert_eq!(typed(Some("\u{8}")), None, "BackSpace");
        assert_eq!(typed(Some("")), None, "a dead key");
        assert_eq!(typed(Some("ab")), None, "several");
        assert_eq!(typed(None), None);
    }
}
