//! An annotation, its geometry and its hit test, with the origin at the top left.

use nsxiv_ng_core::{Point, Rect};
use nsxiv_ng_render::text::{Text, Weight};

/// The tools, in the order their keys go; the status line shows their names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Arrow,
    Rect,
    Ellipse,
    Text,
    Pencil,
    Marker,
    Badge,
    Stamp,
    Censor,
    Spotlight,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Arrow => "arrow",
            Tool::Rect => "rect",
            Tool::Ellipse => "ellipse",
            Tool::Text => "text",
            Tool::Pencil => "pencil",
            Tool::Marker => "marker",
            Tool::Badge => "badge",
            Tool::Stamp => "stamp",
            Tool::Censor => "censor",
            Tool::Spotlight => "spotlight",
        }
    }
}

/// How a rectangle or an ellipse is drawn; `f` cycles through them in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillMode {
    Stroke,
    /// The stroke over a fill of the color at 30%.
    StrokeFill,
    Fill,
}

/// How a censor hides what it covers; `C` cycles through them in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CensorMode {
    Pixelate,
    Blur,
    Solid,
}

impl CensorMode {
    pub fn name(self) -> &'static str {
        match self {
            CensorMode::Pixelate => "pixelate",
            CensorMode::Blur => "blur",
            CensorMode::Solid => "solid",
        }
    }
}

/// An sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The colors keys 1 to 8 pick, with their names for the status line: the system colors as
/// macOS 27.2 gives them in the light appearance, fixed, so a saved image does not
/// depend on the system or its appearance.
pub const PALETTE: [(&str, Rgb); 8] = [
    ("red", Rgb(255, 56, 60)),
    ("orange", Rgb(255, 141, 40)),
    ("yellow", Rgb(255, 204, 0)),
    ("green", Rgb(52, 199, 89)),
    ("blue", Rgb(0, 136, 255)),
    ("purple", Rgb(203, 48, 224)),
    ("black", Rgb(0, 0, 0)),
    ("white", Rgb(255, 255, 255)),
];

/// The stamps `<` and `>` step through.
pub const EMOJIS: [&str; 21] = [
    "✅",
    "❌",
    "⚠️",
    "❗",
    "⭐",
    "🔥",
    "👍",
    "👎",
    "💡",
    "🔒",
    "🚫",
    "📌",
    "🎯",
    "🐛",
    "💯",
    "🤔",
    "😂",
    "🎉",
    "❤️",
    "🏴‍☠️",
    "🍆",
];

/// One annotation. Its geometry is in the image's pixels, origin top left, y down; text's
/// `start` is its first line's baseline at the left.
#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    /// The edit's own number for it, which the censor cache and the selection go by.
    pub id: u64,
    pub tool: Tool,
    pub start: Point,
    pub end: Point,
    /// The pencil's and the marker's path.
    pub points: Vec<Point>,
    /// The text tool's lines, or the stamp's emoji.
    pub text: String,
    pub color: Rgb,
    pub line_width: f64,
    pub fill: FillMode,
    pub censor: CensorMode,
    /// The badge's number.
    pub number: u32,
    pub font_size: f64,
}

impl Annotation {
    /// An annotation with the defaults: red, 4 pixels wide, stroked, pixelated,
    /// number 1, text at 28 pixels.
    pub fn new(id: u64, tool: Tool) -> Annotation {
        Annotation {
            id,
            tool,
            start: Point::default(),
            end: Point::default(),
            points: Vec::new(),
            text: String::new(),
            color: PALETTE[0].1,
            line_width: 4.0,
            fill: FillMode::Stroke,
            censor: CensorMode::Pixelate,
            number: 1,
            font_size: 28.0,
        }
    }

