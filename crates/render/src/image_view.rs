//! The image view: the part of an image the viewport shows, scaled, turned and flipped, over
//! the background or the checkerboard.

mod scale;

use nsxiv_ng_core::viewport::Viewport;

use crate::Colors;
use crate::canvas::{self, Canvas};
use std::sync::Mutex;

use scale::{Axis, Reduced, Reducer, Sampled, Source};

/// An image to draw, as pyramid levels: straight-alpha RGBA rows top to bottom, level 0 the
/// decoded image and each next level ⌈w/2⌉ × ⌈h/2⌉ of the one before. Level 0 alone is a
/// complete source.
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    /// Level 0's size.
    pub width: u32,
    pub height: u32,
    pub levels: &'a [&'a [u8]],
    /// Every alpha of level 0 is 255.
    pub opaque: bool,
}

/// How the image is turned and flipped on screen: `rotation` degrees clockwise, a multiple
/// of 90, then the flips of what the turn shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    pub rotation: u32,
    pub flip_h: bool,
    pub flip_v: bool,
}

/// Draws the image as the viewport places it on the canvas, which is the image area at
/// device `scale`, and returns whether opaque image pixels cover all of it. With `whole`,
/// the image is drawn alone, all of it over the whole canvas. The canvas keeps what lies
/// outside the image; an image with alpha blends over it, onto the checkerboard first when
/// the viewport's alpha layer is on, so a clear canvas keeps the image's alpha. The
/// checkerboard's squares count from the corner of the part drawn, so they stay with the
/// canvas while the image hangs off it.
pub fn draw(
    canvas: &mut Canvas,
    image: &Image,
    view: &Viewport,
    turn: Turn,
    colors: Option<&Colors>,
    scale: f64,
    whole: bool,
) -> bool {
    let (width, height) = (i64::from(canvas.width()), i64::from(canvas.height()));
    let placed = if whole {
        Some(Placement {
            region: (0, 0, width, height),
            origin: (0, 0),
            scaled: (width, height),
        })
    } else {
        placement((canvas.width(), canvas.height()), image, view, turn, scale)
    };
    let Some(placed) = placed else {
        return false;
    };
    let region = placed.region;
    draw_placed(canvas, image, view, turn, colors, scale, placed, region);
    placed.covers(image, view, (width, height))
}

/// Draws the part of the image [`draw`] draws, not whole, that lies in `clip` (x, y, width,
/// height) of the canvas, those pixels exactly as [`draw`] gives them, and leaves the rest of
/// the canvas as it is.
pub fn draw_clipped(
    canvas: &mut Canvas,
    image: &Image,
    view: &Viewport,
    turn: Turn,
    colors: Option<&Colors>,
    scale: f64,
    clip: (i64, i64, i64, i64),
) {
    let Some(placed) = placement((canvas.width(), canvas.height()), image, view, turn, scale)
    else {
        return;
    };
    let (x, y, w, h) = placed.region;
    let (left, top) = (x.max(clip.0), y.max(clip.1));
    let (right, bottom) = ((x + w).min(clip.0 + clip.2), (y + h).min(clip.1 + clip.3));
    if right > left && bottom > top {
        let region = (left, top, right - left, bottom - top);
        draw_placed(canvas, image, view, turn, colors, scale, placed, region);
    }
}

/// Whether [`draw_clipped`] of `clip` gives the very pixels [`draw`], not whole, gives there,
/// by construction: where both take the image as it is, as a copy or without antialiasing,
/// or both enlarge it through the bilinear filter with the same weights at the same precision
/// for every pixel they share. A reduce, and an image with alpha over the checkerboard, which
/// counts its squares from the corner drawn, are not taken as the same.
pub fn clip_draws_as_whole(
    (width, height): (u32, u32),
    image: &Image,
    view: &Viewport,
    turn: Turn,
    scale: f64,
    clip: (i64, i64, i64, i64),
) -> bool {
    let Some(placed) = placement((width, height), image, view, turn, scale) else {
        return false;
    };
    if !image.opaque && view.alpha_layer {
        return false;
    }
    let levels = valid_levels(image);
    if levels.is_empty() {
        return false;
    }
    let image = Image {
        levels: &levels,
        ..*image
    };
    let (iw, ih) = if turn.rotation % 180 == 90 {
        (image.height, image.width)
    } else {
        (image.width, image.height)
    };
    let axes = |region| axes_of((width, height), (iw, ih), &placed, region);
    let (x, y, dw, dh) = placed.region;
    let (left, top) = (x.max(clip.0), y.max(clip.1));
    let (right, bottom) = ((x + dw).min(clip.0 + clip.2), (y + dh).min(clip.1 + clip.3));
    if right <= left || bottom <= top {
        // nothing of the image lands in the clip, which a clipped draw leaves as it is
        return true;
    }
    let (Some(whole), Some(part)) = (
        axes(placed.region),
        axes((left, top, right - left, bottom - top)),
    ) else {
        return false;
    };
    scale::same_as_whole(
        &image,
        turn,
        (&whole.0, &whole.1),
        (&part.0, &part.1),
        view.antialias,
        true,
    )
}

/// Whether the whole draws of `image` with `before` and with `now`, a pan of whole pixels
/// apart, give the same pixels where both draw it: both as copies or without antialiasing,
/// or both enlarging with the same weights at the same precision for every sample they share.
/// The filter takes its precision from the largest weight anywhere along an axis, so a pan
/// can change how every sample rounds; a frame moved from the one `before` drew, its strips
/// drawn, is the one `now` draws only where this holds as [`clip_draws_as_whole`] does for
/// the strips.
pub fn wholes_agree(
    (width, height): (u32, u32),
    image: &Image,
    before: &Viewport,
    now: &Viewport,
    turn: Turn,
    scale: f64,
) -> bool {
    let levels = valid_levels(image);
    if levels.is_empty() {
        return false;
    }
    let image = Image {
        levels: &levels,
        ..*image
    };
    let (iw, ih) = if turn.rotation % 180 == 90 {
        (image.height, image.width)
    } else {
        (image.width, image.height)
    };
    let whole = |view| {
        let placed = placement((width, height), &image, view, turn, scale)?;
        axes_of((width, height), (iw, ih), &placed, placed.region)
    };
    let (Some(before), Some(now_axes)) = (whole(before), whole(now)) else {
        return false;
    };
    scale::same_as_whole(
        &image,
        turn,
        (&now_axes.0, &now_axes.1),
        (&before.0, &before.1),
        now.antialias,
        false,
    )
}

// The axes [`draw`] samples the `region` of a canvas `width`x`height` along, for an image
// `iw`x`ih` as turned and `placed`.
fn axes_of(
    (width, height): (u32, u32),
    (iw, ih): (u32, u32),
    placed: &Placement,
    (x, y, dw, dh): (i64, i64, i64, i64),
) -> Option<(Axis, Axis)> {
    let (origin, size) = (placed.origin, placed.scaled);
    Some((
        Axis::new(i64::from(width), i64::from(iw), (x, dw), (origin.0, size.0))?,
        Axis::new(
            i64::from(height),
            i64::from(ih),
            (y, dh),
            (origin.1, size.1),
        )?,
    ))
}

