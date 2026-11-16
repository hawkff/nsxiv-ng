//! The window's size and scale: the compositor's size where it gives one, the viewer's where
//! it leaves a side open.

use nsxiv_ng_core::viewer::grown_limit;
use nsxiv_ng_core::{Rect, Size};
use smithay_client_toolkit::reexports::csd_frame::WindowState;

/// The room a window has where nothing tells it, as before any output is known.
const NO_OUTPUT: Size = Size {
    width: 1920.0,
    height: 1080.0,
};

/// How many of the sizes the viewer picked itself it keeps, for telling sway's answers.
const PICKED: usize = 8;

/// The states in which the compositor's size binds; without them it is a hint.
pub(super) const HELD: WindowState = WindowState::MAXIMIZED
    .union(WindowState::FULLSCREEN)
    .union(WindowState::RESIZING)
    .union(WindowState::TILED);

/// What the viewer knows of its compositor's ways, from the session's environment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Desktop {
    /// sway, whose IPC places the viewer's floating window.
    pub sway: bool,
    /// COSMIC, which marks floating windows as tiled when it rounds their corners itself.
    pub cosmic: bool,
    /// KDE, where the viewer's KWin script keeps a growing window on screen.
    pub kde: bool,
    /// GNOME, whose mutter keeps a growing window on screen, and leaves a popup the size it
    /// asked for, so the room probe finds nothing.
    pub gnome: bool,
    /// labwc and Wayfire keep the top-left corner of a window that grows itself.
    pub keeps_top_left: bool,
}

impl Desktop {
    /// The compositor of a session whose `XDG_CURRENT_DESKTOP` is `desktop`, its names
    /// spelled as sessions spell them (Wayfire, wayfire, ubuntu:GNOME), and with `sway` where
    /// sway's IPC socket is named.
    pub fn new(desktop: &str, sway: bool) -> Desktop {
        let desktop = desktop.to_lowercase();
        let named = |name: &str| desktop.split(':').any(|d| d == name);
        Desktop {
            sway,
            cosmic: named("cosmic"),
            kde: named("kde"),
            gnome: named("gnome"),
            keeps_top_left: named("labwc") || named("wayfire"),
        }
    }

    /// The states in which a configure's size binds. COSMIC marks floating windows as tiled
    /// too, so there only its size tells a tile: one the viewer did not pick binds. Hyprland
    /// marks every window as tiled and maximized, and keeps the size it gave a window that
    /// resizes itself, so there every size binds.
    pub fn binding(self) -> WindowState {
        match self.cosmic {
            true => WindowState::MAXIMIZED
                .union(WindowState::FULLSCREEN)
                .union(WindowState::RESIZING),
            false => HELD,
        }
    }

    /// Whether a window that grows itself probes the work area around it first: sway and KDE
    /// place it, and mutter keeps it on screen and leaves such a popup untrimmed.
    pub fn probes_room(self) -> bool {
        !self.sway && !self.kde && !self.gnome
    }
}

/// What the window knows of the work area around it, as it grows itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Room {
    /// The compositor keeps the window on screen, or the viewer places it.
    Kept,
    /// Unknown: the window grows once a probe found it.
    Unknown,
    /// The work area relative to the window's top-left corner, and whether the compositor
    /// keeps that corner as the window grows.
    Found(Rect, bool),
}

/// What the size rule did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    Same,
    Changed,
    /// The window would grow into a room that is unknown: it keeps its size until a probe
    /// found the room, then the rule runs again.
    Probe,
}

/// An output, for the room a window may take on it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Output {
    /// Its current mode's pixels, before its rotation.
    pub mode: (i32, i32),
    /// A quarter turn, as on a portrait monitor.
    pub turned: bool,
    pub scale: i32,
    /// Its size in logical units, its scale and turn in it, where the compositor tells it
    /// through xdg-output.
    pub logical: Option<(i32, i32)>,
}

/// The pixels to a logical unit: the fractional scale in 120ths where the window has a
/// viewport, else the largest integer scale of the outputs it is on.
pub(super) fn device_scale(fractional: Option<u32>, integer: i32) -> f64 {
    match fractional {
        Some(scale) => f64::from(scale) / 120.0,
        None => f64::from(integer.max(1)),
    }
}

