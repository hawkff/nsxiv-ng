//! Drawing an edit: the annotations over the base, onto the canvas at the window's fit with
//! the selection, and onto the base at its own size for the flattened image.

use std::collections::{HashMap, HashSet};

use nsxiv_ng_core::Point;
use nsxiv_ng_core::viewport::Viewport;
use nsxiv_ng_render::Canvas;
use nsxiv_ng_render::image_view::{self, Image, Turn};
use nsxiv_ng_render::text::tiny_skia::{
    self, BlendMode, Color, FillRule, FilterQuality, IntRect, LineCap, LineJoin, Paint,
    PathBuilder, Pixmap, PixmapMut, PixmapPaint, Rect, Shader, Stroke, StrokeDash, Transform,
};
use nsxiv_ng_render::text::{Text, Weight};

use crate::annotation::{inset, line_width};
use crate::censor::{SIGMA, blur, pixelate};
use crate::{Annotation, CensorMode, Edit, FillMode, Rgb, RgbaImage, Tool};

/// What drawing keeps between frames.
#[derive(Default)]
pub(crate) struct Drawn {
    /// The base fitted to the window over the background, as RGBA, and what it was made for:
    /// its size, the base's generation and the background.
    preview: Option<(Pixmap, u64, u32)>,
    /// The frame annotations are drawn onto before it goes to the canvas.
    frame: Option<Pixmap>,
    /// The censored pieces, by annotation, mode, rectangle in pixels and base's generation,
    /// the base's bytes between them at most.
    censored: HashMap<CensorKey, Pixmap>,
    /// The pieces scaled to where they show, over the window's background, the frame's bytes
    /// between them at most.
    shown: HashMap<ShownKey, Pixmap>,
    /// Keeps every piece whatever the budgets, which draws the same frames, only faster: what
    /// a budget changes is compared against it.
    #[cfg(test)]
    pub(crate) unbounded: bool,
}

impl Drawn {
    #[cfg(test)]
    fn unbounded(&self) -> bool {
        self.unbounded
    }

    #[cfg(not(test))]
    fn unbounded(&self) -> bool {
        false
    }
}

type CensorKey = (u64, CensorMode, (i32, i32, u32, u32), u64);

/// What the annotations are drawn with besides their own data.
struct Scene<'a> {
    base: &'a Pixmap,
    generation: u64,
    families: &'a [String],
    censored: &'a mut HashMap<CensorKey, Pixmap>,
    used: HashSet<CensorKey>,
    /// The bytes the pieces kept may hold between them.
    budget: usize,
    /// On screen, the base as fitted, which the censors in motion are made from.
    fitted: Option<Fitted<'a>>,
}

/// The base as fitted to the window over `background`, at `at` in the frame and `scale` frame
/// pixels to an image pixel, and the annotations the pointer is moving.
struct Fitted<'a> {
    pixmap: &'a Pixmap,
    at: (i32, i32),
    scale: f64,
    background: u32,
    moving: Vec<u64>,
    shown: &'a mut HashMap<ShownKey, Pixmap>,
    shown_used: HashSet<ShownKey>,
    /// The bytes the scaled pieces may hold between them.
    shown_budget: usize,
}

/// The bytes `pieces` hold between them.
fn held<K>(pieces: &HashMap<K, Pixmap>) -> usize {
    pieces.values().map(|piece| piece.data().len()).sum()
}

/// A censored piece as it shows: its key, where in the frame, and over which background.
type ShownKey = (CensorKey, (i32, i32, u32, u32), u32);

/// Where `area` of the image shows in the frame through `transform`, a scale and a shift,
/// out to whole pixels.
fn shown(area: IntRect, transform: Transform) -> Option<IntRect> {
    let edge = |v: i32, scale: f32, shift: f32| (v as f32 * scale + shift).round() as i32;
    IntRect::from_ltrb(
        edge(area.left(), transform.sx, transform.tx),
        edge(area.top(), transform.sy, transform.ty),
        edge(area.right(), transform.sx, transform.tx),
        edge(area.bottom(), transform.sy, transform.ty),
    )
}

/// The part `area` of `from`, a pixel of each block of `factor` × `factor`.
fn sampled(from: &Pixmap, area: IntRect, factor: u32) -> Option<Pixmap> {
    let (width, height) = (
        area.width().div_ceil(factor),
        area.height().div_ceil(factor),
    );
    let mut out = Pixmap::new(width, height)?;
    let stride = from.width() as usize;
    let (source, target) = (from.pixels(), out.pixels_mut());
    for y in 0..height {
        let row = (area.y() as u32 + (y * factor + factor / 2).min(area.height() - 1)) as usize;
        for x in 0..width {
            let column =
                (area.x() as u32 + (x * factor + factor / 2).min(area.width() - 1)) as usize;
            target[(y * width + x) as usize] = source[row * stride + column];
        }
    }
    Some(out)
}

