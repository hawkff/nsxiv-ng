//! Where each drawn pixel samples the source, at the points Imlib2 samples, and the scaling
//! from the pyramid level that serves: averaged by area going down, bilinear through
//! fast_image_resize going up, and nearest without antialiasing.

use std::borrow::Cow;
use std::ops::Range;

use fast_image_resize::images::{CroppedImage, Image as Pixels, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use super::{Image, Turn};

/// How one axis of the canvas samples one axis of the image as shown, turned and flipped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Axis {
    /// The first canvas pixel drawn, and how many are.
    pub first: i64,
    pub count: usize,
    /// The drawn pixels the scaled image reaches: `lead` pixels in, `samples` of them. The
    /// pixels around them, at most one on a side as the rectangle rounds, repeat the edge.
    lead: i64,
    samples: usize,
    /// The whole image's scaled size, and the first sample's offset into it.
    scaled: i64,
    offset: i64,
    /// The image's size along the axis, and whether the axis enlarges it, so interpolates
    /// rather than averages; one that keeps it does neither.
    size: i64,
    enlarges: bool,
    /// The samples as a crop: sample `j` lies around `left + (j + 0.5) × step` in level-0
    /// pixels. A step of 0, from an enlargement so large that Imlib2's 16.16 step rounds to
    /// nothing, takes the first pixel shown for every sample, as Imlib2's arithmetic does.
    left: f64,
    step: f64,
}

impl Axis {
    /// The axis drawing the canvas pixels `start..start + length` of `extent`, from an image
    /// `size` long that starts at `origin` scaled to `scaled` pixels. Sampling follows the
    /// scaled image alone, so a pan moves the frame by whole pixels and the zoom stays.
    pub(crate) fn new(
        extent: i64,
        size: i64,
        (start, length): (i64, i64),
        (origin, scaled): (i64, i64),
    ) -> Option<Axis> {
        let first = start.max(0);
        let last = start.saturating_add(length).min(extent);
        let sampled = (first.max(origin), last.min(origin.saturating_add(scaled)));
        if last <= first || sampled.1 <= sampled.0 || size <= 0 {
            return None;
        }
        let offset = sampled.0 - origin;
        let enlarges = scaled > size;
        let (step, left) = if scaled == size {
            (1.0, offset as f64)
        } else if enlarges && size > 1 {
            // corners on corners, as `__imlib_CalcPoints` scales up: pixel i samples
            // i × (size − 1) / (scaled − 1), in 16.16
            let step = (((size - 1) << 16) / (scaled - 1)) as f64 / 65536.0;
            (step, offset as f64 * step + 0.5 - 0.5 * step)
        } else {
            // the box from i × size / scaled to (i + 1) × size / scaled
            let step = size as f64 / scaled as f64;
            (step, offset as f64 * step)
        };
        Some(Axis {
            first,
            count: usize::try_from(last - first).ok()?,
            lead: sampled.0 - first,
            samples: usize::try_from(sampled.1 - sampled.0).ok()?,
            scaled,
            offset,
            size,
            enlarges,
            left,
            step,
        })
    }

    /// The step rounded to nothing, so every sample is the first pixel shown.
    fn constant(&self) -> bool {
        self.step == 0.0
    }

    /// The axis takes source pixels as they are: it keeps the image's size, or its step is 0.
    fn copies(&self) -> bool {
        self.scaled == self.size || self.constant()
    }

    /// How many samples the resize makes along the axis: one when it is constant.
    fn resampled(&self) -> usize {
        if self.constant() { 1 } else { self.samples }
    }

    /// The sample each drawn pixel shows of the resize, counted from the far end on a
    /// reversed axis.
    fn order(&self, reversed: bool) -> Vec<usize> {
        self.spread(self.resampled(), reversed)
    }

    /// Which of `samples` each drawn pixel shows, counted from the far end on a reversed axis.
    fn spread(&self, samples: usize, reversed: bool) -> Vec<usize> {
        let last = samples as i64 - 1;
        (0..self.count as i64)
            .map(|j| {
                let sample = (j - self.lead).clamp(0, last);
                (if reversed { last - sample } else { sample }) as usize
            })
            .collect()
    }

