//! Shared pointer input: a drag pans, a click at either side navigates, two clicks on a cell
//! open it, the right button marks in the grid and switches mode in the image view, and the
//! wheel scrolls the grid or zooms.

use nsxiv_ng_core::Point;

use crate::{Request, Session};

/// The longest wait between the two clicks of a double click, in milliseconds.
const DOUBLE_CLICK: u32 = 400;

/// The buttons the viewer takes; frontends drop the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// One scroll gesture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scroll {
    /// A wheel's steps, positive toward the user, as down or a click of X11's button 5.
    Steps(i32),
    /// A wheel that gives no steps, only a distance in logical units, positive down, as on a
    /// Wayland seat before version 5.
    WheelDistance(f64),
    /// A touchpad's smooth scroll in logical units, positive down and right.
    Distance { dx: f64, dy: f64 },
}

/// What a gesture has done so far.
#[derive(Debug, Default)]
pub(crate) struct Gesture {
    /// Where the pointer was last.
    at: Point,
    /// The left button went down in the image view, and has not come up.
    dragging: bool,
    /// The pointer moved since, so the release does not navigate.
    dragged: bool,
    /// The time and cell of a click that a second one may make a double click.
    last_click: Option<(u32, usize)>,
}

impl Session {
    /// A button went down at `at`, in logical units from the top left of the area the session
    /// draws, y downward, at `time`, the display server's milliseconds, which may wrap. Two
    /// presses on one spot within 400 ms make a double click.
    pub fn press(&mut self, button: Button, at: Point, time: u32) -> Vec<Request> {
        self.press_counted(button, at, time, None)
    }

    /// [`Session::press`] with the presses the frontend's system counted, `clicks` 2 or more
    /// for a double click at the user's own double-click speed, where the system counts them.
    pub fn press_counted(
        &mut self,
        button: Button,
        at: Point,
        time: u32,
        clicks: Option<u32>,
    ) -> Vec<Request> {
        if self.editing() {
            // the left button draws in an edit; the right does nothing there
            return match button {
                Button::Left => self.edit_press(at, time, clicks),
                Button::Right => Vec::new(),
            };
        }
        self.pointer.at = at;
        let grid = self.viewer.thumbnail_mode();
        let cell = if self.in_canvas(at) {
            self.viewer.grid().index_at(at)
        } else {
            None
        };
        let effects = match button {
            Button::Left if !grid => {
                self.pointer.dragging = true;
                self.pointer.dragged = false;
                Vec::new()
            }
            // no drag starts here, so the release after a double click cannot navigate
            Button::Left => match cell {
                Some(i) => match self.pointer.last_click {
                    Some((last, j))
                        if j == i
                            && clicks
                                .map_or(time.wrapping_sub(last) < DOUBLE_CLICK, |n| n >= 2) =>
                    {
                        self.pointer.last_click = None;
                        self.viewer.open(i)
                    }
                    _ => {
                        self.pointer.last_click = Some((time, i));
                        self.viewer.select(i)
                    }
                },
                None => Vec::new(),
            },
            Button::Right if !grid => self.viewer.switch_mode(),
            // marks without moving the selection, as nsxiv does
            Button::Right => cell.map_or_else(Vec::new, |i| self.viewer.toggle_mark(i)),
        };
        self.carry(effects)
    }

    /// A button came up at `at`. A left click that did not drag navigates in the image view:
    /// back in the left part of the canvas `nav_width_fraction` gives, on in the right.
    pub fn release(&mut self, button: Button, at: Point) -> Vec<Request> {
        if self.editing() {
            return match button {
                Button::Left => self.edit_release(at),
                Button::Right => Vec::new(),
            };
        }
        if button != Button::Left || !self.pointer.dragging {
            return Vec::new();
        }
        self.pointer.dragging = false;
        if self.viewer.thumbnail_mode() || self.pointer.dragged || !self.in_canvas(at) {
            return Vec::new();
        }
        let width = self.window.width;
        let zone = width * self.nav_width_fraction;
        let effects = if at.x < zone {
            self.viewer.navigate(-1)
        } else if at.x > width - zone {
            self.viewer.navigate(1)
        } else {
            Vec::new()
        };
        self.carry(effects)
    }

    /// The pointer moved to `at`; with the left button down in the image view, the image
    /// follows it.
    pub fn pointer_moved(&mut self, at: Point) -> Vec<Request> {
        if self.editing() {
            return self.edit_drag(at);
        }
        let from = std::mem::replace(&mut self.pointer.at, at);
        if !self.pointer.dragging || self.viewer.thumbnail_mode() {
            return Vec::new();
        }
        self.pointer.dragged = true;
        // the pointer counts y downward, the viewport upward
        let effects = self.viewer.pan(at.x - from.x, from.y - at.y);
        self.carry(effects)
    }

    /// One scroll gesture: the grid scrolls, by a third of a cell a step or by the distance;
    /// in the image view a wheel zooms, out toward the user, a level a step or one level for a
    /// distance, and a touchpad pans.
    pub fn scroll(&mut self, scroll: Scroll) -> Vec<Request> {
        // an edit's canvas neither scrolls nor zooms
        if self.editing() {
            return Vec::new();
        }
        let grid = self.viewer.thumbnail_mode();
        let effects = match scroll {
            Scroll::Steps(steps) if grid => {
                let cell = self.viewer.grid().cell_size();
                self.viewer.scroll_grid(f64::from(steps) * cell / 3.0)
            }
            Scroll::WheelDistance(dy) | Scroll::Distance { dy, .. } if grid => {
                self.viewer.scroll_grid(dy)
            }
            Scroll::Steps(steps) => self.zoom_steps(steps),
            Scroll::WheelDistance(dy) if dy != 0.0 => {
                self.zoom_steps(if dy > 0.0 { 1 } else { -1 })
            }
            Scroll::WheelDistance(_) => Vec::new(),
            Scroll::Distance { dx, dy } => self.viewer.pan(-dx, dy),
        };
        self.carry(effects)
    }

    /// Zooms the image by `factor` about `at`, in the units [`Session::press`] takes: the point
    /// of the image under `at` stays there, as far as the zoom's bounds and the pan's clamp let
    /// it. In the grid, in an edit, and for a factor that is not finite, nothing.
    pub fn zoom_at(&mut self, factor: f64, at: Point) -> Vec<Request> {
        if self.viewer.thumbnail_mode() || !factor.is_finite() || self.editing() {
            return Vec::new();
        }
        let zoom = self.viewer.viewport().zoom() * factor;
        // the pointer counts y downward from the top, the viewport upward from the bottom
        let anchor = Point {
            x: at.x,
            y: self.canvas().height - at.y,
        };
        let effects = self.viewer.zoom_to(zoom, anchor);
        self.carry(effects)
    }

    // A wheel's steps in the image view, out toward the user; more steps than there are zoom
    // levels change nothing more.
    fn zoom_steps(&mut self, steps: i32) -> Vec<nsxiv_ng_core::viewer::Effect> {
        let dir = if steps > 0 { -1 } else { 1 };
        let count = steps.unsigned_abs().min(self.zoom_levels);
        (0..count)
            .flat_map(|_| self.viewer.zoom_step(dir))
            .collect()
    }

    // On the canvas the viewer lays out for, not on the bar below it.
    fn in_canvas(&self, at: Point) -> bool {
        at.y >= 0.0 && at.y < self.canvas().height
    }
}