/// Writes opaque `piece`, enlarged `factor` times with linear filtering, over `shown` of
/// `target`, a row of it made at a time.
fn enlarge(piece: &Pixmap, factor: u32, target: &mut PixmapMut, shown: IntRect) {
    let (pw, ph) = (piece.width() as usize, piece.height() as usize);
    let (w, h) = (shown.width() as usize, shown.height() as usize);
    let stride = target.width() as usize;
    let source = piece.data();
    // the two pixels a pixel of the output lies between, and the second one's weight of 256
    let between = |at: usize, n: usize| {
        let u = ((at as f32 + 0.5) / factor as f32 - 0.5).max(0.0);
        let first = (u as usize).min(n - 1);
        let weight = ((u - first as f32) * 256.0) as u32;
        (first, (first + 1).min(n - 1), weight.min(256))
    };
    let columns: Vec<_> = (0..w).map(|x| between(x, pw)).collect();
    // a row of the piece widened, each channel times 256
    let widen = |row: usize, out: &mut Vec<[u32; 4]>| {
        let line = &source[row * pw * 4..(row + 1) * pw * 4];
        for (o, &(a, b, weight)) in out.iter_mut().zip(&columns) {
            for c in 0..4 {
                let (a, b) = (u32::from(line[a * 4 + c]), u32::from(line[b * 4 + c]));
                o[c] = a * (256 - weight) + b * weight;
            }
        }
    };
    let (mut upper, mut lower) = (vec![[0; 4]; w], vec![[0; 4]; w]);
    let mut widened = (usize::MAX, usize::MAX);
    let data = target.data_mut();
    for y in 0..h {
        let (a, b, weight) = between(y, ph);
        if widened != (a, b) {
            if widened.1 == a {
                std::mem::swap(&mut upper, &mut lower);
            } else {
                widen(a, &mut upper);
            }
            widen(b, &mut lower);
            widened = (a, b);
        }
        let at = ((shown.y() as usize + y) * stride + shown.x() as usize) * 4;
        for ((p, top), bottom) in data[at..at + w * 4]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(&upper)
            .zip(&lower)
        {
            for c in 0..4 {
                p[c] = ((top[c] * (256 - weight) + bottom[c] * weight + 32768) >> 16) as u8;
            }
        }
    }
}

/// Copies `from` into `to` with its corner at `at`, where they overlap, replacing what is
/// there, color and alpha.
fn copy(from: &Pixmap, to: &mut PixmapMut, at: (i32, i32)) {
    let (fw, tw) = (from.width() as i64, to.width() as i64);
    let x0 = i64::from(at.0).max(0);
    let x1 = (i64::from(at.0) + fw).min(tw);
    if x1 <= x0 {
        return;
    }
    let (source, target) = (from.data(), to.data_mut());
    for y in 0..i64::from(from.height()) {
        let ty = i64::from(at.1) + y;
        if ty < 0 || ty >= target.len() as i64 / (tw * 4) {
            continue;
        }
        let s = ((y * fw + x0 - i64::from(at.0)) * 4) as usize;
        let t = ((ty * tw + x0) * 4) as usize;
        let n = ((x1 - x0) * 4) as usize;
        target[t..t + n].copy_from_slice(&source[s..s + n]);
    }
}

