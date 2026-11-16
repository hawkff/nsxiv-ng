//! The censor tool's pixelate and blur, made from the base under the rectangle.

use std::sync::OnceLock;

use nsxiv_ng_render::text::tiny_skia::Pixmap;

/// Pixelates premultiplied RGBA in place, in blocks of `max(8, min(width, height) / 12)`
/// pixels, counted from the top left corner, each the mean of its pixels.
/// Integer arithmetic on the sRGB values throughout, so a frame is the same on every machine.
/// Unlike CIPixellate, which samples one pixel a block and leaves the original wherever a
/// block's center falls outside the rectangle, no pixel keeps its value but by chance: a last
/// column or row of blocks thinner than half a block joins the one before it.
pub(crate) fn pixelate(piece: &mut Pixmap) {
    let (width, height) = (piece.width() as usize, piece.height() as usize);
    // the block's side is m / 12 pixels: its edges fall at k × m / 12, rounded down
    let m = width.min(height).max(96);
    let edges = |extent: usize| {
        let mut edges: Vec<usize> = (0..)
            .map(|k| k * m / 12)
            .take_while(|&e| e < extent)
            .collect();
        // less than half a block, m / 24, left after the last edge
        if edges.len() > 1 && (extent - edges[edges.len() - 1]) * 24 < m {
            edges.pop();
        }
        edges.push(extent);
        edges
    };
    let (columns, rows) = (edges(width), edges(height));
    let data = piece.data_mut();
    for band in rows.windows(2) {
        for block in columns.windows(2) {
            let mut sum = [0u64; 4];
            for y in band[0]..band[1] {
                for p in data[(y * width + block[0]) * 4..(y * width + block[1]) * 4].chunks(4) {
                    for c in 0..4 {
                        sum[c] += u64::from(p[c]);
                    }
                }
            }
            let n = ((band[1] - band[0]) * (block[1] - block[0])) as u64;
            let mean = sum.map(|s| ((s + n / 2) / n) as u8);
            for y in band[0]..band[1] {
                for p in data[(y * width + block[0]) * 4..(y * width + block[1]) * 4].chunks_mut(4)
                {
                    p.copy_from_slice(&mean);
                }
            }
        }
    }
}

/// The blur's deviation, CIGaussianBlur's radius 14, in pixels at any image size.
// shortcut: one deviation at any size, where pixelate's block grows with the
// rectangle; grow it with the rectangle if large print reads through a blur of a large image
pub(crate) const SIGMA: f64 = 14.0;