/// Where the window may grow, in logical units: the compositor's bounds, else the largest
/// output, at the logical size the compositor gives it. An output without one has its pixels
/// divided by the window's fractional scale where one other than 1 came, since wl_output
/// rounds 1.5 up to 2, else by the output's own scale.
pub(super) fn max_size(
    bounds: Option<(u32, u32)>,
    outputs: &[Output],
    fractional: Option<f64>,
) -> Size {
    if let Some((width, height)) = bounds {
        return Size {
            width: f64::from(width),
            height: f64::from(height),
        };
    }
    outputs
        .iter()
        .map(|output| {
            if let Some((width, height)) = output.logical {
                return Size {
                    width: f64::from(width),
                    height: f64::from(height),
                };
            }
            // shortcut: an output without a logical size is divided by the window's own
            // fractional scale, right for the output the window is on and wrong for one at
            // another scale, which may get more room than it has; every compositor with
            // fractional scaling has xdg-output, and Weston and gamescope have neither, so this
            // stays until one has fractional scaling without xdg-output, when the outputs the
            // window is on must be followed and the window fitted again as it moves
            let scale = fractional.unwrap_or(f64::from(output.scale.max(1)));
            let (width, height) = match output.turned {
                true => (output.mode.1, output.mode.0),
                false => output.mode,
            };
            Size {
                width: f64::from(width) / scale,
                height: f64::from(height) / scale,
            }
        })
        .max_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))
        .unwrap_or(NO_OUTPUT)
}

/// The image the window fits, and hugs where the compositor gives one side and leaves the
/// other open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Hug {
    /// The image as shown, in logical units at 100%.
    pub image: Size,
    /// The rows above and below it that are not canvas: the title bar's and the bar's.
    pub rows: f64,
    /// The zoom stays at 100% or less, as the default scale mode keeps it.
    pub down: bool,
}

/// The window's size as configures and the viewer change it.
#[derive(Debug)]
pub(super) struct Sizing {
    /// The size the last configure gave, 0 for a side the viewer picks.
    requested: (u32, u32),
    /// The size the viewer asks for where the compositor leaves it open.
    wanted: Size,
    /// The last sizes the viewer picked itself, newest last.
    picked: Vec<(u32, u32)>,
    /// The window in logical units, nothing before the first configure.
    pub size: (u32, u32),
    /// The last configure's states had the window fullscreen.
    pub fullscreen: bool,
    /// The size the window had before a state that held its size, while one holds it; then,
    /// until the size rule ran, that size, where the compositor puts the window back.
    before_held: Option<(u32, u32)>,
    restored: Option<(u32, u32)>,
    /// The size the last change started from, as the compositor shows the window: the size
    /// before, or after a state that held the size, the size before that state.
    pub from: (u32, u32),
}

impl Sizing {
    pub fn new(wanted: Size) -> Sizing {
        Sizing {
            requested: (0, 0),
            wanted,
            picked: Vec::new(),
            size: (0, 0),
            fullscreen: false,
            before_held: None,
            restored: None,
            from: (0, 0),
        }
    }

    /// Takes a configure: its size, 0 for a side it leaves open, and whether its states bind
    /// that size (maximized, fullscreen, resizing or tiled), say that the user drags an edge,
    /// and put the window in fullscreen. True when it left fullscreen, which fits the window to
    /// the image again.
    pub fn configure(
        &mut self,
        requested: (u32, u32),
        held: bool,
        resizing: bool,
        fullscreen: bool,
    ) -> bool {
        self.requested = requested;
        // fullscreen, maximized or tiled ends with the window put back as it was; a drag
        // leaves it as the user left it
        if resizing {
            self.before_held = None;
        } else if held {
            self.before_held.get_or_insert(self.size);
        } else {
            self.restored = self.before_held.take();
        }
        if resizing && requested.0 > 0 && requested.1 > 0 {
            // a dragged size lasts until the next image, as on X11
            self.wanted = Size {
                width: f64::from(requested.0),
                height: f64::from(requested.1),
            };
            self.remember(requested);
        } else if !held && self.picked.contains(&requested) {
            // sway answers a floating window's own resize with a configure of that size, late
            // when keys come fast; such an answer leaves the size to the viewer.
            // shortcut: a compositor resize to one of the last eight picked sizes reads as an
            // answer too, track configure serials if that ever matters
            self.requested = (0, 0);
        }
        let left = self.fullscreen && !fullscreen;
        self.fullscreen = fullscreen;
        left
    }

