//! The window's size and place on the screen, and points between AppKit and the session.

use nsxiv_ng_core::{Point, Rect, Size};

/// The window's content for `image` with `bar` more points below it for the bar, never
/// larger than the image and within `room`, the screen's visible frame less the window's
/// title bar, in whole points; None for an image with no area.
pub(crate) fn fit(image: Size, bar: f64, room: Size) -> Option<Size> {
    if !(image.width > 0.0 && image.height > 0.0) {
        return None;
    }
    let bar = bar.round().max(0.0);
    let scale = 1f64
        .min(room.width / image.width)
        .min((room.height - bar) / image.height);
    let side = |length: f64| (length * scale).round().max(1.0);
    Some(Size {
        width: side(image.width),
        height: side(image.height) + bar,
    })
}

/// The window's content a user's resize gives, from the content it had, `old`, and the one
/// the resize proposes, keeping the shape of `image` above `bar` more points for the bar: an
/// edge drag changes one side, which drives the other; a
/// corner drag or the zoom button changes both, and the largest content inside the proposed
/// one wins. The proposed content itself for an image with no area.
pub(crate) fn kept_shape(image: Size, bar: f64, old: Size, proposed: Size) -> Size {
    if !(image.width > 0.0 && image.height > 0.0) {
        return proposed;
    }
    let by_height = (proposed.height - bar) * image.width / image.height;
    let width = if proposed.width == old.width {
        by_height
    } else if proposed.height == old.height {
        proposed.width
    } else {
        proposed.width.min(by_height)
    };
    // never below a point wide, and a shape too narrow for one no higher than proposed
    let height = width.max(1.0) * image.height / image.width + bar;
    Size {
        width: width.max(1.0),
        height: if width <= 1.0 {
            height.min(proposed.height)
        } else {
            height
        },
    }
}

/// The frame the zoom button gives the window: the largest of the shape of `image`, above
/// `bar` more points for the bar, inside `default`, AppKit's standard frame, centered in it;
/// `chrome` is what the frame adds to its content. None for an image with no area.
pub(crate) fn zoomed(image: Size, bar: f64, chrome: Size, default: Rect) -> Option<Rect> {
    if !(image.width > 0.0 && image.height > 0.0) {
        return None;
    }
    let room = Size {
        width: default.width - chrome.width,
        height: default.height - chrome.height - bar,
    };
    // as kept_shape gives a frame, so the zoom's proposal keeps it as it is
    let width = room
        .width
        .min(room.height * image.width / image.height)
        .max(1.0);
    // a shape too narrow for a point wide no higher than the room
    let content = (width * image.height / image.width).min(room.height);
    let size = Size {
        width: width + chrome.width,
        height: content + bar + chrome.height,
    };
    Some(Rect {
        x: default.x + (default.width - size.width) / 2.0,
        y: default.y + (default.height - size.height) / 2.0,
        width: size.width,
        height: size.height,
    })
}

/// A frame of `size` with the center of `old`, moved inside `visible` where it reaches past
/// it; one larger than `visible` keeps its top left corner in it. Rectangles are AppKit's,
/// `x` and `y` their bottom left corner, y upward.
pub(crate) fn placed(old: Rect, size: Size, visible: Rect) -> Rect {
    let center = |at: f64, old: f64, new: f64| at + old / 2.0 - new / 2.0;
    let mut x = center(old.x, old.width, size.width);
    let mut y = center(old.y, old.height, size.height);
    x = x.min(visible.x + visible.width - size.width).max(visible.x);
    y = y
        .max(visible.y)
        .min(visible.y + visible.height - size.height);
    Rect {
        x,
        y,
        width: size.width,
        height: size.height,
    }
}