    /// The rectangle from `start` to `end`, whichever corners they are.
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.start.x.min(self.end.x),
            y: self.start.y.min(self.end.y),
            width: (self.end.x - self.start.x).abs(),
            height: (self.end.y - self.start.y).abs(),
        }
    }

    pub fn badge_radius(&self) -> f64 {
        14.0 + self.line_width * 2.0
    }

    /// The square a stamp takes, three times its font size, centered on `start`.
    pub fn stamp_bounds(&self) -> Rect {
        let side = self.font_size * 3.0;
        Rect {
            x: self.start.x - side / 2.0,
            y: self.start.y - side / 2.0,
            width: side,
            height: side,
        }
    }

    /// The text's box: as wide as its widest line in bold, a line 1.25 times the font size
    /// high, from the font size above the first baseline.
    pub fn text_bounds(&self, text: &mut Text, families: &[String]) -> Rect {
        let lines: Vec<_> = self.text.split('\n').collect();
        let width = (lines.iter())
            .map(|line| line_width(text, families, Weight::Bold, line, self.font_size))
            .fold(0.0, f64::max);
        Rect {
            x: self.start.x,
            y: self.start.y - self.font_size,
            width,
            height: lines.len() as f64 * self.font_size * 1.25,
        }
    }

    /// The annotation moved by `d`.
    pub fn moved(&self, d: Point) -> Annotation {
        let shift = |p: Point| Point {
            x: p.x + d.x,
            y: p.y + d.y,
        };
        Annotation {
            start: shift(self.start),
            end: shift(self.end),
            points: self.points.iter().map(|&p| shift(p)).collect(),
            ..self.clone()
        }
    }

    /// The annotation with handle `handle` of [`Annotation::handles`] dragged to `to`. A
    /// rectangle's `start` and `end` become the corners its handles name, so a handle dragged
    /// past the opposite edge turns it inside out, which [`Annotation::rect`] puts right.
    pub fn resized(&self, handle: usize, to: Point) -> Annotation {
        let mut out = self.clone();
        match self.tool {
            Tool::Arrow if handle == 0 => out.start = to,
            Tool::Arrow => out.end = to,
            Tool::Rect | Tool::Ellipse | Tool::Censor | Tool::Spotlight => {
                let r = self.rect();
                let (mut x0, mut y0, mut x1, mut y1) = (r.x, r.y, r.max_x(), r.max_y());
                match handle {
                    0 => (x0, y0) = (to.x, to.y),
                    1 => (x1, y0) = (to.x, to.y),
                    2 => (x0, y1) = (to.x, to.y),
                    _ => (x1, y1) = (to.x, to.y),
                }
                out.start = Point { x: x0, y: y0 };
                out.end = Point { x: x1, y: y1 };
            }
            _ => {}
        }
        out
    }

    /// Whether `at` touches the annotation, within the line's width or 10 pixels, and for
    /// the marker within its drawn width.
    pub fn hit(&self, at: Point, text: &mut Text, families: &[String]) -> bool {
        let slop = if self.tool == Tool::Marker {
            self.line_width * 2.5
        } else {
            self.line_width
        }
        .max(10.0);
        match self.tool {
            Tool::Arrow => segment_distance(at, self.start, self.end) < slop,
            Tool::Rect | Tool::Ellipse | Tool::Censor | Tool::Spotlight => {
                let outer = inset(self.rect(), -slop);
                let solid = self.fill != FillMode::Stroke
                    || matches!(self.tool, Tool::Censor | Tool::Spotlight);
                if solid {
                    return contains(outer, at);
                }
                // a stroked shape is hollow, as a stroke of the rectangle's own
                let inner = inset(self.rect(), slop);
                let hollow = inner.width > 0.0 && inner.height > 0.0 && contains(inner, at);
                contains(outer, at) && !hollow
            }
            Tool::Pencil | Tool::Marker => {
                (self.points.windows(2)).any(|pair| segment_distance(at, pair[0], pair[1]) < slop)
            }
            Tool::Text => contains(inset(self.text_bounds(text, families), -slop), at),
            Tool::Badge => distance(at, self.start) < self.badge_radius() + slop,
            Tool::Stamp => contains(self.stamp_bounds(), at),
            Tool::Select => false,
        }
    }

    /// What the selection's dashed box goes around.
    pub fn bounds(&self, text: &mut Text, families: &[String]) -> Rect {
        match self.tool {
            Tool::Text => self.text_bounds(text, families),
            Tool::Badge => {
                let r = self.badge_radius();
                Rect {
                    x: self.start.x - r,
                    y: self.start.y - r,
                    width: 2.0 * r,
                    height: 2.0 * r,
                }
            }
            Tool::Stamp => self.stamp_bounds(),
            Tool::Pencil | Tool::Marker if !self.points.is_empty() => {
                let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
                let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                for p in &self.points {
                    (x0, y0, x1, y1) = (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y));
                }
                Rect {
                    x: x0,
                    y: y0,
                    width: x1 - x0,
                    height: y1 - y0,
                }
            }
            _ => self.rect(),
        }
    }

    /// The points that resize it: an arrow's two ends; a rectangle's corners, top left, top
    /// right, bottom left, bottom right; nothing for the others.
    pub fn handles(&self) -> Vec<Point> {
        match self.tool {
            Tool::Arrow => vec![self.start, self.end],
            Tool::Rect | Tool::Ellipse | Tool::Censor | Tool::Spotlight => {
                let r = self.rect();
                let corner = |x, y| Point { x, y };
                vec![
                    corner(r.x, r.y),
                    corner(r.max_x(), r.y),
                    corner(r.x, r.max_y()),
                    corner(r.max_x(), r.max_y()),
                ]
            }
            _ => Vec::new(),
        }
    }
}