    /// The size by the rule, within `max`, hugging `hug`'s image where one side is open, and,
    /// where the window grows itself, within the `room` around it.
    pub fn apply(&mut self, max: Size, hug: Option<Hug>, room: Room) -> Applied {
        let restored = self.restored.take();
        let (mut width, mut height) = (f64::from(self.requested.0), f64::from(self.requested.1));
        if let Some(Hug { image, rows, down }) = hug
            && image.width > 0.0
            && image.height > 0.0
        {
            // one side given and one open, as in a niri column: hug the image at the zoom it
            // will get
            let up_to = |zoom: f64| if down { zoom.min(1.0) } else { zoom };
            if width == 0.0 && height > rows {
                let zoom = up_to((height - rows) / image.height);
                width = max.width.min((image.width * zoom).round());
            }
            if height == 0.0 && width > 0.0 {
                let zoom = up_to(width / image.width);
                height = max.height.min((image.height * zoom).round() + rows);
            }
        }
        // the 800x600 default overflows 1280x800 at scale 1.5
        if width == 0.0 {
            width = self.wanted.width.min(max.width.floor());
        }
        if height == 0.0 {
            height = self.wanted.height.min(max.height.floor());
        }
        let mut size = ((width as u32).max(1), (height as u32).max(1));
        let grows = size.0 > self.size.0 || size.1 > self.size.1;
        if self.requested == (0, 0) && self.size.0 > 0 && grows {
            match room {
                Room::Kept => {}
                Room::Unknown => {
                    self.restored = restored;
                    return Applied::Probe;
                }
                Room::Found(area, keeps_top_left) => {
                    let limit = |extent: u32, lo: f64, hi: f64| {
                        let limit = grown_limit(i64::from(extent), lo, hi, keeps_top_left);
                        u32::try_from(limit.max(1)).unwrap_or(u32::MAX)
                    };
                    size.0 = size.0.min(limit(self.size.0, area.x, area.max_x()));
                    size.1 = size.1.min(limit(self.size.1, area.y, area.max_y()));
                }
            }
        }
        if size == self.size {
            return Applied::Same;
        }
        self.from = restored.unwrap_or(self.size);
        self.size = size;
        if self.requested == (0, 0) {
            self.remember(size);
        }
        Applied::Changed
    }

    /// The compositor leaves both sides to the viewer, as for a floating window.
    pub fn open(&self) -> bool {
        self.requested == (0, 0)
    }

    /// Asks for the size that shows `image` whole within `max`, at 100% at most, with `rows`
    /// below it.
    pub fn fit(&mut self, image: Size, rows: f64, max: Size) {
        if !(image.width > 0.0 && image.height > 0.0) {
            return;
        }
        let fit = (max.width / image.width)
            .min((max.height - rows) / image.height)
            .min(1.0);
        self.wanted = Size {
            width: (image.width * fit).round(),
            height: (image.height * fit).round() + rows,
        };
    }

    /// Asks for `wanted` again, as for the grid.
    pub fn reset(&mut self, wanted: Size) {
        self.wanted = wanted;
    }

    /// The window in logical units, or before the first configure the size it asks for.
    pub fn window(&self) -> Size {
        let side = |given: u32, wanted: f64| match given {
            0 => wanted.trunc(),
            given => f64::from(given),
        };
        Size {
            width: side(self.size.0, self.wanted.width),
            height: side(self.size.1, self.wanted.height),
        }
    }