    /// The source pixel each drawn pixel takes, counted from the far end on a reversed axis:
    /// without antialiasing ⌊i × size / scaled⌋, as `__imlib_ScaleSampleRGBA` takes it, which
    /// is the pixel itself on an axis that keeps the size, and with antialiasing the first
    /// pixel shown on a constant axis. Without it the integers have no step to round to 0.
    fn indices(&self, reversed: bool, antialias: bool) -> Vec<usize> {
        let size = self.size;
        self.spread(self.samples, false)
            .into_iter()
            .map(|sample| {
                let i = if antialias && self.constant() {
                    0
                } else {
                    // wide, so no placement overflows
                    let i = (i128::from(self.offset) + sample as i128) * i128::from(size)
                        / i128::from(self.scaled);
                    i.clamp(0, i128::from(size - 1)) as i64
                };
                (if reversed { size - 1 - i } else { i }) as usize
            })
            .collect()
    }

    /// The span of the source axis the samples cover, in the pixels of `level`, which is
    /// `level_size` long.
    fn crop(&self, reversed: bool, level: usize, level_size: u32) -> (f64, f64) {
        let size = self.size;
        let (mut from, mut to) = if self.constant() {
            (0.0, 1.0)
        } else {
            (self.left, self.left + self.samples as f64 * self.step)
        };
        if reversed {
            (from, to) = (size as f64 - to, size as f64 - from);
        }
        let divisor = (level as f64).exp2();
        // the clamps take only rounding noise: samples inside the image cover only it
        (
            (from / divisor).max(0.0),
            (to / divisor).min(f64::from(level_size)),
        )
    }
}

/// Which source axis each shown axis is, and whether it runs backward: (shown x, shown y),
/// each as (along the source's y axis, reversed).
fn axes(turn: Turn) -> ((bool, bool), (bool, bool)) {
    let (h, v) = (turn.flip_h, turn.flip_v);
    // the turn applies first and the flips to what it shows
    match turn.rotation % 360 {
        90 => ((true, !h), (false, v)),
        180 => ((false, !h), (true, !v)),
        270 => ((true, h), (false, !v)),
        _ => ((false, h), (true, v)),
    }
}

/// Pixels a frame is drawn from: straight-alpha RGBA, `width` to a row, and for each shown
/// axis the pixel along its source axis that each drawn pixel takes.
pub(crate) struct Sampled<'a> {
    pub rgba: Cow<'a, [u8]>,
    pub width: usize,
    pub columns: Vec<usize>,
    pub rows: Vec<usize>,
    /// The shown x axis runs along the source's y axis: a quarter turn.
    pub swapped: bool,
}

/// A crop of a level reduced by area while the frame is drawn, a row or a few columns at a
/// time through a [`Reducer`], so no scaled copy of the whole crop is made.
pub(crate) struct Reduced<'a> {
    pixels: &'a [u8],
    width: usize,
    opaque: bool,
    across: Taps,
    down: Taps,
    /// For each shown axis, the sample each drawn pixel shows, as [`Sampled`] has them.
    pub columns: Vec<usize>,
    pub rows: Vec<usize>,
    /// The shown x axis runs along the source's y axis: a quarter turn.
    pub swapped: bool,
    /// The crop in level pixels, across and down, for the tests' whole reduce.
    #[cfg(test)]
    crop: ((f64, f64), (f64, f64)),
}

impl Reduced<'_> {
    /// The samples across the source and down it.
    pub(crate) fn size(&self) -> (usize, usize) {
        (self.across.spans.len(), self.down.spans.len())
    }
}

