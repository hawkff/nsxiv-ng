//! The window's size: fitted to the image within its monitor, and the hints that keep the
//! image's aspect while the user resizes it.

use std::collections::VecDeque;

use nsxiv_ng_core::{Rect, Size};
use x11rb::properties::{AspectRatio, WmSizeHints};

/// The monitor the window is on, as the one that holds its center, else the first.
pub(crate) fn monitor(window: Rect, monitors: &[Rect]) -> Option<Rect> {
    let (x, y) = (
        window.x + window.width / 2.0,
        window.y + window.height / 2.0,
    );
    let holds = |m: &&Rect| x >= m.x && x < m.x + m.width && y >= m.y && y < m.y + m.height;
    monitors.iter().find(holds).or(monitors.first()).copied()
}

/// What `monitor` leaves a window inside a frame of `extents`, _NET_FRAME_EXTENTS' left, right,
/// top and bottom; all of it without the four.
pub(crate) fn room(monitor: Size, extents: &[u32]) -> Size {
    let &[left, right, top, bottom] = extents else {
        return monitor;
    };
    Size {
        width: (monitor.width - f64::from(left) - f64::from(right)).max(1.0),
        height: (monitor.height - f64::from(top) - f64::from(bottom)).max(1.0),
    }
}

/// Whether the window manager's first frame extents fit the window again: those told after a
/// fit made without them, once, while that fit still holds the window's size.
#[derive(Debug, Default)]
pub(crate) struct FrameClaim {
    closed: bool,
    /// The sizes the viewer asked for whose ConfigureNotify has not come yet, oldest first.
    asked: VecDeque<(u16, u16)>,
}

/// The most sizes asked for kept until their ConfigureNotify comes; older ones are dropped.
const ASKED: usize = 8;

impl FrameClaim {
    /// A resize to `size` asked of the server.
    pub(crate) fn asked(&mut self, size: (u16, u16)) {
        if self.asked.len() == ASKED {
            self.asked.pop_front();
        }
        self.asked.push_back(size);
    }

    /// A fit made with `extents` read; one that knew the frame needs no other.
    pub(crate) fn fitted(&mut self, extents: &[u32]) {
        if extents.len() == 4 {
            self.closed = true;
        }
    }

    /// The window configured to `size` from `current`: a size the viewer asked for is its own,
    /// and those asked before it came or were passed over; another, as a user's resize gives,
    /// holds the window's size from then on. A move keeps the size.
    pub(crate) fn configured(&mut self, size: (u16, u16), current: (u16, u16)) {
        if let Some(at) = self.asked.iter().position(|&asked| asked == size) {
            self.asked.drain(..=at);
        } else if size != current {
            self.closed = true;
        }
    }

    /// Whether `extents`, told by a change of _NET_FRAME_EXTENTS, fit the window again: the
    /// first four told after a fit, outside fullscreen, and none after them.
    pub(crate) fn told(&mut self, extents: &[u32], fitted: bool, fullscreen: bool) -> bool {
        if fullscreen || extents.len() != 4 {
            return false;
        }
        let refit = !self.closed && fitted;
        self.closed = true;
        refit
    }
}

/// The window's image area for `image` with `bar` pixels below it, never larger than the image
/// and within `monitor`, in whole pixels; None for an image with no area.
pub(crate) fn fit(image: Size, bar: f64, monitor: Size) -> Option<(u32, u32)> {
    if !(image.width > 0.0 && image.height > 0.0) {
        return None;
    }
    let scale = 1f64
        .min(monitor.width / image.width)
        .min((monitor.height - bar) / image.height);
    let side = |length: f64| ((length * scale).round() as u32).max(1);
    Some((side(image.width), side(image.height)))
}