    fn remember(&mut self, size: (u32, u32)) {
        self.picked.push(size);
        if self.picked.len() > PICKED {
            self.picked.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    const SCREEN: Size = Size {
        width: 1280.0,
        height: 800.0,
    };

    fn size(width: f64, height: f64) -> Size {
        Size { width, height }
    }

    fn hug(width: f64, height: f64, down: bool) -> Option<Hug> {
        Some(Hug {
            image: size(width, height),
            rows: 20.0,
            down,
        })
    }

    // a sizing after a configure of `requested` with no state
    fn configured(requested: (u32, u32)) -> Sizing {
        let mut sizing = Sizing::new(DEFAULT);
        sizing.configure(requested, false, false, false);
        sizing
    }

    #[test]
    fn the_compositors_size_binds_where_it_gives_one() {
        let mut sizing = configured((1000, 700));
        assert_eq!(
            sizing.apply(SCREEN, hug(300.0, 200.0, true), Room::Kept),
            Applied::Changed
        );
        assert_eq!(sizing.size, (1000, 700), "neither the image nor the screen");
        assert_eq!(
            sizing.apply(SCREEN, None, Room::Kept),
            Applied::Same,
            "the same size changes nothing"
        );
    }

    #[test]
    fn the_viewers_size_where_both_sides_are_open_within_the_screen() {
        let mut sizing = configured((0, 0));
        assert_eq!(sizing.window(), DEFAULT, "before the first size");
        assert_eq!(
            sizing.apply(SCREEN, hug(300.0, 200.0, true), Room::Kept),
            Applied::Changed
        );
        assert_eq!(sizing.size, (800, 600), "no side to hug");
        // the 800x600 default overflows 1280x800 at scale 1.5
        let small = size(1280.0 / 1.5, 800.0 / 1.5);
        assert_eq!(sizing.apply(small, None, Room::Kept), Applied::Changed);
        assert_eq!(sizing.size, (800, 533));
        // a fraction of a unit asked for
        let mut sizing = Sizing::new(size(640.7, 480.2));
        assert_eq!(sizing.window(), size(640.0, 480.0));
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (640, 480));
    }

    #[test]
    fn a_given_width_hugs_the_image_in_height() {
        let mut sizing = configured((600, 0));
        // 600 of 300 would zoom to 200%, which the default scale mode keeps at 100%
        sizing.apply(SCREEN, hug(300.0, 200.0, true), Room::Kept);
        assert_eq!(sizing.size, (600, 220), "the image's height and the bar");
        let mut sizing = configured((600, 0));
        sizing.apply(SCREEN, hug(300.0, 200.0, false), Room::Kept);
        assert_eq!(sizing.size, (600, 420), "a fit mode zooms up");
        let mut sizing = configured((600, 0));
        sizing.apply(SCREEN, hug(300.0, 2000.0, true), Room::Kept);
        assert_eq!(sizing.size, (600, 800), "within the screen");
    }

    #[test]
    fn a_given_height_hugs_the_image_in_width() {
        let mut sizing = configured((0, 420));
        sizing.apply(SCREEN, hug(300.0, 200.0, false), Room::Kept);
        assert_eq!(
            sizing.size,
            (600, 420),
            "the bar's rows are not the image's"
        );
        let mut sizing = configured((0, 420));
        sizing.apply(SCREEN, hug(300.0, 200.0, true), Room::Kept);
        assert_eq!(sizing.size, (300, 420), "up to 100%");
        let mut sizing = configured((0, 600));
        sizing.apply(SCREEN, hug(4000.0, 200.0, true), Room::Kept);
        assert_eq!(sizing.size, (1280, 600), "within the screen");
        // a height with no room for the image under the bar hugs nothing
        let mut sizing = configured((0, 20));
        sizing.apply(SCREEN, hug(300.0, 200.0, true), Room::Kept);
        assert_eq!(sizing.size, (800, 20));
        // nor does a side without an image, as in the grid
        let mut sizing = configured((0, 420));
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (800, 420));
    }