/// What a frame is drawn from.
pub(crate) enum Source<'a> {
    /// Pixels at hand: level 0 itself, or a crop scaled up.
    Sampled(Sampled<'a>),
    /// A crop scaled down, reduced as it is drawn.
    Reduced(Reduced<'a>),
}

/// The pixels the axes draw: level 0 itself where no axis scales or without antialiasing,
/// else the part they show scaled from the smallest level that has as many pixels as they
/// need.
pub(crate) fn sample<'a>(
    image: &Image<'a>,
    turn: Turn,
    x: &Axis,
    y: &Axis,
    antialias: bool,
) -> Option<Source<'a>> {
    let ((x_is_y, x_reversed), (_, y_reversed)) = axes(turn);
    if !antialias || (x.copies() && y.copies()) {
        // shortcut: nearest reads level 0, as only it gives Imlib2's pixels exactly; it
        // touches one source pixel a drawn pixel, so a smaller level would gain little
        return Some(Source::Sampled(Sampled {
            rgba: Cow::Borrowed(image.levels[0]),
            width: image.width as usize,
            columns: x.indices(x_reversed, antialias),
            rows: y.indices(y_reversed, antialias),
            swapped: x_is_y,
        }));
    }
    let Plan {
        level,
        size: (level_width, level_height),
        crops: (horizontal, vertical),
        counts,
        enlarges,
    } = plan(image, turn, x, y);
    let pixels = image.levels[level];
    if enlarges {
        let rgba = bilinear(
            pixels,
            (level_width, level_height),
            image.opaque,
            (horizontal, vertical),
            counts,
        )?;
        return Some(Source::Sampled(Sampled {
            rgba: Cow::Owned(rgba),
            width: counts.0,
            columns: x.order(x_reversed),
            rows: y.order(y_reversed),
            swapped: x_is_y,
        }));
    }
    let width = level_width as usize;
    let height = pixels.len() / (width * 4);
    Some(Source::Reduced(Reduced {
        pixels,
        width,
        opaque: image.opaque,
        across: taps(horizontal.0, horizontal.1, counts.0, width),
        down: taps(vertical.0, vertical.1, counts.1, height),
        columns: x.order(x_reversed),
        rows: y.order(y_reversed),
        swapped: x_is_y,
        #[cfg(test)]
        crop: (horizontal, vertical),
    }))
}

/// How the axes are sampled where antialiasing scales them: from the level that serves, its
/// size, the crop of it along the source's x and y axes, the samples across and down, and
/// whether they enlarge, through the bilinear filter, or reduce, by area.
struct Plan {
    level: usize,
    size: (u32, u32),
    crops: ((f64, f64), (f64, f64)),
    counts: (usize, usize),
    enlarges: bool,
}

fn plan(image: &Image, turn: Turn, x: &Axis, y: &Axis) -> Plan {
    let ((x_is_y, x_reversed), (_, y_reversed)) = axes(turn);
    // the source's own x and y axes
    let ((across, across_reversed), (down, down_reversed)) = if x_is_y {
        ((y, y_reversed), (x, x_reversed))
    } else {
        ((x, x_reversed), (y, y_reversed))
    };
    // the smallest level that needs no enlarging
    let (mut level, mut level_width, mut level_height) = (0, image.width, image.height);
    while level + 1 < image.levels.len() {
        let (w, h) = (level_width.div_ceil(2), level_height.div_ceil(2));
        if i64::from(w) < across.scaled || i64::from(h) < down.scaled {
            break;
        }
        (level, level_width, level_height) = (level + 1, w, h);
    }
    let enlarges = |axis: &Axis| axis.enlarges && !axis.constant();
    Plan {
        level,
        size: (level_width, level_height),
        crops: (
            across.crop(across_reversed, level, level_width),
            down.crop(down_reversed, level, level_height),
        ),
        counts: (across.resampled(), down.resampled()),
        // shortcut: an axis that enlarges makes both bilinear, where Imlib2 filters each axis
        // on its own; only a whole draw stretched unevenly enlarges one axis and reduces the
        // other
        enlarges: enlarges(across) || enlarges(down),
    }
}