/// Where [`draw`], not whole, puts an image on a canvas: the rectangle it draws, and the
/// corner and size of the whole image scaled, which sampling follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub region: (i64, i64, i64, i64),
    pub origin: (i64, i64),
    pub scaled: (i64, i64),
}

impl Placement {
    /// Whether opaque image pixels cover all of a canvas of `size`.
    pub fn covers(&self, image: &Image, view: &Viewport, (width, height): (i64, i64)) -> bool {
        let (x, y, dw, dh) = self.region;
        (image.opaque || view.alpha_layer)
            && x <= 0
            && y <= 0
            && x + dw >= width
            && y + dh >= height
    }
}

/// Where [`draw`], not whole, puts `image` on a canvas of `size` at device `scale` as the
/// viewport places it, turned; None where none of it lands there. The rectangle drawn is the
/// source pixels that land in the canvas, in whole pixels, and where they land, rounded to
/// whole pixels; the whole image lies scaled at its corner, which sampling follows, so
/// a pan moves the frame by whole pixels and the zoom stays.
pub fn placement(
    (width, height): (u32, u32),
    image: &Image,
    view: &Viewport,
    turn: Turn,
    scale: f64,
) -> Option<Placement> {
    let (width, height) = (f64::from(width), f64::from(height));
    let (iw, ih) = if turn.rotation % 180 == 90 {
        (image.height, image.width)
    } else {
        (image.width, image.height)
    };
    let (iw, ih) = (f64::from(iw), f64::from(ih));
    let r = view.image_rect();
    let z = view.zoom() * scale;
    if !(z > 0.0 && z.is_finite()) {
        return None;
    }
    // the viewport counts y upward, the canvas downward
    let (dx, dy) = (r.x * scale, height - r.max_y() * scale);
    let place = |d: f64, extent: f64, size: f64| {
        let start = (-d / z).floor().max(0.0);
        let end = ((extent - d) / z).ceil().min(size);
        let at = (d + start * z).round() as i64;
        let length = ((end - start) * z).round().max(1.0) as i64;
        (end > start).then_some((at, length))
    };
    let (Some((x, dw)), Some((y, dh))) = (place(dx, width, iw), place(dy, height, ih)) else {
        return None;
    };
    let scaled = |size: f64| (size * z).round().max(1.0) as i64;
    Some(Placement {
        region: (x, y, dw, dh),
        origin: (dx.round() as i64, dy.round() as i64),
        scaled: (scaled(iw), scaled(ih)),
    })
}

/// Draws the image as `placed`, over the checkerboard first where it has alpha and the
/// viewport's alpha layer is on, in `region` of the rectangle `placed` draws.
#[allow(clippy::too_many_arguments)]
fn draw_placed(
    canvas: &mut Canvas,
    image: &Image,
    view: &Viewport,
    turn: Turn,
    colors: Option<&Colors>,
    scale: f64,
    placed: Placement,
    region: (i64, i64, i64, i64),
) {
    if !image.opaque && view.alpha_layer {
        let cell = ((12.0 * scale) as i64).max(1);
        let (x, y, dw, dh) = placed.region;
        checkerboard(canvas, (x, y), (dw, dh), cell, region);
    }
    let placement = (placed.origin, placed.scaled);
    blit(
        canvas,
        image,
        turn,
        region,
        placement,
        view.antialias,
        colors,
    );
}

/// Draws the image as turned into the canvas rectangle `region` (x, y, width, height), from
/// the whole image scaled to `size` with its corner at `origin`, as
/// `imlib_blend_image_onto_image` does: copied when opaque, blended when not.
pub(crate) fn blit(
    canvas: &mut Canvas,
    image: &Image,
    turn: Turn,
    region: (i64, i64, i64, i64),
    placement: ((i64, i64), (i64, i64)),
    antialias: bool,
    colors: Option<&Colors>,
) {
    let drawing = (antialias, colors, bands(canvas, region), true);
    blit_with(canvas, image, turn, region, placement, drawing);
}

/// The bands of rows to draw `region` of the canvas in: one a core, eight at most, where the
/// pixels drawn, those of the region on the canvas, are a few hundred thousand or more, and
/// one below that, where a thread costs more than it saves.
fn bands(canvas: &Canvas, (x, y, w, h): (i64, i64, i64, i64)) -> usize {
    let on = |start: i64, length: i64, extent: u32| {
        (start.saturating_add(length).min(i64::from(extent)) - start.max(0)).max(0)
    };
    let drawn = on(x, w, canvas.width()).saturating_mul(on(y, h, canvas.height()));
    if drawn >= 1 << 18 {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
    } else {
        1
    }
}

/// [`blit`] in `bands` bands of rows, each on a thread of its own where `spawn` asks for one.
fn blit_with(
    canvas: &mut Canvas,
    image: &Image,
    turn: Turn,
    (x, y, dw, dh): (i64, i64, i64, i64),
    (origin, size): ((i64, i64), (i64, i64)),
    (antialias, colors, bands, spawn): (bool, Option<&Colors>, usize, bool),
) {
    let levels = valid_levels(image);
    if levels.is_empty() {
        return;
    }
    let image = Image {
        levels: &levels,
        ..*image
    };
    let (iw, ih) = if turn.rotation % 180 == 90 {
        (image.height, image.width)
    } else {
        (image.width, image.height)
    };
    let (width, height) = (i64::from(canvas.width()), i64::from(canvas.height()));
    let (Some(across), Some(down)) = (
        Axis::new(width, i64::from(iw), (x, dw), (origin.0, size.0)),
        Axis::new(height, i64::from(ih), (y, dh), (origin.1, size.1)),
    ) else {
        return;
    };
    match scale::sample(&image, turn, &across, &down, antialias) {
        Some(Source::Sampled(sampled)) => {
            let rows = (down.first as usize, sampled.rows.len());
            let columns = (across.first as usize, across.count);
            compose_sampled(canvas, &sampled, columns, rows, colors, bands, spawn);
        }
        Some(Source::Reduced(reduced)) => {
            let rows = (down.first as usize, down.count);
            let left = across.first as usize;
            compose_reduced(canvas, &reduced, left, rows, colors, bands, spawn);
        }
        None => {}
    }
}

/// Composes drawn pixels over `line`: each from the sample `columns` names, through `pixel`,
/// copied when opaque and blended when not.
fn compose(
    line: &mut [u32],
    columns: &[usize],
    colors: Option<&Colors>,
    pixel: impl Fn(usize) -> [u8; 4],
) {
    for (out, &column) in line.iter_mut().zip(columns) {
        let mut rgba = pixel(column);
        if let Some(colors) = colors {
            colors.apply(&mut rgba);
        }
        *out = match rgba[3] {
            255 => canvas::rgb(rgba[0], rgba[1], rgba[2]),
            0 => *out,
            alpha => canvas::over(premultiplied(rgba, alpha), *out),
        };
    }
}