    #[test]
    fn a_configure_of_a_size_the_viewer_picked_leaves_the_size_to_it() {
        let mut sizing = configured((0, 0));
        sizing.fit(size(300.0, 200.0), 20.0, SCREEN);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (300, 220));
        // sway's late answer to that size, after the viewer moved on to another image
        sizing.fit(size(500.0, 400.0), 20.0, SCREEN);
        sizing.configure((300, 220), false, false, false);
        assert_eq!(sizing.apply(SCREEN, None, Room::Kept), Applied::Changed);
        assert_eq!(sizing.size, (500, 420), "the viewer's own size");
        // in a tile or maximized the same size binds
        sizing.configure((300, 220), true, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (300, 220));
        // a size it never picked binds
        sizing.configure((310, 220), false, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (310, 220));
    }

    #[test]
    fn only_the_last_eight_picked_sizes_count_as_answers() {
        let mut sizing = configured((0, 0));
        for width in 100..109 {
            sizing.reset(size(f64::from(width), 100.0));
            sizing.apply(SCREEN, None, Room::Kept);
        }
        sizing.reset(DEFAULT);
        sizing.configure((100, 100), false, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (100, 100), "the oldest is forgotten");
        sizing.configure((101, 100), false, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (800, 600));
    }

    #[test]
    fn an_open_size_shrinks_to_an_output_that_comes_later_without_an_image() {
        // the first configure, before the outputs, within the stand-in's room
        let mut sizing = configured((0, 0));
        assert_eq!(
            sizing.apply(max_size(None, &[], None), None, Room::Kept),
            Applied::Changed
        );
        assert_eq!(sizing.size, (800, 600));
        let small = Output {
            mode: (1280, 800),
            turned: false,
            scale: 2,
            logical: None,
        };
        assert_eq!(
            sizing.apply(max_size(None, &[small], None), None, Room::Kept),
            Applied::Changed
        );
        assert_eq!(sizing.size, (640, 400), "as in the grid, or with -g");
    }

    #[test]
    fn a_window_that_grows_itself_waits_for_the_room_and_grows_within_it() {
        let mut sizing = configured((0, 0));
        sizing.reset(size(200.0, 220.0));
        assert_eq!(
            sizing.apply(SCREEN, None, Room::Unknown),
            Applied::Changed,
            "the first size needs no room"
        );
        sizing.fit(size(1200.0, 700.0), 20.0, SCREEN);
        assert_eq!(sizing.apply(SCREEN, None, Room::Unknown), Applied::Probe);
        assert_eq!(sizing.size, (200, 220), "kept until the probe answers");
        // at 1000,420 on a 1280x720 screen, which keeps the top-left corner: 80 more each way
        let corner = Rect {
            x: -1000.0,
            y: -420.0,
            width: 1280.0,
            height: 720.0,
        };
        let mut grown = configured((0, 0));
        grown.reset(size(200.0, 220.0));
        grown.apply(SCREEN, None, Room::Kept);
        grown.fit(size(1200.0, 700.0), 20.0, SCREEN);
        assert_eq!(
            grown.apply(SCREEN, None, Room::Found(corner, true)),
            Applied::Changed
        );
        assert_eq!(grown.size, (280, 300));
        // near the left edge, where the compositor may keep the center: twice the room before
        let left = Rect {
            x: -30.0,
            y: -500.0,
            width: 1000.0,
            height: 1000.0,
        };
        let mut centered = configured((0, 0));
        centered.reset(size(200.0, 220.0));
        centered.apply(SCREEN, None, Room::Kept);
        centered.fit(size(1200.0, 700.0), 20.0, SCREEN);
        centered.apply(SCREEN, None, Room::Found(left, false));
        assert_eq!(centered.size, (260, 500));
        let mut kept = configured((0, 0));
        kept.reset(size(200.0, 220.0));
        kept.apply(SCREEN, None, Room::Kept);
        kept.fit(size(1200.0, 700.0), 20.0, SCREEN);
        kept.apply(SCREEN, None, Room::Found(left, true));
        assert_eq!(
            kept.size,
            (970, 500),
            "the corner kept, all the room after it"
        );
        // a window parked past the room's edge, as a user may leave it, grows freely
        let past = Rect { x: 10.0, ..left };
        let mut parked = configured((0, 0));
        parked.reset(size(200.0, 220.0));
        parked.apply(SCREEN, None, Room::Kept);
        parked.fit(size(1200.0, 700.0), 20.0, SCREEN);
        parked.apply(SCREEN, None, Room::Found(past, false));
        assert_eq!(parked.size, (1200, 500));
    }

    #[test]
    fn only_a_window_that_grows_itself_needs_the_room() {
        let mut sizing = configured((0, 0));
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.reset(size(300.0, 200.0));
        assert_eq!(
            sizing.apply(SCREEN, None, Room::Unknown),
            Applied::Changed,
            "it shrinks"
        );
        sizing.configure((900, 700), false, false, false);
        assert_eq!(
            sizing.apply(SCREEN, None, Room::Unknown),
            Applied::Changed,
            "the compositor gives the size"
        );
    }

    #[test]
    fn the_session_names_its_compositor_and_its_ways() {
        let gnome = Desktop::new("ubuntu:GNOME", false);
        assert!(gnome.gnome && !gnome.probes_room());
        assert!(!Desktop::new("KDE", false).probes_room());
        assert!(!Desktop::new("sway", true).probes_room());
        for desktop in ["labwc:wlroots", "Wayfire", "wayfire"] {
            let stacking = Desktop::new(desktop, false);
            assert!(
                stacking.keeps_top_left && stacking.probes_room(),
                "{desktop}"
            );
        }
        for desktop in ["niri", "river", "COSMIC", ""] {
            let other = Desktop::new(desktop, false);
            assert!(!other.keeps_top_left && other.probes_room(), "{desktop}");
        }
    }

    #[test]
    fn a_tile_binds_its_size_except_on_cosmic_where_only_the_size_tells_one() {
        let tiled = WindowState::TILED_LEFT | WindowState::TILED_TOP;
        let cosmic = Desktop::new("COSMIC", false);
        assert!(!tiled.intersects(cosmic.binding()));
        assert!(WindowState::MAXIMIZED.intersects(cosmic.binding()));
        assert!(WindowState::RESIZING.intersects(cosmic.binding()));
        let hyprland = Desktop::new("Hyprland", false);
        assert!((tiled | WindowState::MAXIMIZED).intersects(hyprland.binding()));
        assert!(tiled.intersects(Desktop::new("sway", true).binding()));
        // on COSMIC, a tiled configure of a size the viewer picked leaves the size to it, and
        // one of another size binds
        let mut sizing = configured((0, 0));
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.reset(size(300.0, 200.0));
        let held = tiled.intersects(cosmic.binding());
        sizing.configure((800, 600), held, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (300, 200));
        sizing.configure((640, 480), held, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (640, 480));
    }

    // a floating window of 400x322, which went fullscreen
    fn fullscreen() -> Sizing {
        let mut sizing = configured((0, 0));
        sizing.reset(size(400.0, 322.0));
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.configure((1280, 800), true, false, true);
        sizing.apply(SCREEN, None, Room::Kept);
        sizing
    }

    #[test]
    fn a_window_put_back_from_fullscreen_starts_from_the_size_before() {
        let mut sizing = fullscreen();
        assert_eq!(sizing.from, (400, 322), "into fullscreen from its size");
        sizing.configure((0, 0), false, false, false);
        assert_eq!(sizing.apply(SCREEN, None, Room::Kept), Applied::Changed);
        assert_eq!(
            (sizing.from, sizing.size),
            ((400, 322), (400, 322)),
            "the same image: where it was, so nothing to move"
        );
        let mut sizing = fullscreen();
        sizing.fit(size(600.0, 400.0), 20.0, SCREEN);
        sizing.configure((0, 0), false, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(
            (sizing.from, sizing.size),
            ((400, 322), (600, 420)),
            "another image: placed from the size before fullscreen"
        );
        // a maximized window or a tile is put back as well, and only once
        let mut sizing = configured((0, 0));
        sizing.reset(size(400.0, 322.0));
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.configure((1280, 780), true, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.configure((0, 0), false, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.from, (400, 322));
        sizing.reset(size(300.0, 200.0));
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!((sizing.from, sizing.size), ((400, 322), (300, 200)));
        sizing.reset(size(250.0, 150.0));
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.from, (300, 200), "the size before, once put back");
    }

    #[test]
    fn a_plain_resize_or_one_after_a_drag_starts_from_the_size_before() {
        let mut sizing = configured((0, 0));
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.reset(size(300.0, 200.0));
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.from, (800, 600));
        // a drag leaves the window where the user left it
        sizing.configure((700, 500), true, true, false);
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.configure((0, 0), false, false, false);
        sizing.apply(SCREEN, None, Room::Kept);
        sizing.fit(size(200.0, 100.0), 20.0, SCREEN);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!((sizing.from, sizing.size), ((700, 500), (200, 120)));
    }

    #[test]
    fn a_dragged_size_lasts_until_the_next_image() {
        let mut sizing = configured((0, 0));
        sizing.configure((700, 500), true, true, false);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (700, 500));
        // the drag ends, and the compositor leaves the size open again
        sizing.configure((0, 0), false, false, false);
        assert_eq!(sizing.apply(SCREEN, None, Room::Kept), Applied::Same);
        assert_eq!(sizing.size, (700, 500));
        sizing.fit(size(300.0, 200.0), 20.0, SCREEN);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (300, 220));
    }

    #[test]
    fn fitting_keeps_the_image_whole_on_the_screen_at_100_percent_at_most() {
        let mut sizing = configured((0, 0));
        sizing.fit(size(2560.0, 1600.0), 20.0, SCREEN);
        sizing.apply(SCREEN, None, Room::Kept);
        // the height decides: 780 rows for 1600 pixels
        assert_eq!(sizing.size, (1248, 800));
        sizing.fit(size(0.0, 200.0), 20.0, SCREEN);
        sizing.apply(SCREEN, None, Room::Kept);
        assert_eq!(sizing.size, (1248, 800), "no image, no fit");
        // fullscreen, then out of it: the window fits the image again
        assert!(!sizing.configure((1280, 800), true, false, true));
        assert!(sizing.fullscreen);
        sizing.apply(SCREEN, None, Room::Kept);
        assert!(sizing.configure((0, 0), false, false, false));
        assert!(!sizing.fullscreen);
    }

    #[test]
    fn the_room_is_the_bounds_else_the_largest_output_in_logical_units() {
        let output = |mode, turned, scale| Output {
            mode,
            turned,
            scale,
            logical: None,
        };
        let outputs = [
            output((2560, 1600), false, 2),
            output((1920, 1080), true, 1),
        ];
        assert_eq!(
            max_size(Some((1000, 700)), &outputs, None),
            size(1000.0, 700.0)
        );
        assert_eq!(
            max_size(None, &outputs, None),
            size(1080.0, 1920.0),
            "on its side, and larger than 1280 x 800"
        );
        assert_eq!(
            max_size(None, &outputs, Some(1.25)),
            size(2048.0, 1280.0),
            "a fractional scale for every output"
        );
        assert_eq!(max_size(None, &[], None), NO_OUTPUT);
        assert_eq!(
            max_size(None, &[output((1280, 800), false, 0)], None),
            SCREEN,
            "no scale is 1"
        );
    }

    #[test]
    fn an_output_that_tells_its_logical_size_gives_it_whatever_the_windows_scale() {
        let output = |mode, turned, scale, logical| Output {
            mode,
            turned,
            scale,
            logical: Some(logical),
        };
        let outputs = [
            output((1920, 1080), false, 2, (1280, 720)),
            output((3840, 2160), false, 2, (1920, 1080)),
        ];
        // the window's scale of 1.5 would make the second 2560 x 1440, past either screen
        assert_eq!(max_size(None, &outputs, Some(1.5)), size(1920.0, 1080.0));
        let without = outputs.map(|o| Output { logical: None, ..o });
        assert_eq!(
            max_size(None, &without, Some(1.5)),
            size(2560.0, 1440.0),
            "without logical sizes, the rule as before"
        );
        let portrait = output((2560, 1600), true, 2, (800, 1280));
        assert_eq!(
            max_size(None, &[portrait], None),
            size(800.0, 1280.0),
            "a logical size has the turn in it already"
        );
        assert_eq!(
            max_size(Some((1000, 700)), &outputs, Some(1.5)),
            size(1000.0, 700.0),
            "the bounds win"
        );
    }

    #[test]
    fn the_scale_is_the_fractional_one_with_a_viewport_else_the_outputs() {
        assert_eq!(device_scale(Some(180), 2), 1.5);
        assert_eq!(device_scale(Some(120), 2), 1.0);
        assert_eq!(device_scale(None, 2), 2.0);
        assert_eq!(device_scale(None, 0), 1.0);
    }
}