/// The hints for a window that keeps the aspect of an image area `width × height` above a
/// bar `bar` pixels high, as mpv's does; with no image, none, so the grid resizes freely.
pub(crate) fn size_hints(image: Option<(u32, u32)>, bar: u32) -> WmSizeHints {
    let mut hints = WmSizeHints::new();
    if let Some((width, height)) = image {
        let ratio = AspectRatio::new(width as i32, height as i32);
        hints.aspect = Some((ratio, ratio));
        hints.base_size = Some((0, bar as i32));
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn size(width: f64, height: f64) -> Size {
        Size { width, height }
    }

    #[test]
    fn the_monitor_is_the_one_under_the_windows_center() {
        let left = rect(0.0, 0.0, 1920.0, 1080.0);
        let right = rect(1920.0, 0.0, 2560.0, 1440.0);
        let monitors = [left, right];
        assert_eq!(
            monitor(rect(100.0, 100.0, 800.0, 600.0), &monitors),
            Some(left)
        );
        // across both, its center on the right
        assert_eq!(
            monitor(rect(1500.0, 0.0, 900.0, 600.0), &monitors),
            Some(right)
        );
        assert_eq!(
            monitor(rect(-5000.0, 0.0, 800.0, 600.0), &monitors),
            Some(left),
            "off every monitor, the first"
        );
        assert_eq!(monitor(rect(0.0, 0.0, 800.0, 600.0), &[]), None);
    }

    #[test]
    fn a_window_not_placed_yet_asks_with_the_pointer_a_point() {
        let left = rect(0.0, 0.0, 1920.0, 1080.0);
        let right = rect(1920.0, 0.0, 800.0, 600.0);
        let monitors = [left, right];
        assert_eq!(
            monitor(rect(2000.0, 300.0, 0.0, 0.0), &monitors),
            Some(right)
        );
        assert_eq!(
            monitor(rect(1919.0, 0.0, 0.0, 0.0), &monitors),
            Some(left),
            "the edge between them"
        );
        assert_eq!(monitor(rect(1920.0, 0.0, 0.0, 0.0), &monitors), Some(right));
    }

    #[test]
    fn the_fit_keeps_the_image_within_the_monitor_and_never_enlarges_it() {
        let monitor = size(1920.0, 1080.0);
        assert_eq!(fit(size(640.0, 400.0), 20.0, monitor), Some((640, 400)));
        // 4000 x 3000 within 1920 x (1080 - 20): the height binds, 1060 / 3000
        assert_eq!(fit(size(4000.0, 3000.0), 20.0, monitor), Some((1413, 1060)));
        // a panorama: the width binds
        assert_eq!(fit(size(9600.0, 1000.0), 20.0, monitor), Some((1920, 200)));
        assert_eq!(fit(size(10.0, 100_000.0), 0.0, monitor), Some((1, 1080)));
        assert_eq!(fit(size(0.0, 100.0), 20.0, monitor), None);
        assert_eq!(fit(size(f64::NAN, 100.0), 20.0, monitor), None);
    }

    #[test]
    fn the_room_is_the_monitor_less_the_frame() {
        let monitor = size(800.0, 600.0);
        assert_eq!(room(monitor, &[1, 1, 1, 1]), size(798.0, 598.0));
        assert_eq!(room(monitor, &[4, 4, 22, 4]), size(792.0, 574.0));
        assert_eq!(room(monitor, &[]), monitor, "no frame told");
        assert_eq!(room(monitor, &[900, 0, 0, 700]), size(1.0, 1.0));
        // 1200 x 900 within 798 x 598: the height binds, as a frame of 1 all round leaves
        assert_eq!(
            fit(size(1200.0, 900.0), 0.0, room(monitor, &[1, 1, 1, 1])),
            Some((797, 598))
        );
    }

    #[test]
    fn late_frame_extents_fit_the_window_again_once_while_its_first_fit_holds() {
        let frame = [4, 4, 22, 4];
        let first = (800, 600);
        // a claim after a fit made without the frame, the window at 800x600 as asked
        let fitted = || {
            let mut claim = FrameClaim::default();
            claim.asked(first);
            claim.fitted(&[]);
            claim.configured(first, (640, 480));
            claim
        };
        // told before the fit, which then reads them
        let mut claim = FrameClaim::default();
        assert!(!claim.told(&frame, false, false));
        claim.fitted(&frame);
        assert!(!claim.told(&frame, true, false));
        // the fit made without them, the check before the map reads them
        let mut claim = FrameClaim::default();
        claim.fitted(&[]);
        claim.fitted(&frame);
        assert!(!claim.told(&frame, true, false));
        // told after the map: once, and never again
        let mut claim = fitted();
        claim.configured(first, first);
        assert!(!claim.told(&[], true, false), "the property deleted");
        assert!(claim.told(&frame, true, false));
        assert!(!claim.told(&frame, true, false), "told again");
        assert!(!claim.told(&[1, 1, 1, 1], true, false), "other ones");
        // after a user's resize
        let mut claim = fitted();
        claim.configured((700, 500), first);
        assert!(!claim.told(&frame, true, false));
        // during fullscreen, and after it the first ones told outside it
        let mut claim = fitted();
        assert!(!claim.told(&[0, 0, 0, 0], true, true));
        assert!(claim.told(&frame, true, false));
        // two sizes asked before either came, the image's fit and the one with the bar, then
        // both come; the first late extents fit the window again once
        let mut claim = FrameClaim::default();
        claim.asked((1440, 1080));
        claim.fitted(&[]);
        claim.asked((1413, 1080));
        claim.fitted(&[]);
        claim.configured((1440, 1080), (800, 600));
        claim.configured((1413, 1080), (1440, 1080));
        assert!(claim.told(&frame, true, false));
        claim.asked((1386, 1040));
        claim.configured((1386, 1040), (1413, 1080));
        claim.configured((1000, 700), (1386, 1040));
        assert!(claim.closed, "a size never asked for");
        // a size that comes after a newer one asked after it is no longer the viewer's
        let mut claim = FrameClaim::default();
        claim.asked((1440, 1080));
        claim.asked((1413, 1080));
        claim.fitted(&[]);
        claim.configured((1413, 1080), (800, 600));
        claim.configured((1440, 1080), (1413, 1080));
        assert!(
            !claim.told(&frame, true, false),
            "the size passed over came after"
        );
        // more sizes asked than kept: the oldest is not the viewer's any more
        let mut claim = FrameClaim::default();
        for width in 100..=ASKED as u16 + 100 {
            claim.asked((width, 100));
        }
        claim.fitted(&[]);
        claim.configured((101, 100), (800, 600));
        assert!(!claim.closed, "the oldest kept");
        claim.configured((100, 100), (101, 100));
        assert!(claim.closed, "the one dropped");
        // a move keeps the size, which is no resize
        let mut claim = fitted();
        claim.configured(first, first);
        assert!(claim.told(&frame, true, false));
    }

    #[test]
    fn the_hints_keep_the_images_aspect_above_the_bar() {
        let hints = size_hints(Some((1413, 1060)), 20);
        let (min, max) = hints.aspect.expect("an aspect");
        assert_eq!((min.numerator, min.denominator), (1413, 1060));
        assert_eq!((max.numerator, max.denominator), (1413, 1060));
        assert_eq!(
            hints.base_size,
            Some((0, 20)),
            "the bar takes no part in it"
        );
        let free = size_hints(None, 20);
        assert!(free.aspect.is_none() && free.base_size.is_none());
    }
}
