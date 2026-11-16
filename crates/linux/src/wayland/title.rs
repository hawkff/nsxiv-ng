//! What the pointer does on the title bar and the edges the window draws itself, where the
//! compositor decorates nothing, as GNOME and Weston.

use nsxiv_ng_core::Point;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
use smithay_client_toolkit::seat::pointer::CursorIcon;

/// How near an edge, in logical units, a press resizes the window.
const EDGE: f64 = 6.0;

/// The longest wait between the two clicks of a double click on the title, in milliseconds.
const DOUBLE_CLICK: u32 = 400;

/// What a press on the window's own decorations does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Press {
    /// The compositor resizes the window from the edge.
    Resize(ResizeEdge),
    /// The compositor moves the window.
    Move,
    /// The close button.
    Close,
    /// The compositor's window menu.
    Menu,
    /// A double click maximizes the window, or restores it.
    Maximize,
}

/// The edge or corner of a window `width` × `height` logical units large near `at`, in the
/// window's own logical units, or None away from the edges.
pub(super) fn edge(at: Point, width: f64, height: f64) -> Option<ResizeEdge> {
    // in a window narrower than two edges, left and right must not both count: no edge value
    // means both, and a compositor may end the connection for one it does not know
    let top = at.y < EDGE;
    let bottom = !top && at.y >= height - EDGE;
    let left = at.x < EDGE;
    let right = !left && at.x >= width - EDGE;
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

/// The cursor over an edge, and the default one elsewhere.
pub(super) fn cursor(edge: Option<ResizeEdge>) -> CursorIcon {
    match edge {
        Some(ResizeEdge::Top) => CursorIcon::NResize,
        Some(ResizeEdge::Bottom) => CursorIcon::SResize,
        Some(ResizeEdge::Left) => CursorIcon::WResize,
        Some(ResizeEdge::Right) => CursorIcon::EResize,
        Some(ResizeEdge::TopLeft) => CursorIcon::NwResize,
        Some(ResizeEdge::TopRight) => CursorIcon::NeResize,
        Some(ResizeEdge::BottomLeft) => CursorIcon::SwResize,
        Some(ResizeEdge::BottomRight) => CursorIcon::SeResize,
        _ => CursorIcon::Default,
    }
}

/// What a press of the left button, or with `right` of the right one, at `at` does on the
/// decorations of a window `width` logical units wide with a title bar of `title` rows atop
/// it, 0 for none, and with `edge` the edge at `at` where the window has edges to resize by;
/// None where the press goes to the viewer. `last` is the time of the last click on the title,
/// which one at `time` within 400 ms makes a double click.
pub(super) fn press(
    right: bool,
    at: Point,
    width: f64,
    title: f64,
    edge: Option<ResizeEdge>,
    last: Option<u32>,
    time: u32,
) -> Option<Press> {
    if !right && let Some(edge) = edge {
        return Some(Press::Resize(edge));
    }
    if at.y >= title {
        return None;
    }
    Some(if right {
        Press::Menu
    } else if at.x >= width - title {
        // the close button, as wide as the title is high
        Press::Close
    } else if last.is_some_and(|last| time.wrapping_sub(last) < DOUBLE_CLICK) {
        Press::Maximize
    } else {
        Press::Move
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    #[test]
    fn the_edges_and_corners_are_six_units_deep() {
        assert_eq!(edge(at(3.0, 100.0), 400.0, 300.0), Some(ResizeEdge::Left));
        assert_eq!(
            edge(at(394.0, 100.0), 400.0, 300.0),
            Some(ResizeEdge::Right)
        );
        assert_eq!(edge(at(200.0, 5.9), 400.0, 300.0), Some(ResizeEdge::Top));
        assert_eq!(
            edge(at(200.0, 294.0), 400.0, 300.0),
            Some(ResizeEdge::Bottom)
        );
        assert_eq!(edge(at(2.0, 2.0), 400.0, 300.0), Some(ResizeEdge::TopLeft));
        assert_eq!(
            edge(at(399.0, 1.0), 400.0, 300.0),
            Some(ResizeEdge::TopRight)
        );
        assert_eq!(
            edge(at(0.0, 299.0), 400.0, 300.0),
            Some(ResizeEdge::BottomLeft)
        );
        assert_eq!(
            edge(at(399.0, 299.0), 400.0, 300.0),
            Some(ResizeEdge::BottomRight)
        );
        assert_eq!(edge(at(6.0, 6.0), 400.0, 300.0), None, "inside");
        // narrower than two edges: the left wins, never both
        assert_eq!(edge(at(5.0, 100.0), 8.0, 300.0), Some(ResizeEdge::Left));
    }

    #[test]
    fn each_edge_shows_its_cursor() {
        assert_eq!(cursor(None), CursorIcon::Default);
        assert_eq!(cursor(Some(ResizeEdge::Top)), CursorIcon::NResize);
        assert_eq!(cursor(Some(ResizeEdge::BottomRight)), CursorIcon::SeResize);
        assert_eq!(cursor(Some(ResizeEdge::Left)), CursorIcon::WResize);
    }

    #[test]
    fn a_press_on_the_title_moves_closes_maximizes_or_opens_the_menu() {
        let press = |right, x, y, last| press(right, at(x, y), 400.0, 27.0, None, last, 1000);
        assert_eq!(press(false, 100.0, 10.0, None), Some(Press::Move));
        assert_eq!(press(false, 380.0, 10.0, None), Some(Press::Close), "the ×");
        assert_eq!(
            press(false, 372.0, 10.0, None),
            Some(Press::Move),
            "left of it"
        );
        assert_eq!(press(false, 100.0, 10.0, Some(700)), Some(Press::Maximize));
        assert_eq!(
            press(false, 100.0, 10.0, Some(500)),
            Some(Press::Move),
            "too slow for a double click"
        );
        let wrapped = super::press(
            false,
            at(100.0, 10.0),
            400.0,
            27.0,
            None,
            Some(u32::MAX - 50),
            100,
        );
        assert_eq!(wrapped, Some(Press::Maximize), "the clock wrapped");
        assert_eq!(press(true, 100.0, 10.0, None), Some(Press::Menu));
        assert_eq!(
            press(true, 390.0, 10.0, None),
            Some(Press::Menu),
            "on the × too"
        );
        assert_eq!(press(false, 100.0, 27.0, None), None, "below the title");
        assert_eq!(press(true, 100.0, 100.0, None), None);
    }

    #[test]
    fn the_left_button_resizes_at_an_edge_and_the_right_goes_to_the_viewer_there() {
        let left = Some(ResizeEdge::Left);
        assert_eq!(
            press(false, at(2.0, 100.0), 400.0, 27.0, left, None, 0),
            Some(Press::Resize(ResizeEdge::Left))
        );
        assert_eq!(
            press(true, at(2.0, 100.0), 400.0, 27.0, left, None, 0),
            None
        );
        let top = Some(ResizeEdge::Top);
        assert_eq!(
            press(true, at(100.0, 2.0), 400.0, 27.0, top, None, 0),
            Some(Press::Menu),
            "the title's edge is the title's for the right button"
        );
        // without a title bar, as in fullscreen, nothing is the decorations'
        assert_eq!(
            press(false, at(100.0, 2.0), 400.0, 0.0, None, None, 0),
            None
        );
    }
}