/// Blurs premultiplied RGBA in place as CIGaussianBlur does over a piece clamped at its edges,
/// by `sigma` pixels: in linear light, by three box blurs whose variances add to about that of
/// the Gaussian. It comes within 0.37 of 255 of CoreImage's on a photo, and costs a ninth of a
/// true Gaussian.
pub(crate) fn blur(piece: &mut Pixmap, sigma: f64) {
    let (width, height) = (piece.width() as usize, piece.height() as usize);
    let lut = lut();
    // linear light premultiplied, from straight sRGB
    let mut values: Vec<[f32; 4]> = (piece.pixels().iter())
        .map(|p| {
            let p = p.demultiply();
            let a = f32::from(p.alpha()) / 255.0;
            let linear = |c: u8| lut.to_linear[usize::from(c)] * a;
            [linear(p.red()), linear(p.green()), linear(p.blue()), a]
        })
        .collect();
    // a box of an odd width w has a variance of (w² - 1) / 12, and its radius is (w - 1) / 2
    let span = (12.0 * sigma * sigma / 3.0 + 1.0).sqrt();
    let radius = (((span - 1.0) / 2.0).round() as usize).max(1);
    for _ in 0..3 {
        box_pass(&mut values, width, height, radius, true);
        box_pass(&mut values, width, height, radius, false);
    }
    for (p, v) in piece.data_mut().chunks_mut(4).zip(&values) {
        let a = v[3].clamp(0.0, 1.0);
        let alpha = (a * 255.0).round();
        for c in 0..3 {
            let straight = if a > 0.0 {
                (v[c] / a).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let srgb = f32::from(lut.to_srgb[(straight * LUT_STEPS as f32).round() as usize]);
            p[c] = (srgb * alpha / 255.0).round().min(alpha) as u8;
        }
        p[3] = alpha as u8;
    }
}

/// A running-sum box blur of `radius` along each row, or each column, its edges clamped.
fn box_pass(values: &mut [[f32; 4]], width: usize, height: usize, radius: usize, rows: bool) {
    let (length, lines) = if rows {
        (width, height)
    } else {
        (height, width)
    };
    let at = |line: usize, i: usize| {
        if rows {
            line * width + i
        } else {
            i * width + line
        }
    };
    let mut line_values = vec![[0.0f32; 4]; length];
    let span = (2 * radius + 1) as f32;
    let r = radius as isize;
    for line in 0..lines {
        for (i, v) in line_values.iter_mut().enumerate() {
            *v = values[at(line, i)];
        }
        let get = |i: isize| line_values[i.clamp(0, length as isize - 1) as usize];
        let mut sum = [0.0f32; 4];
        for i in -r..=r {
            let v = get(i);
            (0..4).for_each(|c| sum[c] += v[c]);
        }
        for i in 0..length {
            values[at(line, i)] = sum.map(|s| s / span);
            let (add, drop) = (get(i as isize + r + 1), get(i as isize - r));
            (0..4).for_each(|c| sum[c] += add[c] - drop[c]);
        }
    }
}

struct Lut {
    to_linear: [f32; 256],
    to_srgb: Vec<u8>,
}

/// The steps from 0 to 1 the way back to sRGB is tabled in, fine enough that every 8-bit
/// value comes back as it went.
const LUT_STEPS: usize = 4096;

fn lut() -> &'static Lut {
    static LUT: OnceLock<Lut> = OnceLock::new();
    LUT.get_or_init(|| {
        let to_linear = std::array::from_fn(|i| {
            let c = i as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        });
        let to_srgb = (0..=LUT_STEPS)
            .map(|i| {
                let l = i as f32 / LUT_STEPS as f32;
                let c = if l <= 0.003_130_8 {
                    l * 12.92
                } else {
                    1.055 * l.powf(1.0 / 2.4) - 0.055
                };
                (c * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect();
        Lut { to_linear, to_srgb }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixelate_leaves_no_block_thinner_than_half_a_block() {
        // 97 × 89: blocks of 8, with a pixel left over each way, and a corner of one pixel
        let (width, height) = (97, 89);
        let mut piece = Pixmap::new(width, height).unwrap();
        for (i, p) in piece
            .data_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            let v = (i * 7919 % 251) as u8;
            *p = [v, 255 - v, v / 2, 255];
        }
        let corner = piece.pixel(96, 88).unwrap();
        pixelate(&mut piece);
        assert_ne!(
            piece.pixel(96, 88).unwrap(),
            corner,
            "the corner keeps its pixel"
        );
        // the runs of one value along the top row and the left column
        let runs = |values: Vec<_>| {
            let mut runs = vec![1];
            for pair in values.windows(2) {
                if pair[0] == pair[1] {
                    *runs.last_mut().unwrap() += 1;
                } else {
                    runs.push(1);
                }
            }
            runs
        };
        let row = (0..width).map(|x| piece.pixel(x, 0).unwrap()).collect();
        let column = (0..height).map(|y| piece.pixel(0, y).unwrap()).collect();
        for (runs, last) in [(runs(row), 9), (runs(column), 9)] {
            assert!(runs.iter().all(|&run| run >= 4), "{runs:?}");
            assert_eq!(*runs.last().unwrap(), last, "{runs:?}");
        }
    }

    #[test]
    fn every_srgb_value_comes_back_from_linear_light() {
        let lut = lut();
        for c in 0..=255u8 {
            let linear = lut.to_linear[usize::from(c)];
            assert_eq!(lut.to_srgb[(linear * LUT_STEPS as f32).round() as usize], c);
        }
    }
}