/// A point in a view `height` points high, y upward as AppKit counts it, as the session
/// counts it: from the top left, y downward.
pub(crate) fn flip(at: Point, height: f64) -> Point {
    Point {
        x: at.x,
        y: height - at.y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: f64, height: f64) -> Size {
        Size { width, height }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn the_fit_keeps_the_image_within_the_room_and_never_enlarges_it() {
        let room = size(1024.0, 650.0);
        assert_eq!(
            fit(size(640.0, 400.0), 20.0, room),
            Some(size(640.0, 420.0))
        );
        // 4000 x 3000 within 1024 x (650 - 20): the height binds, 630 / 3000
        assert_eq!(
            fit(size(4000.0, 3000.0), 20.0, room),
            Some(size(840.0, 650.0))
        );
        // a panorama: the width binds
        assert_eq!(
            fit(size(9600.0, 1000.0), 20.0, room),
            Some(size(1024.0, 127.0))
        );
        assert_eq!(
            fit(size(10.0, 100_000.0), 0.0, room),
            Some(size(1.0, 650.0))
        );
        assert_eq!(
            fit(size(300.0, 200.0), 23.6, room),
            Some(size(300.0, 224.0))
        );
        assert_eq!(fit(size(0.0, 100.0), 20.0, room), None);
        assert_eq!(fit(size(f64::NAN, 100.0), 20.0, room), None);
    }

    #[test]
    fn a_resize_keeps_the_image_s_shape_with_the_bar_outside_it() {
        let image = size(300.0, 200.0);
        let old = size(300.0, 220.0);
        // the right edge: the width drives the height
        assert_eq!(
            kept_shape(image, 20.0, old, size(450.0, 220.0)),
            size(450.0, 320.0)
        );
        // the bottom edge: the height above the bar drives the width
        assert_eq!(
            kept_shape(image, 20.0, old, size(300.0, 320.0)),
            size(450.0, 320.0)
        );
        // a corner, or the zoom button: the largest inside the proposal, here by its height
        assert_eq!(
            kept_shape(image, 20.0, old, size(600.0, 320.0)),
            size(450.0, 320.0)
        );
        // and here by its width
        assert_eq!(
            kept_shape(image, 20.0, old, size(360.0, 520.0)),
            size(360.0, 260.0)
        );
        // no bar
        assert_eq!(
            kept_shape(image, 0.0, size(300.0, 200.0), size(300.0, 100.0)),
            size(150.0, 100.0)
        );
        // never below a point, and no higher than proposed where that leaves the shape; an
        // image with no area leaves the proposal as it is
        assert_eq!(
            kept_shape(image, 20.0, old, size(300.0, 30.0)),
            size(15.0, 30.0)
        );
        assert_eq!(
            kept_shape(image, 20.0, old, size(300.0, 20.5)),
            size(1.0, 20.5)
        );
        // a shape too narrow for a point: the proposal's height bounds it
        assert_eq!(
            kept_shape(size(10.0, 100_000.0), 27.0, old, size(1.0, 678.0)),
            size(1.0, 678.0)
        );
        // also where only the width changed, to the point itself
        assert_eq!(
            kept_shape(
                size(10.0, 100_000.0),
                27.0,
                size(2.0, 678.0),
                size(1.0, 678.0)
            ),
            size(1.0, 678.0)
        );
        assert_eq!(
            kept_shape(size(0.0, 200.0), 20.0, old, size(10.0, 10.0)),
            size(10.0, 10.0)
        );
    }

    #[test]
    fn the_zoom_gives_the_largest_frame_of_the_image_s_shape_inside_the_standard_one() {
        let default = rect(0.0, 60.0, 1024.0, 678.0);
        // the height binds: (678 - 27) / 400 times 600 is 976.5 of the 1024
        assert_eq!(
            zoomed(size(600.0, 400.0), 27.0, size(0.0, 0.0), default),
            Some(rect(23.75, 60.0, 976.5, 678.0))
        );
        // a panorama: the width binds, centered up and down
        assert_eq!(
            zoomed(size(2000.0, 500.0), 27.0, size(0.0, 0.0), default),
            Some(rect(0.0, 60.0 + (678.0 - 283.0) / 2.0, 1024.0, 283.0))
        );
        // with a title bar of 28 points outside the content
        assert_eq!(
            zoomed(size(600.0, 400.0), 27.0, size(0.0, 28.0), default),
            Some(rect(44.75, 60.0, 934.5, 678.0))
        );
        // the frame it gives is one a resize to it keeps as it is
        let frame = zoomed(size(600.0, 400.0), 27.0, size(0.0, 0.0), default).unwrap();
        let proposed = size(frame.width, frame.height);
        assert_eq!(
            kept_shape(size(600.0, 400.0), 27.0, size(600.0, 427.0), proposed),
            proposed
        );
        // a shape too narrow for a point stays inside the standard frame, and a resize to the
        // frame it gives keeps it
        let narrow = size(10.0, 100_000.0);
        let frame = zoomed(narrow, 27.0, size(0.0, 0.0), default).unwrap();
        assert_eq!(frame, rect(511.5, 60.0, 1.0, 678.0));
        let proposed = size(frame.width, frame.height);
        assert_eq!(
            kept_shape(narrow, 27.0, size(600.0, 427.0), proposed),
            proposed
        );
        assert_eq!(zoomed(size(0.0, 1.0), 27.0, size(0.0, 0.0), default), None);
    }

    #[test]
    fn a_resized_window_keeps_its_center_inside_the_visible_frame() {
        let visible = rect(0.0, 60.0, 1024.0, 678.0);
        // from 800 x 600 at the screen's center to 300 x 224 around the same center
        let old = rect(112.0, 99.0, 800.0, 600.0);
        assert_eq!(
            placed(old, size(300.0, 224.0), visible),
            rect(362.0, 287.0, 300.0, 224.0)
        );
        // grown near the bottom right corner, pushed back in
        let corner = rect(900.0, 70.0, 100.0, 100.0);
        assert_eq!(
            placed(corner, size(600.0, 400.0), visible),
            rect(424.0, 60.0, 600.0, 400.0)
        );
        // near the top left, pushed down and right
        let top = rect(-50.0, 700.0, 100.0, 100.0);
        assert_eq!(
            placed(top, size(200.0, 100.0), visible),
            rect(0.0, 638.0, 200.0, 100.0)
        );
        // larger than the visible frame: its top left corner stays in it
        assert_eq!(
            placed(old, size(2000.0, 1000.0), visible),
            rect(0.0, -262.0, 2000.0, 1000.0)
        );
    }

    #[test]
    fn appkit_counts_y_up_from_the_bottom_and_the_session_down_from_the_top() {
        let flipped = flip(Point { x: 10.0, y: 30.0 }, 200.0);
        assert_eq!(flipped, Point { x: 10.0, y: 170.0 });
        assert_eq!(flip(flipped, 200.0), Point { x: 10.0, y: 30.0 });
    }
}