/// Composes the samples at hand into the canvas rows `first..first + count`, the columns
/// `left..left + width` of each, in `bands` bands of rows, each on a thread of its own where
/// `spawn` asks for one and the system gives it, else on this one.
fn compose_sampled(
    canvas: &mut Canvas,
    sampled: &Sampled,
    (left, width): (usize, usize),
    (first, count): (usize, usize),
    colors: Option<&Colors>,
    bands: usize,
    spawn: bool,
) {
    let stride = canvas.width() as usize;
    let per_band = count.div_ceil(bands.max(1)).max(1);
    let pixels = &mut canvas.pixels_mut()[first * stride..(first + count) * stride];
    let compose_rows = |top: usize, rows: &mut [u32]| {
        let shown = &sampled.rows[top..top + rows.len() / stride];
        for (line, &source_row) in rows.chunks_exact_mut(stride).zip(shown) {
            let line = &mut line[left..left + width];
            compose(line, &sampled.columns, colors, |source_column| {
                let at = if sampled.swapped {
                    source_column * sampled.width + source_row
                } else {
                    source_row * sampled.width + source_column
                };
                sampled.rgba[at * 4..at * 4 + 4].try_into().unwrap()
            });
        }
    };
    // each band's rows, from the first drawn, and its canvas rows, taken by the thread drawing it
    let bands: Vec<Mutex<Option<Band>>> = pixels
        .chunks_mut(per_band * stride)
        .enumerate()
        .map(|(i, rows)| Mutex::new(Some((i * per_band, rows))))
        .collect();
    let draw = |band: &Mutex<Option<Band>>| {
        if let Some((top, rows)) = band.lock().unwrap().take() {
            compose_rows(top, rows);
        }
    };
    std::thread::scope(|scope| {
        let Some((last, others)) = bands.split_last() else {
            return;
        };
        for band in others {
            let spawned = spawn
                && std::thread::Builder::new()
                    .spawn_scoped(scope, move || draw(band))
                    .is_ok();
            // a thread the system refused leaves its band to this one
            if !spawned {
                draw(band);
            }
        }
        draw(last);
    });
}

/// How many sample columns of a quarter-turned crop a band reduces at once: a strip of them
/// for every sample row, which is all a band keeps of the crop.
const STRIP: usize = 64;

/// Draws a crop reduced as it composes into the canvas rows `first..first + count`, from
/// column `left`, in `bands` bands of rows, each with its own canvas rows and no more of the
/// crop than a row of samples, or a strip of them where a quarter turn runs its rows down
/// the canvas. A band goes on a thread of its own where `spawn` asks for one and the system
/// gives it, else on this one.
fn compose_reduced(
    canvas: &mut Canvas,
    reduced: &Reduced,
    left: usize,
    (first, count): (usize, usize),
    colors: Option<&Colors>,
    bands: usize,
    spawn: bool,
) {
    let stride = canvas.width() as usize;
    let per_band = count.div_ceil(bands.max(1)).max(1);
    let pixels = &mut canvas.pixels_mut()[first * stride..(first + count) * stride];
    // each band's rows, from the first drawn, and its canvas rows, taken by the thread drawing it
    let bands: Vec<Mutex<Option<Band>>> = pixels
        .chunks_mut(per_band * stride)
        .enumerate()
        .map(|(i, rows)| Mutex::new(Some((i * per_band, rows))))
        .collect();
    let draw = |band: &Mutex<Option<Band>>| {
        if let Some((top, rows)) = band.lock().unwrap().take() {
            compose_band(rows, top, stride, left, reduced, colors);
        }
    };
    std::thread::scope(|scope| {
        let (last, others) = bands.split_last().expect("one band at least");
        for band in others {
            // shortcut: a thread for each band of each frame, some tens of microseconds each,
            // little beside the reduce of a large image; a pool kept with the viewer would
            // replace them once many such frames come a second, as for a large animation
            let spawned = spawn
                && std::thread::Builder::new()
                    .spawn_scoped(scope, move || draw(band))
                    .is_ok();
            // a thread the system refused leaves its band to this one
            if !spawned {
                draw(band);
            }
        }
        draw(last);
    });
}

/// A band of canvas rows: the first one's place among the rows drawn, and the rows.
type Band<'a> = (usize, &'a mut [u32]);

/// Draws the band of canvas `rows`, `stride` pixels a row, which are the drawn rows from
/// `top` on, reducing only the samples they show.
fn compose_band(
    rows: &mut [u32],
    top: usize,
    stride: usize,
    left: usize,
    reduced: &Reduced,
    colors: Option<&Colors>,
) {
    let width = reduced.columns.len();
    let shown = &reduced.rows[top..top + rows.len() / stride];
    let lines = rows
        .chunks_exact_mut(stride)
        .map(|row| &mut row[left..left + width]);
    let (columns, sample_rows) = reduced.size();
    if !reduced.swapped {
        // each drawn row shows a sample row, the same one again only at an edge
        let mut reducer = Reducer::new(reduced, 0..columns);
        let mut samples = vec![0; columns * 4];
        let mut reduced_row = usize::MAX;
        for (line, &row) in lines.zip(shown) {
            if row != reduced_row {
                reducer.row(row, &mut samples);
                reduced_row = row;
            }
            compose(line, &reduced.columns, colors, |column| {
                samples[column * 4..column * 4 + 4].try_into().unwrap()
            });
        }
        return;
    }
    // a quarter turn: each drawn row shows a sample column, so the band reduces a strip of
    // columns at a time for every sample row
    let mut lines: Vec<&mut [u32]> = lines.collect();
    let (low, high) = (
        shown.iter().copied().min().unwrap_or(0),
        shown.iter().copied().max().unwrap_or(0),
    );
    // no wider than the columns the band shows, which are few for a short turned image
    let mut strip = vec![0; sample_rows * STRIP.min(high - low + 1) * 4];
    for start in (low..=high).step_by(STRIP) {
        let part = start..(start + STRIP).min(high + 1);
        let wide = part.len();
        let mut reducer = Reducer::new(reduced, part.clone());
        for (row, out) in strip
            .chunks_exact_mut(wide * 4)
            .take(sample_rows)
            .enumerate()
        {
            reducer.row(row, out);
        }
        for (line, &column) in lines.iter_mut().zip(shown) {
            if part.contains(&column) {
                let offset = column - part.start;
                compose(line, &reduced.columns, colors, |row| {
                    let at = (row * wide + offset) * 4;
                    strip[at..at + 4].try_into().unwrap()
                });
            }
        }
    }
}

/// The levels whose sizes follow the pyramid contract, from level 0 on.
fn valid_levels<'a>(image: &Image<'a>) -> Vec<&'a [u8]> {
    let (mut w, mut h) = (u64::from(image.width), u64::from(image.height));
    let mut levels = Vec::new();
    for level in image.levels {
        // a size whose bytes overflow ends the pyramid too
        if w.checked_mul(h).and_then(|n| n.checked_mul(4)) != Some(level.len() as u64) {
            break;
        }
        levels.push(*level);
        (w, h) = (w.div_ceil(2), h.div_ceil(2));
    }
    levels
}

/// A straight-alpha pixel as a canvas pixel.
fn premultiplied([r, g, b, _]: [u8; 4], alpha: u8) -> u32 {
    // the channel times alpha over 255, rounded, as `canvas::over` rounds
    let times = |c: u8| {
        let t = u32::from(c) * u32::from(alpha) + 128;
        (t + (t >> 8)) >> 8
    };
    u32::from(alpha) << 24 | times(r) << 16 | times(g) << 8 | times(b)
}