/// The width of a line of annotation text at `size`; while the fonts load, an estimate of
/// 0.6 em a character.
pub(crate) fn line_width(
    text: &mut Text,
    families: &[String],
    weight: Weight,
    line: &str,
    size: f64,
) -> f64 {
    match text.sans(families, weight) {
        Some(mut sans) => f64::from(sans.width(line, size as f32)),
        None => 0.6 * size * line.chars().count() as f64,
    }
}

pub(crate) fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// The distance from `p` to the segment from `a` to `b`.
fn segment_distance(p: Point, a: Point, b: Point) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length2 = dx * dx + dy * dy;
    if length2 <= 0.0 {
        return distance(p, a);
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / length2).clamp(0.0, 1.0);
    distance(
        p,
        Point {
            x: a.x + t * dx,
            y: a.y + t * dy,
        },
    )
}

/// `r` shrunk by `d` on each side, or grown where `d` is negative.
pub(crate) fn inset(r: Rect, d: f64) -> Rect {
    Rect {
        x: r.x + d,
        y: r.y + d,
        width: r.width - 2.0 * d,
        height: r.height - 2.0 * d,
    }
}

/// Whether `r` holds `p`, its left and top edges in and its right and bottom ones out.
pub(crate) fn contains(r: Rect, p: Point) -> bool {
    r.x <= p.x && p.x < r.max_x() && r.y <= p.y && p.y < r.max_y()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{families, text};

    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn shape(tool: Tool, start: Point, end: Point) -> Annotation {
        Annotation {
            start,
            end,
            ..Annotation::new(1, tool)
        }
    }

    /// Which of `points` hit `a`.
    fn hits(a: &Annotation, points: &[Point]) -> Vec<bool> {
        let (mut text, families) = (text(), families());
        points
            .iter()
            .map(|&at| a.hit(at, &mut text, &families))
            .collect()
    }

    #[test]
    fn an_arrow_is_hit_within_its_width_or_ten_pixels() {
        let mut arrow = shape(Tool::Arrow, p(0.0, 0.0), p(100.0, 0.0));
        let near = [p(50.0, 9.9), p(50.0, 10.0), p(109.0, 0.0), p(-11.0, 0.0)];
        assert_eq!(hits(&arrow, &near), [true, false, true, false]);
        arrow.line_width = 16.0;
        assert_eq!(hits(&arrow, &[p(50.0, 15.0), p(50.0, 16.0)]), [true, false]);
    }

    #[test]
    fn a_stroked_rectangle_is_hollow_and_a_filled_one_is_not() {
        let mut rect = shape(Tool::Rect, p(100.0, 50.0), p(0.0, 0.0));
        let points = [
            p(50.0, 25.0),
            p(5.0, 25.0),
            p(-10.0, 25.0),
            p(-10.5, 25.0),
            p(50.0, 59.0),
        ];
        assert_eq!(hits(&rect, &points), [false, true, true, false, true]);
        rect.fill = FillMode::StrokeFill;
        assert_eq!(hits(&rect, &points), [true, true, true, false, true]);
        // an ellipse goes by its rectangle, and a censor and a spotlight are always solid
        for tool in [Tool::Ellipse, Tool::Censor, Tool::Spotlight] {
            let hollow = tool == Tool::Ellipse;
            let a = shape(tool, p(0.0, 0.0), p(100.0, 50.0));
            assert_eq!(hits(&a, &[p(50.0, 25.0)]), [!hollow], "{tool:?}");
        }
        // too small for a hollow: the whole of it hits
        let small = shape(Tool::Rect, p(0.0, 0.0), p(15.0, 15.0));
        assert_eq!(hits(&small, &[p(7.0, 7.0)]), [true]);
    }

    #[test]
    fn a_marker_is_hit_within_its_wider_stroke() {
        let mut marker = Annotation::new(1, Tool::Marker);
        marker.points = vec![p(0.0, 0.0), p(100.0, 0.0), p(100.0, 100.0)];
        marker.line_width = 8.0;
        // 2.5 times the width, where the pencil goes by the width
        assert_eq!(
            hits(&marker, &[p(50.0, 19.0), p(50.0, 21.0)]),
            [true, false]
        );
        assert_eq!(
            hits(&marker, &[p(119.0, 50.0)]),
            [true],
            "the second segment"
        );
        let pencil = Annotation {
            tool: Tool::Pencil,
            ..marker.clone()
        };
        assert_eq!(hits(&pencil, &[p(50.0, 9.0), p(50.0, 11.0)]), [true, false]);
        marker.points.truncate(1);
        assert_eq!(hits(&marker, &[p(0.0, 0.0)]), [false], "a single point");
    }

    #[test]
    fn a_badge_a_stamp_and_text_are_hit_inside_their_bounds() {
        let badge = shape(Tool::Badge, p(100.0, 100.0), p(100.0, 100.0));
        // a radius of 22, and 10 more
        assert_eq!(
            hits(&badge, &[p(131.0, 100.0), p(133.0, 100.0)]),
            [true, false]
        );
        let stamp = shape(Tool::Stamp, p(100.0, 100.0), p(100.0, 100.0));
        // a square of 84 around its center, with no slop
        assert_eq!(
            hits(&stamp, &[p(141.0, 141.0), p(143.0, 100.0)]),
            [true, false]
        );
        let mut words = shape(Tool::Text, p(100.0, 100.0), p(100.0, 100.0));
        words.text = "Hello\nsecond line".to_owned();
        let (mut text, families) = (text(), families());
        let bounds = words.text_bounds(&mut text, &families);
        // from 28 above the baseline, two lines of 35
        assert_eq!((bounds.x, bounds.y, bounds.height), (100.0, 72.0, 70.0));
        assert!((150.0..250.0).contains(&bounds.width), "{bounds:?}");
        let right = bounds.max_x();
        let points = [
            p(right + 9.0, 140.0),
            p(right + 11.0, 140.0),
            p(100.0, 63.0),
        ];
        assert_eq!(hits(&words, &points), [true, false, true]);
        assert_eq!(words.bounds(&mut text, &families), bounds);
    }

    #[test]
    fn the_handles_are_an_arrows_ends_and_a_rectangles_corners_in_order() {
        let arrow = shape(Tool::Arrow, p(5.0, 6.0), p(1.0, 2.0));
        assert_eq!(arrow.handles(), [p(5.0, 6.0), p(1.0, 2.0)]);
        let rect = shape(Tool::Censor, p(30.0, 40.0), p(10.0, 20.0));
        let corners = [p(10.0, 20.0), p(30.0, 20.0), p(10.0, 40.0), p(30.0, 40.0)];
        assert_eq!(rect.handles(), corners);
        assert!(
            shape(Tool::Badge, p(1.0, 1.0), p(1.0, 1.0))
                .handles()
                .is_empty()
        );
    }

    #[test]
    fn a_handle_dragged_moves_its_corner_or_end() {
        let rect = shape(Tool::Rect, p(10.0, 20.0), p(30.0, 40.0));
        let moved = |handle| {
            let r = rect.resized(handle, p(0.0, 0.0)).rect();
            (r.x, r.y, r.max_x(), r.max_y())
        };
        assert_eq!(moved(0), (0.0, 0.0, 30.0, 40.0));
        assert_eq!(moved(1), (0.0, 0.0, 10.0, 40.0), "past the left edge");
        assert_eq!(moved(2), (0.0, 0.0, 30.0, 20.0), "past the top edge");
        assert_eq!(moved(3), (0.0, 0.0, 10.0, 20.0));
        let arrow = shape(Tool::Arrow, p(5.0, 6.0), p(1.0, 2.0));
        assert_eq!(arrow.resized(0, p(9.0, 9.0)).start, p(9.0, 9.0));
        assert_eq!(arrow.resized(1, p(9.0, 9.0)).end, p(9.0, 9.0));
        let badge = shape(Tool::Badge, p(1.0, 1.0), p(1.0, 1.0));
        assert_eq!(badge.resized(0, p(9.0, 9.0)), badge, "no handles");
    }

    #[test]
    fn a_move_takes_every_point_along() {
        let mut pencil = shape(Tool::Pencil, p(1.0, 2.0), p(3.0, 4.0));
        pencil.points = vec![p(0.0, 0.0), p(5.0, 5.0)];
        let moved = pencil.moved(p(10.0, -1.0));
        assert_eq!((moved.start, moved.end), (p(11.0, 1.0), p(13.0, 3.0)));
        assert_eq!(moved.points, [p(10.0, -1.0), p(15.0, 4.0)]);
    }
}
