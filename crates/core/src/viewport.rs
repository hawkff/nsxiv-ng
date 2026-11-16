//! Zoom and pan of the image in a view, with the origin at the bottom left.
//! Frontends draw `image_rect()`; a top-left frontend flips it.

use crate::config::{Config, ZOOM_GAP, ZOOM_LEVELS, ascending};
use crate::{Direction, Point, Rect, Size};

/// How the image fits the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleMode {
    /// Shrinks a large image to fit and leaves a small one at 100%.
    Down,
    Fit,
    Width,
    Height,
    Fill,
    /// The zoom the user chose, kept across view resizes.
    Manual,
}

// sizes, zooms and moves that are not finite change nothing
fn finite(s: Size) -> bool {
    s.width.is_finite() && s.height.is_finite()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Viewport {
    zoom: f64,
    scale_mode: ScaleMode,
    pan: Point,
    /// The shown image's size in pixels, rotation applied; zero without an image.
    image_size: Size,
    view_size: Size,
    pub antialias: bool,
    pub alpha_layer: bool,
    zoom_levels: Vec<f64>,
    pan_fraction: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport::with_config(&Config::default())
    }
}

impl Viewport {
    /// A viewport that zooms through `zoom_levels` and pans by `pan_fraction`.
    pub fn with_config(config: &Config) -> Self {
        // a list built in code may skip the parser's check
        let zoom_levels = if ascending(&config.zoom_levels, ZOOM_GAP) {
            config.zoom_levels.clone()
        } else {
            ZOOM_LEVELS.to_vec()
        };
        Viewport {
            zoom: 1.0,
            scale_mode: ScaleMode::Down,
            pan: Point::default(),
            image_size: Size::default(),
            view_size: Size::default(),
            antialias: true,
            alpha_layer: false,
            zoom_levels,
            pan_fraction: config.pan_fraction,
        }
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn scale_mode(&self) -> ScaleMode {
        self.scale_mode
    }

    pub fn image_size(&self) -> Size {
        self.image_size
    }

    pub fn view_size(&self) -> Size {
        self.view_size
    }

    pub fn display_size(&self) -> Size {
        Size {
            width: self.image_size.width * self.zoom,
            height: self.image_size.height * self.zoom,
        }
    }

    pub fn image_rect(&self) -> Rect {
        let d = self.display_size();
        Rect {
            x: self.pan.x,
            y: self.pan.y,
            width: d.width,
            height: d.height,
        }
    }

    /// A fit mode fits again; a manual zoom stays and only the pan adjusts.
    pub fn set_view_size(&mut self, size: Size) {
        if !finite(size) || size == self.view_size {
            return;
        }
        self.view_size = size;
        if self.scale_mode == ScaleMode::Manual {
            self.clamp_pan();
        } else {
            self.apply_scale_mode();
        }
    }

    pub fn set_image_size(&mut self, size: Size, reset_view: bool) {
        if !finite(size) {
            return;
        }
        self.image_size = size;
        if reset_view {
            self.apply_scale_mode();
        } else {
            self.clamp_pan();
        }
    }

    pub fn set_scale_mode(&mut self, mode: ScaleMode) {
        self.scale_mode = mode;
        self.apply_scale_mode();
    }

    /// Zooms around `anchor`, the view's center by default, and switches to manual mode.
    pub fn set_zoom(&mut self, zoom: f64, anchor: Option<Point>) {
        let anchor_finite = anchor.is_none_or(|a| a.x.is_finite() && a.y.is_finite());
        if !self.has_image() || !zoom.is_finite() || !anchor_finite {
            return;
        }
        let new = zoom.clamp(self.zoom_min(), self.zoom_max());
        let a = anchor.unwrap_or(Point {
            x: self.view_size.width / 2.0,
            y: self.view_size.height / 2.0,
        });
        let ratio = new / self.zoom;
        self.pan.x = a.x - (a.x - self.pan.x) * ratio;
        self.pan.y = a.y - (a.y - self.pan.y) * ratio;
        self.zoom = new;
        self.scale_mode = ScaleMode::Manual;
        self.clamp_pan();
    }

    /// The next zoom level up when `dir` is positive, down otherwise.
    pub fn zoom_step(&mut self, dir: i32) {
        let next = if dir > 0 {
            self.zoom_levels.iter().find(|&&z| z > self.zoom + 0.001)
        } else {
            self.zoom_levels
                .iter()
                .rev()
                .find(|&&z| z < self.zoom - 0.001)
        };
        if let Some(&z) = next {
            self.set_zoom(z, None);
        }
    }

    pub fn center_image(&mut self) {
        let d = self.display_size();
        self.pan = Point {
            x: (self.view_size.width - d.width) / 2.0,
            y: (self.view_size.height - d.height) / 2.0,
        };
        self.clamp_pan();
    }

    pub fn scroll(&mut self, dx: f64, dy: f64) {
        if !(dx.is_finite() && dy.is_finite()) {
            return;
        }
        self.pan.x += dx;
        self.pan.y += dy;
        self.clamp_pan();
    }

    /// Moves the view toward `direction`, by a fifth of it or by all of it.
    pub fn scroll_step(&mut self, direction: Direction, screenful: bool) {
        let fraction = if screenful { 1.0 } else { self.pan_fraction };
        let (sx, sy) = (
            self.view_size.width / fraction,
            self.view_size.height / fraction,
        );
        match direction {
            Direction::Left => self.scroll(sx, 0.0),
            Direction::Right => self.scroll(-sx, 0.0),
            Direction::Up => self.scroll(0.0, -sy),
            Direction::Down => self.scroll(0.0, sy),
        }
    }

    pub fn scroll_to_edge(&mut self, direction: Direction) {
        let d = self.display_size();
        match direction {
            Direction::Left => self.pan.x = 0.0,
            Direction::Right => self.pan.x = self.view_size.width - d.width,
            Direction::Up => self.pan.y = self.view_size.height - d.height,
            Direction::Down => self.pan.y = 0.0,
        }
        self.clamp_pan();
    }

    // the manual zoom range; it reaches below the smallest level for pinch zoom, and fit modes
    // may go lower still
    fn zoom_min(&self) -> f64 {
        self.zoom_levels[0] / 4.0
    }

    fn zoom_max(&self) -> f64 {
        self.zoom_levels[self.zoom_levels.len() - 1]
    }

    fn has_image(&self) -> bool {
        self.image_size.width > 0.0 && self.image_size.height > 0.0
    }

    fn apply_scale_mode(&mut self) {
        if !self.has_image() {
            return;
        }
        let zw = self.view_size.width / self.image_size.width;
        let zh = self.view_size.height / self.image_size.height;
        let zoom = match self.scale_mode {
            ScaleMode::Fit => zw.min(zh),
            ScaleMode::Down => zw.min(zh).min(1.0),
            ScaleMode::Width => zw,
            ScaleMode::Height => zh,
            ScaleMode::Fill => zw.max(zh),
            ScaleMode::Manual => self.zoom,
        };
        // below the manual floor a long panorama still fits; a view of no size takes the floor,
        // as a zero zoom would turn the next zoom's anchor math infinite
        self.zoom = if zoom > 0.0 {
            zoom.min(self.zoom_max())
        } else {
            self.zoom_min()
        };
        self.center_image();
    }

    /// An image smaller than the view centers on that axis; a larger one may not show past its edges.
    fn clamp_pan(&mut self) {
        let d = self.display_size();
        let clamp = |pan: f64, view: f64, image: f64| {
            if image <= view {
                (view - image) / 2.0
            } else {
                // max then min: unlike f64::clamp, they never panic on NaN
                pan.max(view - image).min(0.0)
            }
        };
        self.pan.x = clamp(self.pan.x, self.view_size.width, d.width);
        self.pan.y = clamp(self.pan.y, self.view_size.height, d.height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(view: (f64, f64), image: (f64, f64)) -> Viewport {
        let mut v = Viewport::default();
        v.set_view_size(Size {
            width: view.0,
            height: view.1,
        });
        v.set_image_size(
            Size {
                width: image.0,
                height: image.1,
            },
            true,
        );
        v
    }

    #[test]
    fn scale_modes_fit_the_view() {
        let mut v = viewport((640.0, 480.0), (320.0, 160.0));
        assert_eq!(v.zoom(), 1.0, "down keeps a small image at 100%");
        assert_eq!(
            v.image_rect(),
            Rect {
                x: 160.0,
                y: 160.0,
                width: 320.0,
                height: 160.0
            }
        );
        for (mode, zoom) in [
            (ScaleMode::Fit, 2.0),
            (ScaleMode::Width, 2.0),
            (ScaleMode::Height, 3.0),
            (ScaleMode::Fill, 3.0),
        ] {
            v.set_scale_mode(mode);
            assert_eq!(v.zoom(), zoom, "{mode:?}");
        }
        assert_eq!(
            viewport((640.0, 480.0), (1280.0, 480.0)).zoom(),
            0.5,
            "down shrinks a large image"
        );
        let panorama = viewport((640.0, 480.0), (32000.0, 1000.0));
        assert!(
            (panorama.display_size().width - 640.0).abs() < 1e-9,
            "a fit may go below the manual floor"
        );
        assert_eq!(
            viewport((0.0, 0.0), (320.0, 160.0)).zoom(),
            ZOOM_LEVELS[0] / 4.0,
            "a view of no size takes the floor"
        );
    }

    #[test]
    fn zoom_keeps_the_anchor_and_stays_inside_the_edges() {
        let mut v = viewport((640.0, 480.0), (320.0, 160.0));
        v.set_zoom(4.0, None);
        assert_eq!(v.scale_mode(), ScaleMode::Manual);
        assert_eq!((v.pan.x, v.pan.y), (-320.0, -80.0));
        v.scroll_to_edge(Direction::Right);
        v.scroll_to_edge(Direction::Up);
        assert_eq!((v.pan.x, v.pan.y), (-640.0, -160.0));
        v.scroll(1000.0, 1000.0);
        assert_eq!((v.pan.x, v.pan.y), (0.0, 0.0));
        v.scroll_step(Direction::Right, false);
        assert_eq!(v.pan.x, -128.0, "a step is a fifth of the view");
        v.set_zoom(100.0, None);
        assert_eq!(v.zoom(), 8.0);
        v.set_zoom(0.001, None);
        assert_eq!(v.zoom(), 0.125 / 4.0);
    }

    #[test]
    fn values_that_are_not_finite_change_nothing() {
        let mut v = viewport((640.0, 480.0), (320.0, 160.0));
        v.set_zoom(4.0, None);
        let before = v.clone();
        v.set_zoom(f64::NAN, None);
        v.set_zoom(
            2.0,
            Some(Point {
                x: f64::NAN,
                y: 0.0,
            }),
        );
        v.scroll(f64::NAN, 0.0);
        v.scroll(0.0, f64::INFINITY);
        v.set_view_size(Size {
            width: f64::NAN,
            height: 480.0,
        });
        v.set_image_size(
            Size {
                width: f64::INFINITY,
                height: 10.0,
            },
            true,
        );
        assert_eq!(v, before);
    }

    #[test]
    fn zoom_steps_follow_the_levels() {
        let mut v = viewport((256.0, 192.0), (320.0, 200.0));
        assert_eq!(v.zoom(), 0.8);
        v.zoom_step(1);
        assert_eq!(
            v.zoom(),
            1.0,
            "from between levels, + goes to the next one up"
        );
        v.zoom_step(-1);
        v.zoom_step(-1);
        assert_eq!(v.zoom(), 0.5);
    }

    #[test]
    fn the_config_sets_the_zoom_levels_and_the_pan_step() {
        let config = Config {
            zoom_levels: vec![0.5, 1.0, 3.0],
            pan_fraction: 10.0,
            ..Config::default()
        };
        let mut v = Viewport::with_config(&config);
        v.set_view_size(Size {
            width: 640.0,
            height: 480.0,
        });
        v.set_image_size(
            Size {
                width: 320.0,
                height: 160.0,
            },
            true,
        );
        v.zoom_step(1);
        assert_eq!(v.zoom(), 3.0);
        v.scroll_step(Direction::Right, false);
        assert_eq!(v.pan.x, -160.0 - 64.0, "a step is a tenth of the view");
        v.set_zoom(100.0, None);
        assert_eq!(v.zoom(), 3.0, "the last level bounds zooming");
        v.set_zoom(0.001, None);
        assert_eq!(v.zoom(), 0.125, "and a quarter of the first");
        for odd in [
            vec![],
            vec![8.0, 1.0],
            vec![1.0, 1.0005],
            vec![1.0, f64::NAN],
        ] {
            let config = Config {
                zoom_levels: odd,
                ..Config::default()
            };
            let mut v = Viewport::with_config(&config);
            assert_eq!(
                v,
                Viewport::default(),
                "{:?} takes the defaults",
                config.zoom_levels
            );
            v.set_image_size(
                Size {
                    width: 320.0,
                    height: 160.0,
                },
                true,
            );
            v.set_zoom(2.0, None);
        }
    }

    #[test]
    fn a_resize_refits_unless_the_zoom_is_manual() {
        let mut v = viewport((640.0, 480.0), (320.0, 160.0));
        v.set_scale_mode(ScaleMode::Fit);
        v.set_view_size(Size {
            width: 960.0,
            height: 480.0,
        });
        assert_eq!(v.zoom(), 3.0);
        v.set_zoom(1.5, None);
        v.set_view_size(Size {
            width: 320.0,
            height: 240.0,
        });
        assert_eq!(v.zoom(), 1.5);
        assert_eq!(
            v.pan.x.clamp(-160.0, 0.0),
            v.pan.x,
            "the pan stays inside the edges"
        );
    }
}