/// Whether the axes `part` draw give the very pixels the axes `whole` give where both draw:
/// where both take level 0 as it is, as a copy or without antialiasing, and where both
/// enlarge the same level through the bilinear filter with the same weights for every sample
/// they share, stored at the same precision. `within` asks as well that `part` draws no
/// sample `whole` does not, as for a part of the canvas `whole` draws. A reduce, whose taps
/// move with a crop's float edges, is not taken as the same.
pub(crate) fn same_as_whole(
    image: &Image,
    turn: Turn,
    whole: (&Axis, &Axis),
    part: (&Axis, &Axis),
    antialias: bool,
    within: bool,
) -> bool {
    let copies = |(x, y): (&Axis, &Axis)| x.copies() && y.copies();
    if !antialias || (copies(whole) && copies(part)) {
        return true;
    }
    if copies(whole) || copies(part) {
        return false;
    }
    let (whole, part) = (
        plan(image, turn, whole.0, whole.1),
        plan(image, turn, part.0, part.1),
    );
    if !(whole.enlarges && part.enlarges && whole.level == part.level) {
        return false;
    }
    let axis = |plan: &Plan, across: bool| {
        let ((left, right), (top, bottom)) = plan.crops;
        let (from, to, limit, count) = if across {
            (left, right, plan.size.0, plan.counts.0)
        } else {
            (top, bottom, plan.size.1, plan.counts.1)
        };
        let (start, length) = window(from, to, limit);
        weights(start, length, from - f64::from(start), to - from, count)
    };
    [true, false]
        .into_iter()
        .all(|across| axis(&whole, across).holds(&axis(&part, across), within))
}

/// The pixels of a level `limit` long that the bilinear filter reads for the crop `from..to`
/// of it: the first and how many. Only these are premultiplied, so that stays cheap.
fn window(from: f64, to: f64, limit: u32) -> (u32, u32) {
    let start = (from.floor() as i64 - 2).max(0) as u32;
    let end = (to.ceil() as i64 + 2).min(i64::from(limit)) as u32;
    (start, end - start)
}

/// The weights fast_image_resize gives the samples along an axis, each with the first pixel
/// of the level it reads, and the precision it stores them at; none where it takes the axis
/// as it is.
struct Weights {
    samples: Vec<(u32, Vec<f64>)>,
    /// The first sample's center in the level's pixels.
    first: f64,
    step: f64,
    precision: Option<u8>,
}

impl Weights {
    /// Whether every sample of `part` that `self` has a sample at the center of has its
    /// weights, stored at the same precision; with `within`, every sample of `part` must be.
    fn holds(&self, part: &Weights, within: bool) -> bool {
        if self.precision != part.precision || self.step != part.step {
            return false;
        }
        if self.precision.is_none() {
            // both taken as they are, a pixel a sample
            return (part.first - self.first).fract() == 0.0;
        }
        let shift = (part.first - self.first) / self.step;
        if shift.fract() != 0.0 || (within && shift < 0.0) {
            return false;
        }
        let shift = shift as i64;
        part.samples.iter().enumerate().all(|(i, sample)| {
            let whole = usize::try_from(i as i64 + shift).ok();
            match whole.and_then(|i| self.samples.get(i)) {
                Some(whole) => whole == sample,
                None => !within,
            }
        })
    }
}

/// The bilinear weights fast_image_resize gives the crop `in0..in0 + length` of the window of
/// `size` pixels from `start` of a level, scaled to `count` samples.
// shortcut: a replica of fast_image_resize 6.1.0's bilinear weights (convolution/mod.rs,
// precompute_coefficients, and filters.rs, bilinear_filter), of when it filters an axis at all
// (resizer.rs, do_convolution) and of the precision it stores them in for 8-bit pixels
// (convolution/optimisations.rs, Normalizer16::new); it depends on that crate's internals, so
// on a new version check it against that source again: a difference shows as moved frames
// that differ from those drawn whole in the test that pans over a sweep of zooms
fn weights(start: u32, size: u32, in0: f64, length: f64, count: usize) -> Weights {
    let in1 = in0 + length;
    let scale = (in1 - in0) / count as f64;
    let first = f64::from(start) + in0 + 0.5 * scale;
    let needed = count as f64 != length || in0 != in0.round();
    if !needed || count == 0 || scale <= 0.0 {
        return Weights {
            samples: Vec::new(),
            first,
            step: scale,
            precision: None,
        };
    }
    // the bilinear filter's support is a pixel, widened by the scale where it reduces
    let filter_scale = scale.max(1.0);
    let radius = filter_scale;
    let recip = 1.0 / filter_scale;
    let window = radius.ceil() as usize * 2 + 1;
    let mut samples = Vec::with_capacity(count);
    let mut max = 0.0f64;
    for out in 0..count {
        let in_center = in0 + (out as f64 + 0.5) * scale;
        let x_min = (in_center - radius).floor().max(0.0) as u32;
        let x_max = (in_center + radius).ceil().min(f64::from(size)) as u32;
        let center = in_center - 0.5;
        let mut bound = x_min;
        let mut values = Vec::with_capacity(window);
        let mut sum = 0.0;
        for x in x_min..x_max {
            let w = (f64::from(x) - center) * recip;
            let w = if w.abs() < 1.0 { 1.0 - w.abs() } else { 0.0 };
            if x == bound && w == 0.0 {
                bound += 1;
            } else {
                values.push(w);
                sum += w;
            }
        }
        if sum != 0.0 {
            values.iter_mut().for_each(|w| *w /= sum);
        }
        max = values.iter().copied().fold(max, f64::max);
        samples.push((start + bound, values));
    }
    // the largest weight sets the precision: the most bits that keep it within an i16
    let mut precision = 0;
    for bits in 0..22u8 {
        precision = bits;
        if (max * f64::from(1u32 << (bits + 1))).round() as i64 >= 1 << 15 {
            break;
        }
    }
    Weights {
        samples,
        first,
        step: scale,
        precision: Some(precision),
    }
}