impl Edit {
    /// Draws the edit onto `canvas`, the window's at device `scale`: `background` around the
    /// image fitted at no more than 100% and centered, the annotations and the one being
    /// drawn, and the selection's dashed box and handles in `selection`.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        scale: f64,
        text: &mut Text,
        background: u32,
        selection: Rgb,
    ) {
        let (width, height) = (canvas.width(), canvas.height());
        let (fit, origin) = (self.fit(), self.origin());
        let k = fit * scale;
        let fitted = (
            ((f64::from(self.base.width()) * k).round() as u32).max(1),
            ((f64::from(self.base.height()) * k).round() as u32).max(1),
        );
        let preview = self.preview(fitted, background);
        let Some(mut frame) = (self.drawn.frame.take())
            .filter(|f| (f.width(), f.height()) == (width, height))
            .or_else(|| Pixmap::new(width, height))
        else {
            return;
        };
        let [b, g, r, a] = background.to_le_bytes();
        frame.fill(Color::from_rgba8(r, g, b, a));
        let at = (
            (origin.x * scale).round() as i32,
            (origin.y * scale).round() as i32,
        );
        copy(&preview, &mut frame.as_mut(), at);
        let transform = Transform::from_row(k as f32, 0.0, 0.0, k as f32, at.0 as f32, at.1 as f32);
        let mut all = self.annotations.clone();
        all.extend(self.drawing.clone());
        let moving = self.moving();
        let unbounded = self.drawn.unbounded();
        let budget = |bytes: usize| if unbounded { usize::MAX } else { bytes };
        // pieces kept for a larger frame than this one let go first
        if held(&self.drawn.shown) > budget(frame.data().len()) {
            self.drawn.shown.clear();
        }
        let mut scene = Scene {
            base: &self.base,
            generation: self.generation,
            families: &self.families,
            censored: &mut self.drawn.censored,
            used: HashSet::new(),
            budget: budget(self.base.data().len()),
            fitted: Some(Fitted {
                pixmap: &preview,
                at,
                scale: k,
                background,
                moving,
                shown: &mut self.drawn.shown,
                shown_used: HashSet::new(),
                shown_budget: budget(frame.data().len()),
            }),
        };
        let mut target = frame.as_mut();
        draw_all(&mut target, transform, &all, &mut scene, text);
        let selected = (self.selection).and_then(|id| all.iter().find(|a| a.id == id));
        if let Some(a) = selected {
            draw_selection(
                &mut target,
                transform,
                a,
                1.0 / fit,
                selection,
                &mut scene,
                text,
            );
        }
        // pieces of annotations gone are let go
        let used = std::mem::take(&mut scene.used);
        let shown_used = (scene.fitted.take())
            .map(|f| f.shown_used)
            .unwrap_or_default();
        self.drawn.censored.retain(|key, _| used.contains(key));
        self.drawn.shown.retain(|key, _| shown_used.contains(key));
        for (pixel, p) in canvas
            .pixels_mut()
            .iter_mut()
            .zip(frame.data().as_chunks::<4>().0)
        {
            *pixel = u32::from_le_bytes([p[2], p[1], p[0], p[3]]);
        }
        self.drawn.preview = Some((preview, self.generation, background));
        self.drawn.frame = Some(frame);
    }

    /// The base fitted to `size` over `background`, made again when the size, the base or
    /// the background changed, through the viewer's own scaling.
    fn preview(&mut self, size: (u32, u32), background: u32) -> Pixmap {
        if let Some((preview, generation, color)) = self.drawn.preview.take()
            && (preview.width(), preview.height()) == size
            && (generation, color) == (self.generation, background)
        {
            return preview;
        }
        let mut pixels = vec![background; size.0 as usize * size.1 as usize];
        if let Some(mut canvas) = Canvas::new(&mut pixels, size.0, size.1) {
            let opaque = self.base.pixels().iter().all(|p| p.alpha() == 255);
            // the viewer's scaling takes straight alpha, which an opaque base is already
            let straight;
            let level: &[u8] = if opaque {
                self.base.data()
            } else {
                straight = self.base.clone().take_demultiplied();
                &straight
            };
            let levels = [level];
            let image = Image {
                width: self.base.width(),
                height: self.base.height(),
                levels: &levels,
                opaque,
            };
            let view = Viewport::default();
            image_view::draw(&mut canvas, &image, &view, Turn::default(), None, 1.0, true);
        }
        let rgba = (pixels.iter())
            .flat_map(|p| {
                let [b, g, r, a] = p.to_le_bytes();
                [r, g, b, a]
            })
            .collect();
        let size = nsxiv_ng_render::text::tiny_skia::IntSize::from_wh(size.0, size.1);
        size.and_then(|size| Pixmap::from_vec(rgba, size))
            .unwrap_or_else(|| Pixmap::new(1, 1).expect("a pixel"))
    }

    /// The base with every annotation drawn at its own size: what is saved and copied.
    pub fn flattened(&mut self, text: &mut Text) -> RgbaImage {
        let mut pixmap = self.base.clone();
        let budget = if self.drawn.unbounded() {
            usize::MAX
        } else {
            self.base.data().len()
        };
        let mut scene = Scene {
            base: &self.base,
            generation: self.generation,
            families: &self.families,
            censored: &mut self.drawn.censored,
            used: HashSet::new(),
            budget,
            fitted: None,
        };
        draw_all(
            &mut pixmap.as_mut(),
            Transform::identity(),
            &self.annotations,
            &mut scene,
            text,
        );
        RgbaImage::from_pixmap(pixmap)
    }
}

/// Draws the annotations in order, the dimming of every spotlight first.
fn draw_all(
    target: &mut PixmapMut,
    transform: Transform,
    annotations: &[Annotation],
    scene: &mut Scene,
    text: &mut Text,
) {
    let spots: Vec<_> = annotations
        .iter()
        .filter(|a| a.tool == Tool::Spotlight)
        .collect();
    if !spots.is_empty() {
        // the image less each spot, even-odd: where two spots overlap, it dims again
        let mut path = PathBuilder::new();
        let (w, h) = (scene.base.width() as f32, scene.base.height() as f32);
        path.push_rect(Rect::from_xywh(0.0, 0.0, w, h).expect("a base"));
        for spot in spots {
            if let Some(r) = rect(spot) {
                path.push_rect(r);
            }
        }
        if let Some(path) = path.finish() {
            let dim = paint(Color::from_rgba(0.0, 0.0, 0.0, 0.55).expect("a color"));
            target.fill_path(&path, &dim, FillRule::EvenOdd, transform, None);
        }
    }
    for a in annotations {
        draw(target, transform, a, scene, text);
    }
}

fn color(rgb: Rgb, alpha: f32) -> Color {
    let mut color = Color::from_rgba8(rgb.0, rgb.1, rgb.2, 255);
    color.apply_opacity(alpha);
    color
}

fn paint(color: Color) -> Paint<'static> {
    Paint {
        shader: Shader::SolidColor(color),
        anti_alias: true,
        ..Paint::default()
    }
}

/// A stroke: round joins, and butt caps unless it says otherwise.
fn stroke(width: f64, cap: LineCap) -> Stroke {
    Stroke {
        width: width as f32,
        line_cap: cap,
        line_join: LineJoin::Round,
        ..Stroke::default()
    }
}

fn rect(a: &Annotation) -> Option<Rect> {
    let r = a.rect();
    Rect::from_xywh(r.x as f32, r.y as f32, r.width as f32, r.height as f32)
}

/// A rectangle's path whose dashes start from the bottom right corner along the bottom, then
/// go up the left side, fitted to the reference frames.
fn dashed_rect(r: Rect) -> tiny_skia::Path {
    let mut path = PathBuilder::new();
    path.move_to(r.right(), r.bottom());
    path.line_to(r.left(), r.bottom());
    path.line_to(r.left(), r.top());
    path.line_to(r.right(), r.top());
    path.close();
    path.finish().unwrap_or_else(|| PathBuilder::from_rect(r))
}

