//! Mip pyramids: an image with its levels, each half the one before, which a redraw scales
//! down from instead of the full image.

use crate::Image;

/// An image with its pyramid.
#[derive(Debug)]
pub struct Pyramid {
    /// The decoded image first, then each level ⌈w/2⌉ × ⌈h/2⌉ of the one before, down to
    /// 1 × 1. A level's pixels are the alpha-weighted averages of 2 × 2 blocks of the one
    /// before, with straight alpha as in [`Image::rgba`].
    pub levels: Vec<Image>,
    /// Every alpha of the decoded image is 255, and so of every level.
    pub opaque: bool,
}

impl Pyramid {
    /// Builds the levels below `image`, in [`crate::memory_needed`] bytes with it.
    pub(crate) fn new(image: Image) -> Pyramid {
        let opaque = image.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
        let mut levels = vec![image];
        for (width, height) in smaller(levels[0].width, levels[0].height) {
            let level = half(&levels[levels.len() - 1], width, height, opaque);
            levels.push(level);
        }
        Pyramid { levels, opaque }
    }
}

/// The sizes of the levels below an image of this size, each half the one before, rounded
/// up, down to 1 × 1. An empty image has none.
pub(crate) fn smaller(width: u32, height: u32) -> impl Iterator<Item = (u32, u32)> {
    std::iter::successors(Some((width, height)), |&(w, h)| {
        (w.min(h) > 0 && w.max(h) > 1).then(|| (w.div_ceil(2), h.div_ceil(2)))
    })
    .skip(1)
}

// The level of `width` × `height` below `above`. An odd edge repeats its last row or column,
// which averages to the mean of the 2 × 1, 1 × 2 or 1 × 1 block it has.
fn half(above: &Image, width: u32, height: u32, opaque: bool) -> Image {
    let (width, row) = (width as usize, above.width as usize * 4);
    let mut rgba = vec![0; width * height as usize * 4];
    for (out, pair) in rgba
        .chunks_exact_mut(width * 4)
        .zip(above.rgba.chunks(row * 2))
    {
        let (top, bottom) = pair.split_at(row);
        let bottom = if bottom.is_empty() { top } else { bottom };
        let (top, top_last) = top.as_chunks::<4>().0.as_chunks::<2>();
        let (bottom, bottom_last) = bottom.as_chunks::<4>().0.as_chunks::<2>();
        let (out, _) = out.as_chunks_mut::<4>();
        for ((pixel, &[a, b]), &[c, d]) in out.iter_mut().zip(top).zip(bottom) {
            *pixel = average([a, b, c, d], opaque);
        }
        if let ([a], [c]) = (top_last, bottom_last) {
            out[width - 1] = average([*a, *a, *c, *c], opaque);
        }
    }
    Image {
        width: width as u32,
        height,
        rgba,
        may_animate: false,
    }
}