/// The crop of a level scaled to `columns` × `rows` by fast_image_resize's bilinear filter.
fn bilinear(
    pixels: &[u8],
    (width, height): (u32, u32),
    opaque: bool,
    ((left, right), (top, bottom)): ((f64, f64), (f64, f64)),
    (columns, rows): (usize, usize),
) -> Option<Vec<u8>> {
    let (window_left, window_width) = window(left, right, width);
    let (window_top, window_height) = window(top, bottom, height);
    let source = ImageRef::new(width, height, pixels, PixelType::U8x4).ok()?;
    let source = CroppedImage::new(
        &source,
        window_left,
        window_top,
        window_width,
        window_height,
    )
    .ok()?;
    let mut scaled = Pixels::new(
        u32::try_from(columns).ok()?,
        u32::try_from(rows).ok()?,
        PixelType::U8x4,
    );
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
        .crop(
            left - f64::from(window_left),
            top - f64::from(window_top),
            right - left,
            bottom - top,
        )
        .use_alpha(!opaque);
    Resizer::new().resize(&source, &mut scaled, &options).ok()?;
    Some(scaled.into_vec())
}

/// Weights in 2.14 fixed point; a pixel's sum to one.
const ONE: u16 = 1 << 14;

/// For each destination pixel of an axis, the first source pixel it averages and the range
/// of `weights` that holds how much of each it covers.
struct Taps {
    spans: Vec<(usize, std::ops::Range<usize>)>,
    weights: Vec<u16>,
}

/// The source pixels that the boxes splitting `from..to` into `count` cover, and by how
/// much, on an axis `limit` pixels long.
fn taps(from: f64, to: f64, count: usize, limit: usize) -> Taps {
    let step = (to - from) / count as f64;
    let mut spans = Vec::with_capacity(count);
    let mut weights = Vec::with_capacity(count * (step.ceil() as usize + 1));
    for i in 0..count {
        let (start, end) = (from + i as f64 * step, from + (i + 1) as f64 * step);
        let first = (start.floor().max(0.0) as usize).min(limit - 1);
        let last = (end.ceil() as usize).clamp(first + 1, limit);
        // each weight is the step between the box's rounded coverage at the pixel's two
        // edges, so none is negative and together they make exactly one
        let covered =
            |edge: f64| (((edge - start) / step).clamp(0.0, 1.0) * f64::from(ONE)).round() as u16;
        let at = weights.len();
        let mut before = 0;
        for pixel in first..last {
            let after = if pixel + 1 == last {
                ONE
            } else {
                covered(pixel as f64 + 1.0)
            };
            weights.push(after - before);
            before = after;
        }
        spans.push((first, at..weights.len()));
    }
    Taps { spans, weights }
}

/// A row of straight-alpha pixels premultiplied into `out`.
fn premultiply(row: &[u8], out: &mut [u8]) {
    for (&[r, g, b, a], out) in row
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0)
    {
        let times = |c: u8| {
            let t = u16::from(c) * u16::from(a) + 128;
            ((t + (t >> 8)) >> 8) as u8
        };
        *out = [times(r), times(g), times(b), a];
    }
}