fn point(path: &mut PathBuilder, p: Point, line: bool) {
    if line {
        path.line_to(p.x as f32, p.y as f32);
    } else {
        path.move_to(p.x as f32, p.y as f32);
    }
}

fn draw(
    target: &mut PixmapMut,
    transform: Transform,
    a: &Annotation,
    scene: &mut Scene,
    text: &mut Text,
) {
    let solid = paint(color(a.color, 1.0));
    match a.tool {
        Tool::Arrow => {
            let (dx, dy) = (a.end.x - a.start.x, a.end.y - a.start.y);
            let length = dx.hypot(dy);
            if length <= 1.0 {
                return;
            }
            let (ux, uy) = (dx / length, dy / length);
            let head = (a.line_width * 4.0).max(12.0);
            let base = Point {
                x: a.end.x - ux * head,
                y: a.end.y - uy * head,
            };
            let mut shaft = PathBuilder::new();
            point(&mut shaft, a.start, false);
            point(&mut shaft, base, true);
            if let Some(shaft) = shaft.finish() {
                let round = stroke(a.line_width, LineCap::Round);
                target.stroke_path(&shaft, &solid, &round, transform, None);
            }
            let (px, py) = (-uy * head * 0.5, ux * head * 0.5);
            let mut tip = PathBuilder::new();
            point(&mut tip, a.end, false);
            point(
                &mut tip,
                Point {
                    x: base.x + px,
                    y: base.y + py,
                },
                true,
            );
            point(
                &mut tip,
                Point {
                    x: base.x - px,
                    y: base.y - py,
                },
                true,
            );
            tip.close();
            if let Some(tip) = tip.finish() {
                target.fill_path(&tip, &solid, FillRule::Winding, transform, None);
            }
        }
        Tool::Rect | Tool::Ellipse => {
            let Some(r) = rect(a) else { return };
            let path = if a.tool == Tool::Rect {
                Some(PathBuilder::from_rect(r))
            } else {
                PathBuilder::from_oval(r)
            };
            let Some(path) = path else { return };
            let outline = stroke(a.line_width, LineCap::Butt);
            match a.fill {
                FillMode::Stroke => target.stroke_path(&path, &solid, &outline, transform, None),
                FillMode::StrokeFill => {
                    let fill = paint(color(a.color, 0.3));
                    target.fill_path(&path, &fill, FillRule::Winding, transform, None);
                    target.stroke_path(&path, &solid, &outline, transform, None);
                }
                FillMode::Fill => {
                    target.fill_path(&path, &solid, FillRule::Winding, transform, None)
                }
            }
        }
        Tool::Pencil | Tool::Marker => {
            let p = &a.points;
            if p.len() < 2 {
                return;
            }
            // smoothed: quadratic curves through the midpoints
            let mut path = PathBuilder::new();
            point(&mut path, p[0], false);
            for pair in p[1..].windows(2) {
                let mid = ((pair[0].x + pair[1].x) / 2.0, (pair[0].y + pair[1].y) / 2.0);
                path.quad_to(
                    pair[0].x as f32,
                    pair[0].y as f32,
                    mid.0 as f32,
                    mid.1 as f32,
                );
            }
            point(&mut path, p[p.len() - 1], true);
            let Some(path) = path.finish() else { return };
            let (width, mut ink) = if a.tool == Tool::Marker {
                (a.line_width * 5.0, paint(color(a.color, 0.45)))
            } else {
                (a.line_width, solid)
            };
            if a.tool == Tool::Marker {
                ink.blend_mode = BlendMode::Multiply;
            }
            target.stroke_path(&path, &ink, &stroke(width, LineCap::Round), transform, None);
        }
        Tool::Text => {
            let Some(mut sans) = text.sans(scene.families, Weight::Bold) else {
                return;
            };
            for (i, line) in a.text.split('\n').enumerate() {
                let y = a.start.y + i as f64 * a.font_size * 1.25;
                let origin = (a.start.x as f32, y as f32);
                sans.draw(
                    target,
                    line,
                    a.font_size as f32,
                    origin,
                    color(a.color, 1.0),
                    transform,
                );
            }
        }
        Tool::Badge => {
            let r = a.badge_radius();
            let circle = Rect::from_xywh(
                (a.start.x - r) as f32,
                (a.start.y - r) as f32,
                (2.0 * r) as f32,
                (2.0 * r) as f32,
            );
            if let Some(circle) = circle.and_then(PathBuilder::from_oval) {
                target.fill_path(&circle, &solid, FillRule::Winding, transform, None);
            }
            let number = a.number.to_string();
            let width = line_width(text, scene.families, Weight::Bold, &number, r);
            let Some(mut sans) = text.sans(scene.families, Weight::Bold) else {
                return;
            };
            let origin = (
                (a.start.x - width / 2.0) as f32,
                (a.start.y + r * 0.35) as f32,
            );
            sans.draw(target, &number, r as f32, origin, Color::WHITE, transform);
        }
        Tool::Stamp => {
            let Some(mut sans) = text.sans(scene.families, Weight::Regular) else {
                return;
            };
            let size = a.font_size * 2.4;
            let origin = (
                (a.start.x - a.font_size * 1.5) as f32,
                (a.start.y + a.font_size) as f32,
            );
            sans.draw(
                target,
                &a.text,
                size as f32,
                origin,
                color(a.color, 1.0),
                transform,
            );
        }
        Tool::Censor => censor(target, transform, a, scene),
        Tool::Spotlight => {
            let Some(r) = rect(a) else { return };
            let mut dashed = stroke(a.line_width, LineCap::Butt);
            dashed.dash = StrokeDash::new(vec![6.0, 4.0], 0.0);
            target.stroke_path(&dashed_rect(r), &solid, &dashed, transform, None);
        }
        Tool::Select => {}
    }
}