/// The white and gray squares under transparent pixels, `cell` pixels wide and counted from
/// `at`, the corner of the rectangle drawn, over that rectangle as far as it lies on the
/// canvas.
fn checkerboard(
    canvas: &mut Canvas,
    at: (i64, i64),
    size: (i64, i64),
    cell: i64,
    (cx, cy, cw, ch): (i64, i64, i64, i64),
) {
    let (width, height) = (i64::from(canvas.width()), i64::from(canvas.height()));
    let (columns, rows) = (
        at.0.max(cx).max(0)..(at.0 + size.0).min(cx + cw).min(width),
        at.1.max(cy).max(0)..(at.1 + size.1).min(cy + ch).min(height),
    );
    let pixels = canvas.pixels_mut();
    for row in rows {
        for column in columns.clone() {
            let white = ((column - at.0) / cell + (row - at.1) / cell) % 2 == 0;
            pixels[(row * width + column) as usize] = if white {
                canvas::rgb(255, 255, 255)
            } else {
                canvas::rgb(0xbf, 0xbf, 0xbf)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsxiv_ng_core::Size;

    // what blit drew before reducing band by band: the whole crop reduced into one buffer,
    // then composed
    fn blit_whole(
        canvas: &mut Canvas,
        image: &Image,
        turn: Turn,
        (x, y, dw, dh): (i64, i64, i64, i64),
        (origin, size): ((i64, i64), (i64, i64)),
        colors: Option<&Colors>,
    ) {
        let levels = valid_levels(image);
        let image = Image {
            levels: &levels,
            ..*image
        };
        let (iw, ih) = if turn.rotation % 180 == 90 {
            (image.height, image.width)
        } else {
            (image.width, image.height)
        };
        let (width, height) = (i64::from(canvas.width()), i64::from(canvas.height()));
        let across = Axis::new(width, i64::from(iw), (x, dw), (origin.0, size.0)).unwrap();
        let down = Axis::new(height, i64::from(ih), (y, dh), (origin.1, size.1)).unwrap();
        let sampled = scale::sample_whole(&image, turn, &across, &down, true).unwrap();
        let stride = width as usize;
        let pixels = canvas.pixels_mut();
        // the loop blit composed with before the bands, as it was
        for (row, &source_row) in sampled.rows.iter().enumerate() {
            let line = (down.first as usize + row) * stride + across.first as usize;
            let line = &mut pixels[line..line + across.count];
            for (out, &source_column) in line.iter_mut().zip(&sampled.columns) {
                let at = if sampled.swapped {
                    source_column * sampled.width + source_row
                } else {
                    source_row * sampled.width + source_column
                };
                let mut rgba: [u8; 4] = sampled.rgba[at * 4..at * 4 + 4].try_into().unwrap();
                if let Some(colors) = colors {
                    colors.apply(&mut rgba);
                }
                *out = match rgba[3] {
                    255 => canvas::rgb(rgba[0], rgba[1], rgba[2]),
                    0 => *out,
                    alpha => canvas::over(premultiplied(rgba, alpha), *out),
                };
            }
        }
    }

    /// The eight ways an image can show turned and flipped.
    const EIGHT_TURNS: [(u32, bool, bool); 8] = [
        (0, false, false),
        (0, true, false),
        (0, false, true),
        (180, false, false),
        (90, false, false),
        (90, true, false),
        (90, false, true),
        (270, false, false),
    ];

    #[test]
    fn a_quarter_turn_composes_strip_by_strip_the_pixels_of_the_whole_crop() {
        // 300 columns reduced to 150, which run down a canvas 160 rows high: three strips,
        // the last short, in decreasing order where the turn and flip reverse them
        let (w, h) = (300u32, 200u32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [
                    (x * 3 + y) as u8,
                    (y * 7) as u8,
                    (x ^ y) as u8,
                    (40 + x + y * 3) as u8,
                ]
            })
            .collect();
        let image = Image {
            width: w,
            height: h,
            levels: &[&rgba],
            opaque: false,
        };
        let (cw, ch) = (120i64, 160i64);
        let under: Vec<u32> = (0..cw * ch)
            .map(|i| match (i % cw / 3 + i / cw / 3) % 2 {
                0 => canvas::rgb(255, 255, 255),
                _ => canvas::rgb(0xbf, 0xbf, 0xbf),
            })
            .collect();
        let table = Colors::new(0.7, 1.4);
        // the turned image is 200 x 300, shown at half size from a corner off the canvas
        let placement = ((-3, 2), (100, 150));
        let region = (0, 2, 97, 150);
        for (rotation, flip_h, flip_v) in
            [(90, false, false), (90, false, true), (270, false, false)]
        {
            let turn = Turn {
                rotation,
                flip_h,
                flip_v,
            };
            for colors in [None, table.as_ref()] {
                let mut whole = under.clone();
                let mut canvas = Canvas::new(&mut whole, cw as u32, ch as u32).unwrap();
                blit_whole(&mut canvas, &image, turn, region, placement, colors);
                for bands in [1, 2, 500] {
                    let mut banded = under.clone();
                    let mut canvas = Canvas::new(&mut banded, cw as u32, ch as u32).unwrap();
                    let drawing = (true, colors, bands, true);
                    blit_with(&mut canvas, &image, turn, region, placement, drawing);
                    assert!(
                        banded == whole,
                        "{bands} bands, {turn:?}, {}",
                        colors.is_some()
                    );
                }
            }
        }
    }

    #[test]
    fn bands_compose_the_pixels_of_the_whole_crop() {
        // detail with alpha, an odd size, and levels of the sizes the contract gives
        let (w, h) = (97u32, 61u32);
        let level = |w: u32, h: u32, seed: u32| -> Vec<u8> {
            (0..w * h)
                .flat_map(|i| {
                    let (x, y) = (i % w, i / w);
                    let alpha = (60 + x * 3 + y * 5 + seed) as u8;
                    [
                        (x * 7 + y + seed) as u8,
                        (y * 5) as u8,
                        (x * y) as u8,
                        alpha,
                    ]
                })
                .collect()
        };
        let mut levels = vec![level(w, h, 0)];
        let (mut lw, mut lh) = (w, h);
        while lw > 1 || lh > 1 {
            (lw, lh) = (lw.div_ceil(2), lh.div_ceil(2));
            levels.push(level(lw, lh, levels.len() as u32));
        }
        let alpha: Vec<&[u8]> = levels.iter().map(|l| &l[..]).collect();
        let opaque_levels: Vec<Vec<u8>> = levels
            .iter()
            .map(|l| l.chunks(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect())
            .collect();
        let opaque: Vec<&[u8]> = opaque_levels.iter().map(|l| &l[..]).collect();
        let table = Colors::new(1.6, 1.3);
        let (cw, ch) = (50i64, 34i64);
        // the checkerboard's two colors in single pixels, under the image's alpha
        let under: Vec<u32> = (0..cw * ch)
            .map(|i| match (i % cw + i / cw) % 2 {
                0 => canvas::rgb(255, 255, 255),
                _ => canvas::rgb(0xbf, 0xbf, 0xbf),
            })
            .collect();
        for (levels, opaque) in [(&alpha, false), (&opaque, true)] {
            let image = Image {
                width: w,
                height: h,
                levels,
                opaque,
            };
            // reduced from level 0 and from smaller levels, with the crop starting and ending
            // inside source pixels, and hanging off the canvas
            for placement in [
                ((3, 2), (44, 28)),
                ((-13, -7), (70, 45)),
                ((5, 4), (21, 13)),
                ((-40, -30), (90, 58)),
            ] {
                for (rotation, flip_h, flip_v) in EIGHT_TURNS {
                    let turn = Turn {
                        rotation,
                        flip_h,
                        flip_v,
                    };
                    // a turn swaps the placement's sides, which stays within the canvas
                    let ((ox, oy), (sw, sh)) = placement;
                    let ((ox, oy), (sw, sh)) = if rotation % 180 == 90 {
                        ((oy, ox), (sh * 4 / 5, sw * 4 / 5))
                    } else {
                        ((ox, oy), (sw, sh))
                    };
                    let (x, y) = (ox.max(0), oy.max(0));
                    let region = (x, y, (ox + sw).min(cw) - x, (oy + sh).min(ch) - y);
                    let placement = ((ox, oy), (sw, sh));
                    for colors in [None, table.as_ref()] {
                        let mut whole = under.clone();
                        let mut canvas = Canvas::new(&mut whole, cw as u32, ch as u32).unwrap();
                        blit_whole(&mut canvas, &image, turn, region, placement, colors);
                        // more bands than rows too, a row each
                        for bands in [1, 2, 3, 7, 200] {
                            for spawn in [true, false] {
                                let mut banded = under.clone();
                                let mut canvas =
                                    Canvas::new(&mut banded, cw as u32, ch as u32).unwrap();
                                let drawing = (true, colors, bands, spawn);
                                blit_with(&mut canvas, &image, turn, region, placement, drawing);
                                assert!(
                                    banded == whole,
                                    "{bands} bands, threads {spawn}, opaque {opaque}, \
                                     {placement:?}, {turn:?}, colors {}",
                                    colors.is_some()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_bands_count_the_pixels_drawn_on_the_canvas() {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
        let mut small = vec![0; 100 * 100];
        let small = Canvas::new(&mut small, 100, 100).unwrap();
        // a placement many times the canvas, which draws its ten thousand pixels alone
        assert_eq!(bands(&small, (-10_000, -10_000, 30_000, 30_000)), 1);
        let mut large = vec![0; 1000 * 600];
        let large = Canvas::new(&mut large, 1000, 600).unwrap();
        assert_eq!(bands(&large, (0, 0, 1000, 600)), cores);
        assert_eq!(
            bands(&large, (-500, 0, 1000, 600)),
            cores,
            "300 thousand on it"
        );
        assert_eq!(bands(&large, (-800, 0, 1000, 600)), 1, "120 thousand on it");
    }

    #[test]
    fn bands_compose_the_samples_at_hand_as_one_band_does() {
        // detail with alpha, enlarged, kept at its size, and taken without antialiasing
        let (w, h) = (37u32, 23u32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [
                    (x * 7 + y) as u8,
                    (y * 11) as u8,
                    (x * y) as u8,
                    (60 + x * 3 + y * 5) as u8,
                ]
            })
            .collect();
        let image = Image {
            width: w,
            height: h,
            levels: &[&rgba],
            opaque: false,
        };
        let table = Colors::new(1.6, 1.3);
        let (cw, ch) = (90i64, 70i64);
        let under: Vec<u32> = (0..cw * ch)
            .map(|i| match (i % cw + i / cw) % 2 {
                0 => canvas::rgb(255, 255, 255),
                _ => canvas::rgb(0xbf, 0xbf, 0xbf),
            })
            .collect();
        // enlarged from a corner off the canvas, kept at its size, enlarged past the canvas
        for placement in [
            ((-13, -7), (100, 62)),
            ((5, 4), (37, 23)),
            ((-40, -30), (250, 160)),
        ] {
            for (rotation, flip_h, flip_v) in EIGHT_TURNS {
                let turn = Turn {
                    rotation,
                    flip_h,
                    flip_v,
                };
                let ((ox, oy), (sw, sh)) = placement;
                let (sw, sh) = if rotation % 180 == 90 {
                    (sh, sw)
                } else {
                    (sw, sh)
                };
                let (x, y) = (ox.max(0), oy.max(0));
                let region = (x, y, (ox + sw).min(cw) - x, (oy + sh).min(ch) - y);
                let placement = ((ox, oy), (sw, sh));
                for (antialias, colors) in [(true, None), (true, table.as_ref()), (false, None)] {
                    let mut one = under.clone();
                    let mut canvas = Canvas::new(&mut one, cw as u32, ch as u32).unwrap();
                    let drawing = (antialias, colors, 1, false);
                    blit_with(&mut canvas, &image, turn, region, placement, drawing);
                    // more bands than rows too, a row each
                    for bands in [2, 3, 7, 200] {
                        for spawn in [true, false] {
                            let mut banded = under.clone();
                            let mut canvas =
                                Canvas::new(&mut banded, cw as u32, ch as u32).unwrap();
                            let drawing = (antialias, colors, bands, spawn);
                            blit_with(&mut canvas, &image, turn, region, placement, drawing);
                            assert!(
                                banded == one,
                                "{bands} bands, threads {spawn}, {placement:?}, {turn:?}, \
                                 antialias {antialias}, colors {}",
                                colors.is_some()
                            );
                        }
                    }
                }
            }
        }
    }

    // a viewport showing an image of `image` pixels in a view of `view`, as `w` leaves it
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
    fn whole_draws_a_pan_apart_that_round_otherwise_are_not_said_to_agree() {
        // a black column and one of red 5, enlarged 5.5 times into a 2x2 canvas from its
        // corner, then panned a pixel left: the first frame's first sample, on a pixel, sets
        // a coarser precision than the second frame's, so the column both draw rounds
        // otherwise, though the column that came into view is drawn as the whole
        let rgba = [[0, 0, 0, 255], [5, 0, 0, 255]].repeat(2).concat();
        let image = Image {
            width: 2,
            height: 2,
            levels: &[&rgba],
            opaque: true,
        };
        let mut before = viewport((2.0, 2.0), (2.0, 2.0));
        before.set_zoom(5.5, None);
        before.scroll(100.0, -100.0);
        let mut now = before.clone();
        now.scroll(-1.0, 0.0);
        let turn = Turn::default();
        let origin = |view| placement((2, 2), &image, view, turn, 1.0).unwrap().origin;
        assert_eq!((origin(&before), origin(&now)), ((0, 0), (-1, 0)));
        let whole = |view| {
            let mut pixels = vec![0; 4];
            let mut canvas = Canvas::new(&mut pixels, 2, 2).unwrap();
            draw(&mut canvas, &image, view, turn, None, 1.0, false);
            pixels
        };
        let (last, want) = (whole(&before), whole(&now));
        let entered = (1, 0, 1, 2);
        let mut moved = vec![last[1], 0, last[3], 0];
        let mut canvas = Canvas::new(&mut moved, 2, 2).unwrap();
        draw_clipped(&mut canvas, &image, &now, turn, None, 1.0, entered);
        assert!(clip_draws_as_whole(
            (2, 2),
            &image,
            &now,
            turn,
            1.0,
            entered
        ));
        assert_ne!(moved, want, "the frame moved is not the one drawn whole");
        assert!(!wholes_agree((2, 2), &image, &before, &now, turn, 1.0));
        // a pan whose two whole draws keep one precision agrees
        let mut further = now.clone();
        further.scroll(-1.0, 0.0);
        assert!(wholes_agree((2, 2), &image, &now, &further, turn, 1.0));
    }

    #[test]
    fn a_clipped_draw_said_to_draw_as_the_whole_gives_its_pixels_in_its_clip_alone() {
        // detail with alpha, over the checkerboard and without it, enlarged by factors whole
        // and uneven, kept at its size, and without antialiasing
        let (w, h) = (97u32, 61u32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let alpha = if (x + y) % 5 == 0 {
                    255
                } else {
                    (90 + x + y) as u8
                };
                [(x * 7 + y) as u8, (y * 11) as u8, (x * y) as u8, alpha]
            })
            .collect();
        let sentinel = canvas::rgb(1, 2, 3);
        let (cw, ch) = (120u32, 90u32);
        let clips = [
            (0, 0, 120, 7),
            (0, 83, 120, 7),
            (0, 0, 9, 90),
            (113, 0, 7, 90),
            (40, 30, 21, 17),
        ];
        let table = Colors::new(1.3, 1.1);
        let mut drawn_same = 0;
        for opaque in [false, true] {
            let image = Image {
                width: w,
                height: h,
                levels: &[&rgba],
                opaque,
            };
            for (zoom, antialias, alpha_layer) in [
                (1.0, true, false),
                (1.5, true, true),
                (2.0, true, false),
                (2.37, true, true),
                (3.0, false, false),
            ] {
                let mut view = viewport((f64::from(cw), f64::from(ch)), (97.0, 61.0));
                view.set_zoom(zoom, None);
                view.scroll(13.0, -7.0);
                view.antialias = antialias;
                view.alpha_layer = alpha_layer;
                for (rotation, flip_h, flip_v) in [(0, false, false), (90, true, false)] {
                    let turn = Turn {
                        rotation,
                        flip_h,
                        flip_v,
                    };
                    let mut whole = vec![sentinel; (cw * ch) as usize];
                    let mut canvas = Canvas::new(&mut whole, cw, ch).unwrap();
                    draw(&mut canvas, &image, &view, turn, table.as_ref(), 1.0, false);
                    for clip in clips {
                        let exact = clip_draws_as_whole((cw, ch), &image, &view, turn, 1.0, clip);
                        let certain = !antialias || zoom == 1.0;
                        assert!(exact || !certain, "a copy, or nearest, is always the same");
                        if !exact {
                            continue;
                        }
                        drawn_same += 1;
                        let mut clipped = vec![sentinel; (cw * ch) as usize];
                        let mut canvas = Canvas::new(&mut clipped, cw, ch).unwrap();
                        draw_clipped(&mut canvas, &image, &view, turn, table.as_ref(), 1.0, clip);
                        for (i, (&got, &want)) in clipped.iter().zip(&whole).enumerate() {
                            let (x, y) = ((i % cw as usize) as i64, (i / cw as usize) as i64);
                            let inside = x >= clip.0
                                && x < clip.0 + clip.2
                                && y >= clip.1
                                && y < clip.1 + clip.3;
                            let expected = if inside { want } else { sentinel };
                            assert_eq!(
                                got, expected,
                                "({x}, {y}), clip {clip:?}, zoom {zoom}, antialias \
                                 {antialias}, alpha layer {alpha_layer}, {turn:?}, opaque {opaque}"
                            );
                        }
                    }
                }
            }
        }
        assert!(drawn_same > 40, "{drawn_same} clips drawn as the whole");
    }

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        rgba.repeat((w * h) as usize)
    }

    fn draw_into(
        pixels: &mut [u32],
        size: (u32, u32),
        image: &Image,
        view: &Viewport,
        turn: Turn,
    ) -> bool {
        let mut canvas = Canvas::new(pixels, size.0, size.1).unwrap();
        draw(&mut canvas, image, view, turn, None, 1.0, false)
    }

    #[test]
    fn an_image_drawn_whole_keeps_its_first_and_last_rows() {
        // 1600x236 at 50% in a view of 800x464 is 1000x147.5
        // terminal pixels at 1.25, a frame of 1000x148, whose first row is the red top and last
        // the blue bottom
        let (w, h) = (1600, 236);
        let rgba: Vec<u8> = (0..h)
            .flat_map(|row| {
                let color = match row {
                    0 => [255, 0, 0, 255],
                    235 => [0, 0, 255, 255],
                    _ => [0, 255, 0, 255],
                };
                color.repeat(w as usize)
            })
            .collect();
        let image = Image {
            width: w,
            height: h,
            levels: &[&rgba],
            opaque: true,
        };
        let view = viewport((800.0, 464.0), (1600.0, 236.0));
        assert_eq!(view.zoom(), 0.5);
        let mut pixels = vec![0; 1000 * 148];
        let mut canvas = Canvas::new(&mut pixels, 1000, 148).unwrap();
        let turn = Turn {
            rotation: 0,
            flip_h: false,
            flip_v: false,
        };
        assert!(draw(&mut canvas, &image, &view, turn, None, 1.25, true));
        let (top, bottom) = (pixels[500], pixels[147 * 1000 + 500]);
        assert!(
            top >> 24 == 0xff
                && (top >> 16) & 0xff > 0
                && bottom >> 24 == 0xff
                && bottom & 0xff > 0,
            "{top:x} {bottom:x}"
        );
    }

    #[test]
    fn turns_come_before_flips() {
        // 3 × 2: red green blue over white black gray
        let rgba: Vec<u8> = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
            [0, 0, 0, 255],
            [128, 128, 128, 255],
        ]
        .concat();
        let image = Image {
            width: 3,
            height: 2,
            levels: &[&rgba],
            opaque: true,
        };
        let (red, green, blue) = (
            canvas::rgb(255, 0, 0),
            canvas::rgb(0, 255, 0),
            canvas::rgb(0, 0, 255),
        );
        let (white, black, gray) = (
            canvas::rgb(255, 255, 255),
            canvas::rgb(0, 0, 0),
            canvas::rgb(128, 128, 128),
        );
        let turned = viewport((2.0, 3.0), (2.0, 3.0));
        let mut pixels = [0; 6];
        let quarter = Turn {
            rotation: 90,
            ..Turn::default()
        };
        assert!(draw_into(&mut pixels, (2, 3), &image, &turned, quarter));
        assert_eq!(
            pixels,
            [white, red, black, green, gray, blue],
            "a quarter turn clockwise"
        );
        let flipped = Turn {
            flip_h: true,
            ..quarter
        };
        draw_into(&mut pixels, (2, 3), &image, &turned, flipped);
        assert_eq!(
            pixels,
            [red, white, green, black, blue, gray],
            "then a flip of what shows"
        );
        let back = Turn {
            rotation: 270,
            flip_v: true,
            ..Turn::default()
        };
        draw_into(&mut pixels, (2, 3), &image, &turned, back);
        assert_eq!(
            pixels,
            [red, white, green, black, blue, gray],
            "the same, turned the other way"
        );
        let half = Turn {
            rotation: 180,
            flip_h: true,
            flip_v: true,
        };
        let mut pixels = [0; 6];
        draw_into(
            &mut pixels,
            (3, 2),
            &image,
            &viewport((3.0, 2.0), (3.0, 2.0)),
            half,
        );
        assert_eq!(
            pixels,
            [red, green, blue, white, black, gray],
            "a half turn and two flips undo each other"
        );
    }

    #[test]
    fn coverage_needs_opaque_pixels_over_the_whole_canvas() {
        let opaque = solid(4, 4, [9, 9, 9, 255]);
        let mut image = Image {
            width: 4,
            height: 4,
            levels: &[&opaque],
            opaque: true,
        };
        let mut pixels = [0; 16];
        assert!(draw_into(
            &mut pixels,
            (4, 4),
            &image,
            &viewport((4.0, 4.0), (4.0, 4.0)),
            Turn::default()
        ));
        let mut pixels = [0; 30];
        assert!(!draw_into(
            &mut pixels,
            (5, 6),
            &image,
            &viewport((5.0, 6.0), (4.0, 4.0)),
            Turn::default()
        ));
        // an image with alpha covers only on the checkerboard
        image.opaque = false;
        let mut view = viewport((4.0, 4.0), (4.0, 4.0));
        let mut pixels = [0; 16];
        assert!(!draw_into(
            &mut pixels,
            (4, 4),
            &image,
            &view,
            Turn::default()
        ));
        view.alpha_layer = true;
        assert!(draw_into(
            &mut pixels,
            (4, 4),
            &image,
            &view,
            Turn::default()
        ));
    }

    #[test]
    fn whole_draws_all_of_the_image_over_the_canvas() {
        // 2 × 1, black then white, onto 4 × 2 without antialiasing
        let rgba = [[0, 0, 0, 255], [255, 255, 255, 255]].concat();
        let image = Image {
            width: 2,
            height: 1,
            levels: &[&rgba],
            opaque: true,
        };
        let mut view = viewport((100.0, 100.0), (2.0, 1.0));
        view.antialias = false;
        let mut pixels = [0; 8];
        let mut canvas = Canvas::new(&mut pixels, 4, 2).unwrap();
        assert!(draw(
            &mut canvas,
            &image,
            &view,
            Turn::default(),
            None,
            1.0,
            true
        ));
        let (black, white) = (canvas::rgb(0, 0, 0), canvas::rgb(255, 255, 255));
        assert_eq!(
            pixels,
            [black, black, white, white, black, black, white, white]
        );
    }

    #[test]
    fn the_checkerboard_counts_its_cells_from_the_drawn_rectangle_s_corner() {
        let clear = solid(30, 30, [0, 0, 0, 0]);
        let image = Image {
            width: 30,
            height: 30,
            levels: &[&clear],
            opaque: false,
        };
        // centered 5 pixels in from each edge
        let mut view = viewport((40.0, 40.0), (30.0, 30.0));
        view.alpha_layer = true;
        let mut pixels = vec![0; 40 * 40];
        let mut canvas = Canvas::new(&mut pixels, 40, 40).unwrap();
        draw(
            &mut canvas,
            &image,
            &view,
            Turn::default(),
            None,
            1.0,
            false,
        );
        let (white, gray) = (canvas::rgb(255, 255, 255), canvas::rgb(0xbf, 0xbf, 0xbf));
        let at = |x: usize, y: usize| pixels[y * 40 + x];
        assert_eq!(at(4, 5), 0, "outside the image");
        assert_eq!(
            [at(5, 5), at(16, 5), at(17, 5), at(5, 17), at(17, 17)],
            [white, white, gray, gray, white]
        );
        // panned 10 pixels off the left edge, the squares still start at the canvas's
        let mut view = viewport((20.0, 30.0), (30.0, 30.0));
        view.set_zoom(1.0, None);
        view.scroll_to_edge(nsxiv_ng_core::Direction::Right);
        view.alpha_layer = true;
        let mut pixels = vec![0; 20 * 30];
        let mut canvas = Canvas::new(&mut pixels, 20, 30).unwrap();
        draw(
            &mut canvas,
            &image,
            &view,
            Turn::default(),
            None,
            1.0,
            false,
        );
        assert_eq!([pixels[0], pixels[11], pixels[12]], [white, white, gray]);
    }

    // the pixels `blit` draws of a canvas `width` × 1 from the image, scaled to `scaled` and
    // placed at `origin` along x and kept along y
    fn blitted(image: &Image, turn: Turn, width: u32, (origin, scaled): (i64, i64)) -> Vec<u32> {
        let mut pixels = vec![0; width as usize];
        let mut canvas = Canvas::new(&mut pixels, width, 1).unwrap();
        let region = (0, 0, i64::from(width), 1);
        blit(
            &mut canvas,
            image,
            turn,
            region,
            ((origin, 0), (scaled, 1)),
            true,
            None,
        );
        pixels
    }

    #[test]
    fn an_axis_that_keeps_its_size_leaves_the_other_s_filter_alone() {
        // black, black, white, white halved across, its height of 1 kept: an average, not
        // bilinear
        let strip: Vec<u8> = [0, 0, 255, 255]
            .iter()
            .flat_map(|&c| [c, c, c, 255])
            .collect();
        let (black, white) = (canvas::rgb(0, 0, 0), canvas::rgb(255, 255, 255));
        let across = Image {
            width: 4,
            height: 1,
            levels: &[&strip],
            opaque: true,
        };
        assert_eq!(blitted(&across, Turn::default(), 2, (0, 2)), [black, white]);
        // the same strip standing, laid down by a quarter turn either way
        let standing = Image {
            width: 1,
            height: 4,
            ..across
        };
        for (rotation, expected) in [(270, [black, white]), (90, [white, black])] {
            let turn = Turn {
                rotation,
                ..Turn::default()
            };
            assert_eq!(blitted(&standing, turn, 2, (0, 2)), expected, "{rotation}");
        }
    }

    #[test]
    fn an_enlargement_past_the_16_16_step_shows_the_first_pixel() {
        // 2 pixels to 131,072: Imlib2's step rounds to 0, so every pixel shows the first
        let pair = [[0, 0, 0, 255], [255, 255, 255, 255]].concat();
        let image = Image {
            width: 2,
            height: 1,
            levels: &[&pair],
            opaque: true,
        };
        let placement = (-100_000, 131_072);
        let (black, white) = (canvas::rgb(0, 0, 0), canvas::rgb(255, 255, 255));
        assert_eq!(blitted(&image, Turn::default(), 4, placement), [black; 4]);
        let flipped = Turn {
            flip_h: true,
            ..Turn::default()
        };
        assert_eq!(blitted(&image, flipped, 4, placement), [white; 4]);
    }

    #[test]
    fn nearest_needs_no_step_at_any_enlargement() {
        // the same 2 pixels drawn whole over 131,072 without antialiasing: half and half
        let pair = [[0, 0, 0, 255], [255, 255, 255, 255]].concat();
        let image = Image {
            width: 2,
            height: 1,
            levels: &[&pair],
            opaque: true,
        };
        let mut view = viewport((2.0, 1.0), (2.0, 1.0));
        view.antialias = false;
        let (black, white) = (canvas::rgb(0, 0, 0), canvas::rgb(255, 255, 255));
        for (flip_h, first, second) in [(false, black, white), (true, white, black)] {
            let turn = Turn {
                flip_h,
                ..Turn::default()
            };
            let mut pixels = vec![0; 131_072];
            let mut canvas = Canvas::new(&mut pixels, 131_072, 1).unwrap();
            draw(&mut canvas, &image, &view, turn, None, 1.0, true);
            let (left, right) = pixels.split_at(65_536);
            assert!(left.iter().all(|&p| p == first), "flipped: {flip_h}");
            assert!(right.iter().all(|&p| p == second), "flipped: {flip_h}");
        }
    }

    #[test]
    fn a_frame_at_100_percent_is_the_image_s_own_pixels() {
        // 5 × 3 of distinct colors in a larger view, every turn and flip
        let rgba: Vec<u8> = (0..15u8)
            .flat_map(|i| [i * 17, 255 - i * 17, i * 5, 255])
            .collect();
        let image = Image {
            width: 5,
            height: 3,
            levels: &[&rgba],
            opaque: true,
        };
        for rotation in [0, 90, 180, 270] {
            for (flip_h, flip_v) in [(false, false), (true, false), (false, true)] {
                let turn = Turn {
                    rotation,
                    flip_h,
                    flip_v,
                };
                let mut view = viewport((9.0, 7.0), (5.0, 3.0));
                if rotation % 180 == 90 {
                    view = viewport((9.0, 7.0), (3.0, 5.0));
                }
                let mut smooth = [0; 63];
                draw_into(&mut smooth, (9, 7), &image, &view, turn);
                view.antialias = false;
                let mut nearest = [0; 63];
                draw_into(&mut nearest, (9, 7), &image, &view, turn);
                assert_eq!(smooth, nearest, "{turn:?}");
            }
        }
    }

    #[test]
    fn sizes_whose_bytes_overflow_end_the_pyramid() {
        let image = Image {
            width: 1 << 31,
            height: 1 << 31,
            levels: &[&[]],
            opaque: true,
        };
        assert!(valid_levels(&image).is_empty());
        let mut pixels = [0; 4];
        let mut canvas = Canvas::new(&mut pixels, 2, 2).unwrap();
        blit(
            &mut canvas,
            &image,
            Turn::default(),
            (0, 0, 2, 2),
            ((0, 0), (2, 2)),
            true,
            None,
        );
        assert_eq!(pixels, [0; 4]);
    }

    #[test]
    fn levels_serve_without_enlarging() {
        // 8 × 8 in red, its levels in green and blue, so the colors tell which level drew
        let (red, green, blue) = (
            solid(8, 8, [255, 0, 0, 255]),
            solid(4, 4, [0, 255, 0, 255]),
            solid(2, 2, [0, 0, 255, 255]),
        );
        let image = Image {
            width: 8,
            height: 8,
            levels: &[&red, &green, &blue],
            opaque: true,
        };
        let drawn = |zoom: f64| {
            let mut view = viewport((8.0, 8.0), (8.0, 8.0));
            view.set_zoom(zoom, None);
            let mut pixels = [0; 64];
            draw_into(&mut pixels, (8, 8), &image, &view, Turn::default());
            pixels[36]
        };
        assert_eq!(drawn(1.0), canvas::rgb(255, 0, 0));
        assert_eq!(
            drawn(0.75),
            canvas::rgb(255, 0, 0),
            "level 1 would need enlarging"
        );
        assert_eq!(drawn(0.5), canvas::rgb(0, 255, 0));
        assert_eq!(drawn(0.375), canvas::rgb(0, 255, 0));
        assert_eq!(drawn(0.25), canvas::rgb(0, 0, 255));
        assert_eq!(
            drawn(0.125),
            canvas::rgb(0, 0, 255),
            "the smallest level serves below it"
        );
        // a level of the wrong size ends the pyramid
        let short = solid(3, 4, [0, 255, 0, 255]);
        let image = Image {
            levels: &[&red, &short, &blue],
            ..image
        };
        let mut view = viewport((8.0, 8.0), (8.0, 8.0));
        view.set_zoom(0.25, None);
        let mut pixels = [0; 64];
        draw_into(&mut pixels, (8, 8), &image, &view, Turn::default());
        assert_eq!(pixels[36], canvas::rgb(255, 0, 0));
    }

    #[test]
    fn imlib2_samples_photo_150_panned_from_a_larger_image() {
        // the reference frame's crop rounds outward, so the rectangle it asks Imlib2 for hangs
        // off the canvas, and Imlib2's clipping then samples the whole image scaled to more
        // than the zoom. Drawn from what Imlib2 samples, the golden frame matches within the
        // photo's decoder difference, which proves the cause; `draw` keeps the zoom instead.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
        let photo = nsxiv_ng_decode::decode(&root.join("fixtures/photo.jpg"), 1 << 30).unwrap();
        let golden = image::open(root.join("golden/photo-150-panned.png"))
            .unwrap()
            .to_rgba8()
            .into_raw();
        // the keys `= + l l d` on a 256 × 192 view
        let mut view = viewport((256.0, 192.0), (640.0, 480.0));
        view.set_zoom(1.5, None);
        view.scroll_step(nsxiv_ng_core::Direction::Right, false);
        view.scroll_step(nsxiv_ng_core::Direction::Right, false);
        view.scroll_step(nsxiv_ng_core::Direction::Down, false);
        let (r, z) = (view.image_rect(), view.zoom());
        // the reference frame's source crop and destination, then Imlib2's integer clip of
        // them: the source shrinks with the destination, and the scale follows the shrunk
        // source
        let sampled = |d: f64, extent: i64, size: i64| {
            let start = (-d / z).floor().max(0.0);
            let end = ((extent as f64 - d) / z).ceil().min(size as f64);
            let (s0, s) = (start as i64, (end - start) as i64);
            let (at, length) = (
                (d + start * z).round() as i64,
                ((end - start) * z).round() as i64,
            );
            let first = at.max(0);
            let sw = s * ((at + length).min(extent) - first) / length;
            let offset = (first - at) + s0 * length / sw;
            ((at, length), (first - offset, length * size / sw))
        };
        let (x, origin_x) = sampled(r.x, 256, 640);
        let (y, origin_y) = sampled(192.0 - r.max_y(), 192, 480);
        assert_eq!((origin_x, origin_y), ((-459, 971), (-308, 733)));
        let image = Image {
            width: photo.width,
            height: photo.height,
            levels: &[&photo.rgba],
            opaque: true,
        };
        let mut pixels = vec![canvas::rgb(255, 255, 255); 256 * 192];
        let mut canvas = Canvas::new(&mut pixels, 256, 192).unwrap();
        let placement = ((origin_x.0, origin_y.0), (origin_x.1, origin_y.1));
        blit(
            &mut canvas,
            &image,
            Turn::default(),
            (x.0, y.0, x.1, y.1),
            placement,
            true,
            None,
        );
        let (mut largest, mut off) = (0, 0);
        for (&pixel, expected) in pixels.iter().zip(golden.as_chunks::<4>().0) {
            let [_, r, g, b] = pixel.to_be_bytes();
            for (a, &b) in [r, g, b].into_iter().zip(&expected[..3]) {
                largest = largest.max(a.abs_diff(b));
                off += usize::from(a.abs_diff(b) > 2);
            }
        }
        let share = off as f64 * 100.0 / (256.0 * 192.0 * 3.0);
        assert!(largest <= 10 && share <= 5.48, "{largest}, {share:.3}%");
    }
}