/// 255 / alpha in 16.16 fixed point, for each alpha, so unpremultiplying needs no division.
fn reciprocals() -> [u32; 256] {
    std::array::from_fn(|a| match a as u32 {
        0 => 0,
        a => ((255 << 16) + a / 2) / a,
    })
}

fn unpremultiply([r, g, b, a]: [u8; 4], reciprocals: &[u32; 256]) -> [u8; 4] {
    let times = reciprocals[usize::from(a)];
    let divide = |c: u8| ((u32::from(c) * times + (1 << 15)) >> 16).min(255) as u8;
    [divide(r), divide(g), divide(b), a]
}

/// Adds each channel of a row times `weight` to its sum. Both factors fit 16 bits, so the
/// products need no 32-bit multiply, which baseline x86-64 lacks.
fn accumulate(sums: &mut [u32], row: &[u8], weight: u16) {
    let (sum_chunks, sum_rest) = sums.as_chunks_mut::<16>();
    let (row_chunks, row_rest) = row.as_chunks::<16>();
    for (sums, row) in sum_chunks.iter_mut().zip(row_chunks) {
        for (sum, &channel) in sums.iter_mut().zip(row) {
            *sum += u32::from(channel) * u32::from(weight);
        }
    }
    for (sum, &channel) in sum_rest.iter_mut().zip(row_rest) {
        *sum += u32::from(channel) * u32::from(weight);
    }
}

/// Reduces samples of a [`Reduced`] crop, each the average of the source area its box
/// covers, weighted by alpha: the rows first, then the columns. It keeps what one row needs.
pub(crate) struct Reducer<'a> {
    reduced: &'a Reduced<'a>,
    /// The sample columns it reduces, and the source columns their boxes reach.
    columns: Range<usize>,
    start: usize,
    end: usize,
    premultiplied: Vec<u8>,
    premultiplied_row: usize,
    sums: Vec<u32>,
    column_sums: Vec<u16>,
    reciprocals: [u32; 256],
}

impl<'a> Reducer<'a> {
    /// A reducer of the sample columns `columns` of the crop.
    pub(crate) fn new(reduced: &'a Reduced<'a>, columns: Range<usize>) -> Reducer<'a> {
        let spans = &reduced.across.spans[columns.clone()];
        let start = spans[0].0;
        let end = spans
            .iter()
            .map(|(first, range)| first + range.len())
            .max()
            .unwrap_or(start);
        Reducer {
            reduced,
            columns,
            start,
            end,
            premultiplied: vec![0; (end - start) * 4],
            premultiplied_row: usize::MAX,
            sums: vec![0; (end - start) * 4],
            column_sums: vec![0; (end - start) * 4],
            reciprocals: reciprocals(),
        }
    }

    /// The reducer's columns of sample row `row`, as straight-alpha RGBA into `out`.
    pub(crate) fn row(&mut self, row: usize, out: &mut [u8]) {
        let Reduced {
            pixels,
            width,
            opaque,
            across,
            down,
            ..
        } = self.reduced;
        let (start, end) = (self.start, self.end);
        let (first, range) = &down.spans[row];
        self.sums.fill(0);
        for (row, &weight) in (*first..).zip(&down.weights[range.clone()]) {
            let mut source = &pixels[(row * width + start) * 4..(row * width + end) * 4];
            if !opaque {
                // a box's last row is often the next one's first
                if self.premultiplied_row != row {
                    premultiply(source, &mut self.premultiplied);
                    self.premultiplied_row = row;
                }
                source = &self.premultiplied;
            }
            accumulate(&mut self.sums, source, weight);
        }
        // 8.8 fixed point, so the second pass fits 32 bits
        for (column, &sum) in self.column_sums.iter_mut().zip(&self.sums) {
            *column = ((sum + 32) >> 6) as u16;
        }
        let column_pixels = self.column_sums.as_chunks::<4>().0;
        let spans = &across.spans[self.columns.clone()];
        for ((first, range), out) in spans.iter().zip(out.as_chunks_mut::<4>().0) {
            let mut total = [0u32; 4];
            for (pixel, &weight) in column_pixels[first - start..]
                .iter()
                .zip(&across.weights[range.clone()])
            {
                for (total, &channel) in total.iter_mut().zip(pixel) {
                    *total += u32::from(channel) * u32::from(weight);
                }
            }
            let pixel = total.map(|t| ((t + (1 << 21)) >> 22).min(255) as u8);
            *out = if *opaque {
                pixel
            } else {
                unpremultiply(pixel, &self.reciprocals)
            };
        }
    }
}