/// Covers the annotation's rectangle, on the image and out to whole pixels, solid, or with
/// the base under it pixelated or blurred, a piece kept while its annotation stays the same.
fn censor(target: &mut PixmapMut, transform: Transform, a: &Annotation, scene: &mut Scene) {
    let r = a.rect();
    let (w, h) = (
        f64::from(scene.base.width()),
        f64::from(scene.base.height()),
    );
    let (x0, y0) = (r.x.max(0.0).floor(), r.y.max(0.0).floor());
    let (x1, y1) = (r.max_x().min(w).ceil(), r.max_y().min(h).ceil());
    let Some(area) = IntRect::from_ltrb(x0 as i32, y0 as i32, x1 as i32, y1 as i32) else {
        return;
    };
    if a.censor == CensorMode::Solid {
        target.fill_rect(area.to_rect(), &paint(color(a.color, 1.0)), transform, None);
        return;
    }
    if let Some(fitted) = &scene.fitted
        && fitted.moving.contains(&a.id)
    {
        // while the pointer moves it, made from the image as fitted, in a few milliseconds
        // where the full-size piece of the whole image takes tenths of a second
        let fitted_area = IntRect::from_xywh(
            fitted.at.0,
            fitted.at.1,
            fitted.pixmap.width(),
            fitted.pixmap.height(),
        );
        // cut to the frame too, which a canvas not yet the view's size leaves smaller
        let frame = IntRect::from_xywh(0, 0, target.width(), target.height());
        let shown = shown(area, transform)
            .zip(fitted_area)
            .and_then(|(s, f)| s.intersect(&f))
            .zip(frame)
            .and_then(|(s, f)| s.intersect(&f));
        let Some(shown) = shown else { return };
        let from = shown.translate(-fitted.at.0, -fitted.at.1);
        // a quarter of a megapixel at most, enlarged again: a blur and block means hide that
        let pixels = u64::from(shown.width()) * u64::from(shown.height());
        let factor = ((pixels as f64 / 250_000.0).sqrt().ceil() as u32).max(1);
        let Some(mut piece) = from.and_then(|from| sampled(fitted.pixmap, from, factor)) else {
            return;
        };
        match a.censor {
            CensorMode::Pixelate => pixelate(&mut piece),
            _ => blur(&mut piece, SIGMA * fitted.scale / f64::from(factor)),
        }
        enlarge(&piece, factor, target, shown);
        return;
    }
    let key = (
        a.id,
        a.censor,
        (area.x(), area.y(), area.width(), area.height()),
        scene.generation,
    );
    // a piece that would pass what the pieces kept may hold is made, drawn and dropped
    let mut made = None;
    if !scene.censored.contains_key(&key) {
        let Some(mut piece) = scene.base.clone_rect(area) else {
            return;
        };
        match a.censor {
            CensorMode::Pixelate => pixelate(&mut piece),
            _ => blur(&mut piece, SIGMA),
        }
        if held(scene.censored) + piece.data().len() <= scene.budget {
            scene.censored.insert(key, piece);
        } else {
            made = Some(piece);
        }
    }
    let piece = match &made {
        Some(piece) => piece,
        None => {
            scene.used.insert(key);
            &scene.censored[&key]
        }
    };
    let Some(fitted) = &mut scene.fitted else {
        // at the base's own size, in place of what is there, color and alpha: a censor over a
        // translucent base must not let the base show through
        let at = transform.pre_translate(area.x() as f32, area.y() as f32);
        let paint = PixmapPaint {
            blend_mode: BlendMode::Source,
            quality: FilterQuality::Nearest,
            ..PixmapPaint::default()
        };
        target.draw_pixmap(0, 0, piece.as_ref(), &paint, at, None);
        return;
    };
    // on screen, scaled to where it shows over the background once, as the fitted base shows,
    // so it is opaque, and copied from then on in place of what is under it
    let Some(device) = shown(area, transform) else {
        return;
    };
    let key = (
        key,
        (device.x(), device.y(), device.width(), device.height()),
        fitted.background,
    );
    let mut scaled_made = None;
    if !fitted.shown.contains_key(&key) {
        let Some(mut scaled) = Pixmap::new(device.width(), device.height()) else {
            return;
        };
        let [b, g, r, a] = fitted.background.to_le_bytes();
        scaled.fill(Color::from_rgba8(r, g, b, a));
        let (sx, sy) = (
            device.width() as f32 / piece.width() as f32,
            device.height() as f32 / piece.height() as f32,
        );
        let paint = PixmapPaint {
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        let fit = Transform::from_scale(sx, sy);
        scaled.draw_pixmap(0, 0, piece.as_ref(), &paint, fit, None);
        if held(fitted.shown) + scaled.data().len() <= fitted.shown_budget {
            fitted.shown.insert(key, scaled);
        } else {
            scaled_made = Some(scaled);
        }
    }
    let scaled = match &scaled_made {
        Some(scaled) => scaled,
        None => {
            fitted.shown_used.insert(key);
            &fitted.shown[&key]
        }
    };
    copy(scaled, target, (device.x(), device.y()));
}

/// The selection's box, dashed, 4 pixels of the screen outside its bounds, and its handles as
/// white disks ringed in `color`; `scale` is image pixels to a pixel of the screen.
fn draw_selection(
    target: &mut PixmapMut,
    transform: Transform,
    a: &Annotation,
    scale: f64,
    rgb: Rgb,
    scene: &mut Scene,
    text: &mut Text,
) {
    let ink = paint(color(rgb, 1.0));
    let b = inset(a.bounds(text, scene.families), -4.0 * scale);
    let mut line = stroke(1.5 * scale, LineCap::Butt);
    if let Some(r) = Rect::from_xywh(b.x as f32, b.y as f32, b.width as f32, b.height as f32) {
        line.dash = StrokeDash::new(vec![(4.0 * scale) as f32, (3.0 * scale) as f32], 0.0);
        target.stroke_path(&dashed_rect(r), &ink, &line, transform, None);
    }
    line.dash = None;
    let r = 4.5 * scale;
    for h in a.handles() {
        let disk = Rect::from_xywh(
            (h.x - r) as f32,
            (h.y - r) as f32,
            (2.0 * r) as f32,
            (2.0 * r) as f32,
        );
        if let Some(disk) = disk.and_then(PathBuilder::from_oval) {
            target.fill_path(
                &disk,
                &paint(Color::WHITE),
                FillRule::Winding,
                transform,
                None,
            );
            target.stroke_path(&disk, &ink, &line, transform, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nsxiv_ng_core::{Point, Size};
    use nsxiv_ng_render::canvas::rgb;
    use nsxiv_ng_render::image_view::{Image, Turn};
    use nsxiv_ng_render::text::Text;

    use crate::testing::{families, text};
    use crate::{Annotation, CensorMode, Edit, Rgb, RgbaImage, Tool};

    /// An edit of a `width` × `height` image of `pixel`.
    fn edit_of(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> (Edit, Vec<u8>) {
        let rgba: Vec<u8> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .flat_map(|(x, y)| pixel(x, y))
            .collect();
        let levels = [rgba.as_slice()];
        let image = Image {
            width,
            height,
            levels: &levels,
            opaque: rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
        };
        let source = PathBuf::from("a.png");
        let edit = Edit::new(&image, Turn::default(), None, u64::MAX, source, families()).unwrap();
        (edit, rgba)
    }

    fn noise(x: u32, y: u32) -> [u8; 4] {
        let v = (x * 7919 + y * 104_729) % 251;
        [v as u8, (v * 3 % 256) as u8, (255 - v) as u8, 255]
    }

    /// Red and blue pixels alternating, each half clear.
    fn checker(x: u32, y: u32) -> [u8; 4] {
        if (x + y).is_multiple_of(2) {
            [255, 0, 0, 128]
        } else {
            [0, 0, 255, 128]
        }
    }

    /// A pixelate censor over the left half of a 64 × 48 checker, and a blur over the right.
    fn censored_checker() -> Edit {
        let (mut edit, _) = edit_of(64, 48, checker);
        let mut blurred = shape(2, Tool::Censor, (32.0, 0.0), (64.0, 48.0));
        blurred.censor = CensorMode::Blur;
        edit.annotations = vec![shape(1, Tool::Censor, (0.0, 0.0), (32.0, 48.0)), blurred];
        edit
    }

    /// Asserts each 8 × 8 block of the left half is one value, alpha included, and neighbors
    /// in the right half are a few levels apart at most, in pixels of 64 × 48.
    fn assert_censored(at: impl Fn(u32, u32) -> [u8; 4], what: &str) {
        for y in 0..48 {
            for x in 0..32 {
                let corner = at(x / 8 * 8, y / 8 * 8);
                assert_eq!(at(x, y), corner, "{what}: pixelated at {x}, {y}");
            }
            for x in 33..64 {
                let (a, b) = (at(x - 1, y), at(x, y));
                let apart = (0..4).map(|c| a[c].abs_diff(b[c])).max().unwrap();
                assert!(apart <= 4, "{what}: blurred at {x}, {y}: {a:?} {b:?}");
            }
        }
    }

    #[test]
    fn censors_keep_no_more_than_the_bases_bytes_and_draw_what_they_would_keeping_all() {
        // three censors over the whole image, each a piece of the base's size
        let censors = |edit: &mut Edit| {
            let mut blurred = shape(2, Tool::Censor, (0.0, 0.0), (64.0, 48.0));
            blurred.censor = CensorMode::Blur;
            edit.annotations = vec![
                shape(1, Tool::Censor, (0.0, 0.0), (64.0, 48.0)),
                blurred,
                shape(3, Tool::Censor, (16.0, 0.0), (64.0, 48.0)),
            ];
            edit.resize(Size {
                width: 64.0,
                height: 48.0,
            });
        };
        let mut text = text();
        let frames = |edit: &mut Edit, text: &mut Text| {
            (0..2)
                .map(|_| {
                    let mut pixels = vec![0; 64 * 48];
                    let mut canvas = nsxiv_ng_render::Canvas::new(&mut pixels, 64, 48).unwrap();
                    edit.draw(&mut canvas, 1.0, text, rgb(0, 0, 0), Rgb(0, 0, 255));
                    pixels
                })
                .collect::<Vec<_>>()
        };
        let (mut kept, base) = edit_of(64, 48, noise);
        censors(&mut kept);
        let bounded = frames(&mut kept, &mut text);
        let held = |edit: &Edit| {
            let kept: usize = edit.drawn.censored.values().map(|p| p.data().len()).sum();
            let shown: usize = edit.drawn.shown.values().map(|p| p.data().len()).sum();
            (kept, shown)
        };
        let (pieces, scaled) = held(&kept);
        assert!(
            pieces <= base.len() && scaled <= 64 * 48 * 4,
            "{pieces} {scaled}"
        );
        let flat = kept.flattened(&mut text);
        assert!(held(&kept).0 <= base.len());
        let (mut all, _) = edit_of(64, 48, noise);
        all.drawn.unbounded = true;
        censors(&mut all);
        assert!(frames(&mut all, &mut text) == bounded);
        assert!(held(&all).0 > base.len(), "all three kept");
        assert!(all.flattened(&mut text) == flat);
    }

    #[test]
    fn a_censor_in_motion_stays_inside_a_frame_smaller_than_the_view() {
        let mut text = text();
        for (view, mode) in [
            (Some(200.0), CensorMode::Pixelate),
            (Some(200.0), CensorMode::Blur),
            (None, CensorMode::Pixelate),
            (None, CensorMode::Blur),
        ] {
            let (mut edit, _) = edit_of(200, 150, noise);
            if let Some(width) = view {
                edit.resize(Size {
                    width,
                    height: 150.0,
                });
            }
            // across the far corner of the image, which the 100 × 80 canvas leaves out
            let mut censor = shape(1, Tool::Censor, (60.0, 40.0), (200.0, 150.0));
            censor.censor = mode;
            edit.drawing = Some(censor);
            let mut pixels = vec![0; 100 * 80];
            let mut canvas = nsxiv_ng_render::Canvas::new(&mut pixels, 100, 80).unwrap();
            edit.draw(&mut canvas, 1.0, &mut text, rgb(0, 0, 0), Rgb(0, 0, 255));
        }
    }

    #[test]
    fn scaled_pieces_keep_to_the_budget_of_a_frame_that_shrank() {
        let (mut edit, _) = edit_of(64, 48, noise);
        let mut blurred = shape(2, Tool::Censor, (0.0, 0.0), (64.0, 48.0));
        blurred.censor = CensorMode::Blur;
        edit.annotations = vec![shape(1, Tool::Censor, (0.0, 0.0), (64.0, 48.0)), blurred];
        let mut text = text();
        let held =
            |edit: &Edit| -> usize { (edit.drawn.shown.values()).map(|p| p.data().len()).sum() };
        // 128 wide, then 127: the image at 32 either way, its pieces under the same keys
        for width in [128, 127] {
            edit.resize(Size {
                width: f64::from(width),
                height: 48.0,
            });
            let mut pixels = vec![0; width as usize * 48];
            let mut canvas = nsxiv_ng_render::Canvas::new(&mut pixels, width, 48).unwrap();
            edit.draw(&mut canvas, 1.0, &mut text, rgb(0, 0, 0), Rgb(0, 0, 255));
            let frame = width as usize * 48 * 4;
            assert!(held(&edit) <= frame, "{width}: {} of {frame}", held(&edit));
        }
    }

    #[test]
    fn a_censor_over_a_translucent_base_replaces_what_is_under_it() {
        let flat = censored_checker().flattened(&mut text());
        assert_censored(|x, y| pixel(&flat, x, y), "flattened");
        // on screen, over the background as the base shows there
        let mut edit = censored_checker();
        edit.resize(Size {
            width: 64.0,
            height: 48.0,
        });
        let mut pixels = vec![0; 64 * 48];
        let mut canvas = nsxiv_ng_render::Canvas::new(&mut pixels, 64, 48).unwrap();
        edit.draw(
            &mut canvas,
            1.0,
            &mut text(),
            rgb(0, 128, 0),
            Rgb(0, 0, 255),
        );
        let at = |x: u32, y: u32| pixels[(y * 64 + x) as usize].to_le_bytes();
        assert_censored(at, "on screen");
    }

    fn shape(id: u64, tool: Tool, start: (f64, f64), end: (f64, f64)) -> Annotation {
        let point = |(x, y)| Point { x, y };
        Annotation {
            start: point(start),
            end: point(end),
            ..Annotation::new(id, tool)
        }
    }

    fn pixel(image: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
        let at = (y * image.width + x) as usize * 4;
        image.rgba[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn the_flattened_image_is_the_base_where_nothing_was_drawn() {
        let (mut edit, base) = edit_of(64, 48, noise);
        edit.annotations
            .push(shape(1, Tool::Arrow, (10.0, 10.0), (30.0, 20.0)));
        let flat = edit.flattened(&mut text());
        assert_eq!((flat.width, flat.height), (64, 48));
        let row = 64 * 4;
        assert_eq!(flat.rgba[35 * row..], base[35 * row..], "below the arrow");
        assert_ne!(flat.rgba[..30 * row], base[..30 * row], "the arrow");
    }

    #[test]
    fn pixelate_leaves_no_row_of_its_rectangle_as_the_base_had_it() {
        let (mut edit, base) = edit_of(64, 48, noise);
        edit.annotations
            .push(shape(1, Tool::Censor, (60.0, 45.0), (5.0, 5.0)));
        let flat = edit.flattened(&mut text());
        for y in 5..45 {
            let row = |rgba: &[u8]| rgba[(y * 64 + 5) * 4..(y * 64 + 60) * 4].to_vec();
            assert_ne!(row(&flat.rgba), row(&base), "row {y}");
        }
        // blocks of 8, from the corner: one color across a block, another past its edge
        assert_eq!(pixel(&flat, 5, 5), pixel(&flat, 12, 12));
        assert_ne!(pixel(&flat, 12, 5), pixel(&flat, 13, 5));
    }

    #[test]
    fn blur_and_solid_cover_their_rectangle() {
        let (mut edit, base) = edit_of(64, 48, noise);
        let mut blurred = shape(1, Tool::Censor, (8.0, 8.0), (40.0, 40.0));
        blurred.censor = CensorMode::Blur;
        let mut solid = shape(2, Tool::Censor, (44.0, 8.0), (60.0, 40.0));
        solid.censor = CensorMode::Solid;
        edit.annotations = vec![blurred, solid];
        let flat = edit.flattened(&mut text());
        let (a, b) = (pixel(&flat, 20, 20), pixel(&flat, 21, 20));
        // neighbors a blur makes alike, which the noise is not
        assert!((0..3).all(|c| a[c].abs_diff(b[c]) < 8), "{a:?} {b:?}");
        assert_ne!(base[(20 * 64 + 20) * 4..][..4], a);
        assert_eq!(pixel(&flat, 50, 20), [255, 56, 60, 255]);
    }

    #[test]
    fn two_spotlights_that_overlap_dim_the_overlap_again() {
        let gray = [200, 200, 200, 255];
        let (mut edit, _) = edit_of(64, 48, |_, _| gray);
        edit.annotations = vec![
            shape(1, Tool::Spotlight, (4.0, 4.0), (30.0, 30.0)),
            shape(2, Tool::Spotlight, (20.0, 20.0), (50.0, 40.0)),
        ];
        let flat = edit.flattened(&mut text());
        let dimmed = [90, 90, 90, 255];
        assert_eq!(pixel(&flat, 12, 12), gray, "in one spot");
        assert_eq!(pixel(&flat, 25, 25), dimmed, "in both");
        assert_eq!(pixel(&flat, 58, 44), dimmed, "in none");
    }

    #[test]
    fn a_censor_moving_is_made_from_the_image_as_fitted_and_shows_alike_at_full_size() {
        let mut text = text();
        for mode in [CensorMode::Pixelate, CensorMode::Blur] {
            let (mut edit, base) = edit_of(200, 150, noise);
            edit.resize(Size {
                width: 200.0,
                height: 150.0,
            });
            let mut censor = shape(1, Tool::Censor, (20.0, 20.0), (180.0, 130.0));
            censor.censor = mode;
            let frame = |edit: &mut Edit, text: &mut Text| {
                let mut pixels = vec![0; 200 * 150];
                let mut canvas = nsxiv_ng_render::Canvas::new(&mut pixels, 200, 150).unwrap();
                edit.draw(&mut canvas, 1.0, text, rgb(0, 0, 0), Rgb(0, 0, 255));
                pixels
            };
            edit.drawing = Some(censor.clone());
            let moving = frame(&mut edit, &mut text);
            let at = |pixels: &[u32], x: usize, y: usize| pixels[y * 200 + x].to_le_bytes();
            let under = &base[(60 * 200 + 60) * 4..][..3];
            assert_ne!(
                at(&moving, 60, 60)[..3],
                [under[2], under[1], under[0]],
                "{mode:?}"
            );
            // at 100%, the image as fitted is the base: the same piece either way
            edit.drawing = None;
            edit.annotations.push(censor);
            assert!(frame(&mut edit, &mut text) == moving, "{mode:?}");
        }
    }

    #[test]
    fn a_frame_fits_the_image_centered_with_what_is_being_drawn_and_the_selection() {
        let (mut edit, _) = edit_of(400, 300, |_, _| [100, 120, 140, 255]);
        let mut text = text();
        // 200 × 300 logical units at a scale of 2: the image at half its size, centered
        edit.resize(Size {
            width: 200.0,
            height: 300.0,
        });
        let mut pixels = vec![0; 400 * 600];
        let mut canvas = nsxiv_ng_render::Canvas::new(&mut pixels, 400, 600).unwrap();
        let (background, selection) = (rgb(10, 20, 30), Rgb(1, 2, 250));
        edit.annotations
            .push(shape(1, Tool::Rect, (40.0, 40.0), (200.0, 200.0)));
        edit.selection = Some(1);
        // an arrow being drawn, from the image's (300, 100) to (380, 280)
        edit.drawing = Some(shape(2, Tool::Arrow, (300.0, 100.0), (380.0, 280.0)));
        edit.draw(&mut canvas, 2.0, &mut text, background, selection);
        let at = |x: usize, y: usize| pixels[y * 400 + x];
        assert_eq!(at(200, 100), background, "above the image");
        assert_eq!(at(200, 500), background, "below it");
        assert_eq!(
            at(100, 300),
            rgb(100, 120, 140),
            "the image, inside the rectangle"
        );
        // the arrow's middle, (340, 190) of the image, is at (340, 190 + 150) on the canvas
        assert_eq!(at(340, 340), rgb(255, 56, 60), "the arrow being drawn");
        assert!(pixels.contains(&rgb(1, 2, 250)), "the selection's color");
        assert!(pixels.contains(&rgb(255, 255, 255)), "its handles");
    }
}