// The mean of four pixels, rounded, each color weighted by its alpha, so a transparent
// pixel lends none; opaque pixels need no weights.
fn average(pixels: [[u8; 4]; 4], opaque: bool) -> [u8; 4] {
    let sum = |channel: usize| pixels.iter().map(|p| u32::from(p[channel])).sum::<u32>();
    if opaque {
        return [0, 1, 2, 3].map(|channel| ((sum(channel) + 2) / 4) as u8);
    }
    let alpha = sum(3);
    if alpha == 0 {
        return [0; 4];
    }
    let color = |channel: usize| {
        let weighted = pixels
            .iter()
            .map(|p| u32::from(p[channel]) * u32::from(p[3]))
            .sum::<u32>();
        ((weighted + alpha / 2) / alpha) as u8
    };
    [color(0), color(1), color(2), ((alpha + 2) / 4) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, pixels: &[[u8; 4]]) -> Image {
        assert_eq!(pixels.len(), (width * height) as usize);
        Image {
            width,
            height,
            rgba: pixels.concat(),
            may_animate: false,
        }
    }

    fn sizes(pyramid: &Pyramid) -> Vec<(u32, u32)> {
        pyramid.levels.iter().map(|l| (l.width, l.height)).collect()
    }

    #[test]
    fn odd_sizes_halve_rounding_up_and_average_what_their_edge_blocks_hold() {
        // gray levels 0 to 14, opaque, in 5 x 3
        let gray: Vec<[u8; 4]> = (0..15).map(|v| [v * 10, v * 10, v * 10, 255]).collect();
        let pyramid = Pyramid::new(image(5, 3, &gray));
        assert!(pyramid.opaque);
        assert_eq!(sizes(&pyramid), [(5, 3), (3, 2), (2, 1), (1, 1)]);
        let level: Vec<u8> = pyramid.levels[1]
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[0])
            .collect();
        // 2 x 2 blocks, a 1 x 2 block at the right edge, 2 x 1 at the bottom and 1 x 1 in its
        // corner
        assert_eq!(level, [30, 50, 65, 105, 125, 140]);
        assert!(
            pyramid.levels[1]
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[3] == 255)
        );
        assert_eq!(pyramid.levels[3].rgba, [91, 91, 91, 255]);
    }

    #[test]
    fn a_line_halves_along_its_length_only() {
        let column: Vec<[u8; 4]> = (0..5).map(|v| [v * 50, 0, 0, 255]).collect();
        let pyramid = Pyramid::new(image(1, 5, &column));
        assert_eq!(sizes(&pyramid), [(1, 5), (1, 3), (1, 2), (1, 1)]);
        assert_eq!(
            pyramid.levels[1].rgba,
            [25, 0, 0, 255, 125, 0, 0, 255, 200, 0, 0, 255]
        );
        let row = Pyramid::new(image(5, 1, &column));
        assert_eq!(sizes(&row), [(5, 1), (3, 1), (2, 1), (1, 1)]);
        assert_eq!(row.levels[1].rgba, pyramid.levels[1].rgba);
        let dot = Pyramid::new(image(1, 1, &[[1, 2, 3, 4]]));
        assert_eq!((dot.levels.len(), dot.opaque), (1, false));
        assert_eq!(
            Pyramid::new(image(0, 3, &[])).levels.len(),
            1,
            "an empty image"
        );
    }

    #[test]
    fn transparent_pixels_lend_no_color() {
        // opaque red beside transparent green, and quarter-alpha blue beside three-quarter red
        let pixels = [
            [255, 0, 0, 255],
            [0, 255, 0, 0],
            [0, 0, 255, 64],
            [255, 0, 0, 192],
        ];
        let pyramid = Pyramid::new(image(2, 2, &pixels[..2].repeat(2)));
        assert!(!pyramid.opaque);
        assert_eq!(pyramid.levels[1].rgba, [255, 0, 0, 128]);
        let pyramid = Pyramid::new(image(2, 1, &pixels[2..]));
        assert_eq!(pyramid.levels[1].rgba, [191, 0, 64, 128]);
        let clear = Pyramid::new(image(2, 1, &[[9, 9, 9, 0]; 2]));
        assert_eq!(clear.levels[1].rgba, [0; 4]);
    }

    #[test]
    fn the_opaque_flag_skips_weights_that_change_nothing() {
        // the same opaque pixels, averaged with and without weights
        let pixels: Vec<[u8; 4]> = (0..35u32)
            .map(|i| {
                [
                    (i * 37 % 256) as u8,
                    (i * 91 % 256) as u8,
                    (i * 13 % 256) as u8,
                    255,
                ]
            })
            .collect();
        let opaque = Pyramid::new(image(7, 5, &pixels));
        assert!(opaque.opaque);
        let mut weighted = vec![image(7, 5, &pixels)];
        for (width, height) in smaller(7, 5) {
            let level = half(&weighted[weighted.len() - 1], width, height, false);
            weighted.push(level);
        }
        assert_eq!(opaque.levels, weighted);
        let mut one_clear = pixels.clone();
        one_clear[34][3] = 254;
        assert!(!Pyramid::new(image(7, 5, &one_clear)).opaque);
    }

    #[test]
    fn memory_needed_counts_the_bytes_the_levels_take() {
        for (width, height) in [(1, 1), (1, 9), (9, 1), (5, 3), (64, 48), (1, 1000)] {
            let pixels = vec![[0, 0, 0, 255]; (width * height) as usize];
            let pyramid = Pyramid::new(image(width, height, &pixels));
            let bytes: usize = pyramid.levels.iter().map(|l| l.rgba.capacity()).sum();
            assert_eq!(
                bytes as u64,
                crate::memory_needed(width, height),
                "{width}x{height}"
            );
        }
        // a line's levels add as much again as itself, less one pixel; a square's a third
        assert_eq!(crate::memory_needed(1, 1024), 2047 * 4);
        assert_eq!(crate::memory_needed(1024, 1024), 1_398_101 * 4);
    }
}