/// The crop of a level `width` pixels wide reduced to `columns` × `rows` as one buffer, as
/// frames were drawn before they were reduced band by band: what the tests compare them with.
#[cfg(test)]
pub(super) fn reduce(
    pixels: &[u8],
    width: usize,
    opaque: bool,
    ((left, right), (top, bottom)): ((f64, f64), (f64, f64)),
    (columns, rows): (usize, usize),
) -> Vec<u8> {
    let height = pixels.len() / (width * 4);
    let across = taps(left, right, columns, width);
    let down = taps(top, bottom, rows, height);
    // the columns the boxes reach
    let start = across.spans[0].0;
    let end = across
        .spans
        .last()
        .map_or(start, |(first, range)| first + range.len());
    let mut premultiplied = vec![0; (end - start) * 4];
    let mut premultiplied_row = usize::MAX;
    let mut sums = vec![0u32; (end - start) * 4];
    let mut column_sums = vec![0u16; (end - start) * 4];
    let mut out = vec![0; columns * rows * 4];
    let reciprocals = reciprocals();
    for ((first, range), out) in down.spans.iter().zip(out.chunks_exact_mut(columns * 4)) {
        sums.fill(0);
        for (row, &weight) in (*first..).zip(&down.weights[range.clone()]) {
            let mut source = &pixels[(row * width + start) * 4..(row * width + end) * 4];
            if !opaque {
                if premultiplied_row != row {
                    premultiply(source, &mut premultiplied);
                    premultiplied_row = row;
                }
                source = &premultiplied;
            }
            accumulate(&mut sums, source, weight);
        }
        for (column, &sum) in column_sums.iter_mut().zip(&sums) {
            *column = ((sum + 32) >> 6) as u16;
        }
        let column_pixels = column_sums.as_chunks::<4>().0;
        for ((first, range), out) in across.spans.iter().zip(out.as_chunks_mut::<4>().0) {
            let mut total = [0u32; 4];
            for (pixel, &weight) in column_pixels[first - start..]
                .iter()
                .zip(&across.weights[range.clone()])
            {
                for (total, &channel) in total.iter_mut().zip(pixel) {
                    *total += u32::from(channel) * u32::from(weight);
                }
            }
            let pixel = total.map(|t| ((t + (1 << 21)) >> 22).min(255) as u8);
            *out = if opaque {
                pixel
            } else {
                unpremultiply(pixel, &reciprocals)
            };
        }
    }
    out
}

/// [`sample`] as frames were sampled before they were reduced band by band, a crop scaled
/// down into one buffer by [`reduce`]: what the tests compare the banded drawing with.
#[cfg(test)]
pub(crate) fn sample_whole<'a>(
    image: &Image<'a>,
    turn: Turn,
    x: &Axis,
    y: &Axis,
    antialias: bool,
) -> Option<Sampled<'a>> {
    match sample(image, turn, x, y, antialias)? {
        Source::Sampled(sampled) => Some(sampled),
        Source::Reduced(reduced) => {
            let (columns, rows) = reduced.size();
            let (pixels, width, opaque) = (reduced.pixels, reduced.width, reduced.opaque);
            let rgba = reduce(pixels, width, opaque, reduced.crop, (columns, rows));
            Some(Sampled {
                rgba: Cow::Owned(rgba),
                width: columns,
                columns: reduced.columns.clone(),
                rows: reduced.rows.clone(),
                swapped: reduced.swapped,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reducer_of_some_columns_gives_the_pixels_of_the_whole() {
        let image = |w: usize, h: usize| -> Vec<u8> {
            (0..w * h)
                .flat_map(|i| {
                    let (x, y) = (i % w, i / w);
                    [
                        (x * 7 + y) as u8,
                        (y * 5) as u8,
                        (x * y) as u8,
                        (128 + x + y) as u8,
                    ]
                })
                .collect()
        };
        for opaque in [false, true] {
            // the last one crosses strips of 64 columns, the last strip short
            for ((w, h), crop, (columns, rows)) in [
                ((97, 61), ((0.0, 97.0), (0.0, 61.0)), (40, 25)),
                ((97, 61), ((3.3, 90.1), (2.7, 59.9)), (61, 43)),
                ((97, 61), ((10.0, 20.0), (5.0, 60.0)), (7, 33)),
                ((300, 40), ((1.5, 298.5), (0.0, 40.0)), (150, 20)),
            ] {
                let pixels = image(w, h);
                let whole = reduce(&pixels, w, opaque, crop, (columns, rows));
                let ((left, right), (top, bottom)) = crop;
                let reduced = Reduced {
                    pixels: &pixels,
                    width: w,
                    opaque,
                    across: taps(left, right, columns, w),
                    down: taps(top, bottom, rows, h),
                    columns: Vec::new(),
                    rows: Vec::new(),
                    swapped: false,
                    crop,
                };
                // a few columns at a time, rows in any order
                for chunk in [1, 3, 64] {
                    for start in (0..columns).step_by(chunk) {
                        let part = start..(start + chunk).min(columns);
                        let mut reducer = Reducer::new(&reduced, part.clone());
                        let mut out = vec![0; part.len() * 4];
                        for row in (0..rows).rev() {
                            reducer.row(row, &mut out);
                            let at = (row * columns + part.start) * 4;
                            assert_eq!(out, whole[at..at + out.len()], "{crop:?} {part:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn boxes_weigh_what_they_cover() {
        // 5 pixels into 4: each box 1.25 wide
        let taps = taps(0.0, 5.0, 4, 5);
        let spans: Vec<(usize, Vec<u16>)> = taps
            .spans
            .iter()
            .map(|(first, range)| (*first, taps.weights[range.clone()].to_vec()))
            .collect();
        // four fifths and a fifth, then three fifths and two
        assert_eq!(spans[0], (0, vec![13107, 3277]));
        assert_eq!(spans[1], (1, vec![9830, 6554]));
        assert!(spans.iter().all(|(_, w)| w.iter().sum::<u16>() == ONE));
    }

    #[test]
    fn every_box_weighs_exactly_one() {
        // boxes so wide that each weight rounds, from a crop that starts inside a pixel
        for ratio in [181, 500, 4097] {
            let taps = taps(0.25, 0.25 + 2.0 * ratio as f64, 2, 2 * ratio + 1);
            for (_, range) in &taps.spans {
                let sum: u32 = taps.weights[range.clone()]
                    .iter()
                    .map(|&w| u32::from(w))
                    .sum();
                assert_eq!(sum, u32::from(ONE), "1 in {ratio}");
            }
        }
        let white = [255; 4].repeat(500 * 500);
        let out = reduce(&white, 500, true, ((0.0, 500.0), (0.0, 500.0)), (1, 1));
        assert_eq!(out, [255; 4], "500 × 500 into one pixel");
    }

    #[test]
    fn no_placement_overflows_the_nearest_pixel() {
        let axis = Axis::new(
            1,
            2,
            (0, 1),
            (-5_000_000_000_000_000_000, 9_000_000_000_000_000_000),
        )
        .unwrap();
        assert_eq!(axis.indices(false, false), [1]);
        assert_eq!(axis.indices(true, false), [0]);
    }

    #[test]
    fn reducing_averages_by_area_and_alpha() {
        // black, white, black, white, black, in 1.25-pixel boxes, as Imlib2 averages them
        let row: Vec<u8> = [0u8, 255, 0, 255, 0]
            .iter()
            .flat_map(|&c| [c, c, c, 255])
            .collect();
        let out = reduce(&row, 5, true, ((0.0, 5.0), (0.0, 1.0)), (4, 1));
        let levels: Vec<u8> = out.iter().step_by(4).copied().collect();
        assert_eq!(levels, [51, 153, 153, 51]);
        // a clear pixel's hidden red lends nothing to an opaque green's color, which comes
        // back from half alpha within its rounding
        let pixels = [[255, 0, 0, 0], [0, 200, 0, 255]].concat();
        let out = reduce(&pixels, 2, false, ((0.0, 2.0), (0.0, 1.0)), (1, 1));
        assert_eq!(out, [0, 199, 0, 128]);
    }
}
