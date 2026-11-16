//! Frames of canvas pixels in the forms a terminal shows: PNG, sixels and half-block cells.

use std::fmt::Write;

use icy_sixel::{EncodeOptions, SixelImage};
use nsxiv_ng_core::Size;
use nsxiv_ng_core::viewport::Viewport;

use super::kitty::pixel_file;

/// The pixels a frame of `view` takes on a canvas of `canvas` units, a unit being a sixteenth
/// of a cell's height, and the scale it is drawn at: the terminal's own pixels for a kitty or
/// sixel image, 16 a row for half-block cells, which average them down. A kitty terminal scales
/// an image up as Imlib2 does, linearly, so an `image` shown larger than its pixels goes at
/// those, which the terminal scales itself; not over the checkerboard, whose squares would grow
/// with it.
pub fn frame_pixels(
    view: &Viewport,
    image: bool,
    kitty: bool,
    sixel: bool,
    cell_height: f64,
    canvas: Size,
) -> (u32, u32, f64) {
    let mut scale = if kitty || sixel {
        cell_height / 16.0
    } else {
        1.0
    };
    if kitty && image && view.antialias && !view.alpha_layer && view.zoom() * scale > 1.0 {
        scale = 1.0 / view.zoom();
    }
    let pixels = |units: f64| ((units * scale).round() as u32).max(1);
    (pixels(canvas.width), pixels(canvas.height), scale)
}

/// A premultiplied `0xAARRGGBB` pixel as straight RGBA, alpha 0 giving a clear pixel.
pub(super) fn straight(pixel: u32) -> [u8; 4] {
    let a = pixel >> 24;
    let channel = |shift: u32| {
        let c = (pixel >> shift) & 0xff;
        match a {
            0 => 0,
            255 => c as u8,
            _ => ((c * 255 + a / 2) / a).min(255) as u8,
        }
    };
    [channel(16), channel(8), channel(0), a as u8]
}

/// A frame of `width × height` pixels as a PNG, 8-bit RGB, or RGBA with `alpha`, at the png
/// crate's fastest level, the Up filter on every row; None when the frame is not that size or
/// has none.
pub fn png(frame: &[u32], width: u32, height: u32, alpha: bool) -> Option<Vec<u8>> {
    if frame.len() as u64 != u64::from(width) * u64::from(height) {
        return None;
    }
    let mut out = Vec::new();
    let mut encoder = ::png::Encoder::new(&mut out, width, height);
    encoder.set_color(if alpha {
        ::png::ColorType::Rgba
    } else {
        ::png::ColorType::Rgb
    });
    encoder.set_depth(::png::BitDepth::Eight);
    encoder.set_compression(::png::Compression::Fastest);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&pixel_file(frame, alpha)).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// A frame of `width × height` pixels as a sixel image of exactly that size, its raster
/// attributes saying so, in at most 256 colors and undithered, which takes fewer bytes and
/// keeps closer to the image. A pixel with alpha under 128 is left undrawn, so the terminal's
/// background shows; any other is drawn opaque, in its unpremultiplied color. Sixels go in
/// bands of 6 rows, and a terminal may count a last band cut short as whole, its rows past the
/// frame undrawn, so a frame that must not reach a row below has a height divisible by 6.
/// None when the frame is not that size or has none.
pub fn sixels(frame: &[u32], width: u32, height: u32) -> Option<String> {
    let image =
        SixelImage::try_from_rgba(pixel_file(frame, true), width as usize, height as usize).ok()?;
    let options = EncodeOptions {
        diffusion: 0.0,
        ..EncodeOptions::default()
    };
    let mut sixels = image.encode_with(&options).ok()?;
    // icy_sixel ends the last band with a graphics newline, which would move the cursor a band
    // past the image
    if sixels.ends_with("-\x1b\\") {
        sixels.remove(sixels.len() - 3);
    }
    Some(sixels)
}

/// The part of a sixel frame that goes to the terminal: `width × height` pixels from `x, y`,
/// written from the cell whose corner is at or before `x, y`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SixelCut {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The part of a `width × height` frame, of cells `cell_height` pixels high, to send as
/// sixels: of the image view, with `image`, the bounds of the pixels a sixel image draws,
/// alpha 128 or more, so an opaque picture goes exactly as drawn and the encoder gets no
/// clear pixel, which would take it down its far slower path, and one with clear pixels of
/// its own keeps all it shows; else the whole frame. Written from the cell at or before its
/// corner, the picture moves left and up by less than a cell, never past the frame's edges.
pub fn sixel_cut(
    frame: &[u32],
    (width, height): (u32, u32),
    image: bool,
    cell_height: u32,
) -> SixelCut {
    let whole = SixelCut {
        x: 0,
        y: 0,
        width,
        height,
    };
    let size = width as usize * height as usize;
    if !image || width == 0 || frame.len() != size || cell_height == 0 {
        return whole;
    }
    // each row from its ends inward, so a row of the picture costs its margins alone
    let drawn = |pixel: &u32| pixel >> 24 >= 128;
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    for (y, row) in (0..).zip(frame.chunks_exact(width as usize)) {
        let Some(first) = row.iter().position(drawn) else {
            continue;
        };
        let last = row.iter().rposition(drawn).unwrap_or(first);
        (left, right) = (left.min(first as u32), right.max(last as u32 + 1));
        (top, bottom) = (top.min(y), y + 1);
    }
    if right <= left || bottom <= top {
        return whole;
    }
    // a picture with a margin on a side goes there by whole cells, as it is written from a
    // cell; one that runs past the edge by the pixel, its corner on the cell already; a pan
    // step, a fifth of the view, hides the difference
    let written_top = top / cell_height * cell_height;
    // a terminal may count a last band cut short as whole, and it must end within the frame,
    // whose height is whole bands short of the bar's row: of a picture that runs to the
    // frame's bottom, its last rows go, at most 5; one that ends above would lose rows it
    // shows, so the whole frame goes
    let bands = top + (height - written_top) / 6 * 6;
    if bottom > bands {
        if bottom < height || bands <= top {
            return whole;
        }
        bottom = bands;
    }
    SixelCut {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// The pixels of `cut` out of a frame `width` pixels wide; None where the frame does not
/// hold them.
pub fn cut_pixels(frame: &[u32], width: u32, cut: &SixelCut) -> Option<Vec<u32>> {
    let (x, y, w, h) = (
        cut.x as usize,
        cut.y as usize,
        cut.width as usize,
        cut.height as usize,
    );
    if x + w > width as usize || (y + h) * width as usize > frame.len() {
        return None;
    }
    let mut pixels = Vec::with_capacity(cut.width as usize * cut.height as usize);
    for y in cut.y..cut.y + cut.height {
        let start = (y * width + cut.x) as usize;
        pixels.extend_from_slice(&frame[start..start + cut.width as usize]);
    }
    Some(pixels)
}

/// The `cut` of a frame `width` pixels wide as sixels, after the move of the cursor to its
/// corner, of cells of `cell` pixels; None where the frame does not hold the cut.
pub fn placed_sixels(
    frame: &[u32],
    width: u32,
    cut: &SixelCut,
    cell: (u32, u32),
) -> Option<String> {
    let (column, row) = (cut.x / cell.0.max(1) + 1, cut.y / cell.1.max(1) + 1);
    let sixels = sixels(&cut_pixels(frame, width, cut)?, cut.width, cut.height)?;
    Some(format!("\x1b[{row};{column}H{sixels}"))
}

/// The colors a terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit color.
    True,
    /// The 256-color palette, of which the 6×6×6 cube and the gray ramp, whose colors are fixed.
    Colors256,
    /// The 16 ANSI colors, as xterm shows them by default.
    Colors16,
}

/// The colors a terminal shows, from its `COLORTERM` and `TERM` variables.
pub fn color_depth(colorterm: Option<&str>, term: Option<&str>) -> ColorDepth {
    let term = term.unwrap_or_default();
    if matches!(colorterm, Some("truecolor" | "24bit")) || term.ends_with("-direct") {
        ColorDepth::True
    } else if term.contains("256color") {
        ColorDepth::Colors256
    } else {
        ColorDepth::Colors16
    }
}

/// A frame of `width × height` pixels as `columns × rows` cells of half blocks: each cell is
/// `▀`, its upper half in the foreground and its lower half in the background, and each half
/// the average of the frame's pixels in its box. The frame is expected at the size
/// `frame_pixels` gives, which keeps the image's shape on cells of any aspect. A half whose
/// average alpha is under 128 is clear, showing the terminal's default background, as `▄` or a
/// space shows it; any other is opaque, in its unpremultiplied color. The cells start at the
/// cursor, which is in column 1, rows separated by CR LF, and the colors end reset. Empty when
/// the frame is not that size, or there is nothing to fill.
pub fn half_blocks(
    frame: &[u32],
    width: u32,
    height: u32,
    columns: usize,
    rows: usize,
    depth: ColorDepth,
) -> String {
    let (width, height) = (width as usize, height as usize);
    let mut out = String::new();
    if frame.len() != width * height || frame.is_empty() || columns == 0 || rows == 0 {
        return out;
    }
    // the pixels of part i of n along an extent, at least one
    let span = |i: usize, n: usize, extent: usize| {
        let start = i * extent / n;
        start..((i + 1) * extent / n).max(start + 1)
    };
    let half = |column: usize, half_row: usize| {
        let xs = span(column, columns, width);
        let ys = span(half_row, 2 * rows, height);
        let mut sums = [0u64; 4];
        for y in ys.clone() {
            for &pixel in &frame[y * width..][xs.clone()] {
                for (i, sum) in sums.iter_mut().enumerate() {
                    *sum += u64::from((pixel >> (24 - 8 * i)) & 0xff);
                }
            }
        }
        let n = (xs.len() * ys.len()) as u64;
        let [a, r, g, b] = sums.map(|sum| ((sum + n / 2) / n) as u32);
        let [r, g, b, a] = straight(a << 24 | r << 16 | g << 8 | b);
        (a >= 128).then(|| ink([r, g, b], depth))
    };
    // the colors set so far, None before the first cell
    let (mut fg, mut bg) = (None, None);
    for row in 0..rows {
        if row > 0 {
            out.push_str("\r\n");
        }
        for column in 0..columns {
            let (upper, lower) = (half(column, 2 * row), half(column, 2 * row + 1));
            let (glyph, new_fg, new_bg) = match (upper, lower) {
                (Some(upper), lower) => ('▀', Some(upper), lower.unwrap_or(Ink::Default)),
                (None, Some(lower)) => ('▄', Some(lower), Ink::Default),
                (None, None) => (' ', None, Ink::Default),
            };
            let mut codes = String::new();
            if let Some(new_fg) = new_fg.filter(|&ink| fg != Some(ink)) {
                new_fg.push_sgr(&mut codes, false);
                fg = Some(new_fg);
            }
            if bg != Some(new_bg) {
                new_bg.push_sgr(&mut codes, true);
                bg = Some(new_bg);
            }
            if !codes.is_empty() {
                codes.pop(); // the last separator
                let _ = write!(out, "\x1b[{codes}m");
            }
            out.push(glyph);
        }
    }
    out.push_str("\x1b[0m");
    out
}

// a color as one of a terminal's
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ink {
    Default,
    Rgb([u8; 3]),
    Palette(u8),
    Ansi(u8),
}

impl Ink {
    // the SGR parameters that set it, each followed by ;
    fn push_sgr(self, codes: &mut String, background: bool) {
        let (base, bright, extended): (u8, u8, u8) = if background {
            (40, 100, 48)
        } else {
            (30, 90, 38)
        };
        let _ = match self {
            Ink::Default => write!(codes, "{};", base + 9),
            Ink::Rgb([r, g, b]) => write!(codes, "{extended};2;{r};{g};{b};"),
            Ink::Palette(n) => write!(codes, "{extended};5;{n};"),
            Ink::Ansi(n) if n < 8 => write!(codes, "{};", base + n),
            Ink::Ansi(n) => write!(codes, "{};", bright + n - 8),
        };
    }
}

// xterm's default 16 colors
const ANSI: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 0, 0],
    [0, 205, 0],
    [205, 205, 0],
    [0, 0, 238],
    [205, 0, 205],
    [0, 205, 205],
    [229, 229, 229],
    [127, 127, 127],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [92, 92, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];

// the nearest of the colors at depth
fn ink(color: [u8; 3], depth: ColorDepth) -> Ink {
    let distance = |other: [u8; 3]| {
        (0..3)
            .map(|i| (i32::from(color[i]) - i32::from(other[i])).pow(2))
            .sum::<i32>()
    };
    match depth {
        ColorDepth::True => Ink::Rgb(color),
        ColorDepth::Colors256 => {
            // the cube's levels are 0, 95, 135, 175, 215 and 255; the grays 8 to 238 by 10
            let step = |v: u8| match v {
                0..48 => 0,
                48..115 => 1,
                _ => (v - 35) / 40,
            };
            let level = |step: u8| if step == 0 { 0 } else { 55 + 40 * step };
            let cube = color.map(step);
            let average = color.iter().map(|&v| u32::from(v)).sum::<u32>() / 3;
            let gray = (average.saturating_sub(3) / 10).min(23) as u8;
            let gray_value = 8 + 10 * gray;
            if distance([gray_value; 3]) < distance(cube.map(level)) {
                Ink::Palette(232 + gray)
            } else {
                Ink::Palette(16 + 36 * cube[0] + 6 * cube[1] + cube[2])
            }
        }
        ColorDepth::Colors16 => {
            let nearest = (0..16).min_by_key(|&i| distance(ANSI[i])).unwrap_or(0);
            Ink::Ansi(nearest as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A frame of `width × height` with an opaque picture over [x0, x1) × [y0, y1).
    fn framed(width: u32, height: u32, (x0, y0, x1, y1): (u32, u32, u32, u32)) -> Vec<u32> {
        let mut frame = vec![0; width as usize * height as usize];
        for y in y0..y1 {
            for x in x0..x1 {
                frame[(y * width + x) as usize] = 0xff11_2233;
            }
        }
        frame
    }

    #[test]
    fn a_sixel_cut_holds_the_whole_picture_written_from_the_cell_at_or_before_its_corner() {
        let cell = (10, 20);
        let place = |cut: &SixelCut| (cut.x / cell.0 * cell.0, cut.y / cell.1 * cell.1);
        // filling the frame, or past every side, it is the frame and does not move
        let frame = framed(200, 96, (0, 0, 200, 96));
        let whole = SixelCut {
            x: 0,
            y: 0,
            width: 200,
            height: 96,
        };
        let cut = sixel_cut(&frame, (200, 96), true, cell.1);
        assert_eq!(cut, whole);
        let frame = framed(200, 96, (20, 10, 120, 70));
        assert_eq!(
            sixel_cut(&frame, (200, 96), false, cell.1),
            whole,
            "not of the image view"
        );
        let clear = vec![0; 200 * 96];
        assert_eq!(
            sixel_cut(&clear, (200, 96), true, cell.1),
            whole,
            "nothing drawn"
        );
        // at the left or the top edge, not moved either
        let frame = framed(200, 96, (0, 33, 150, 70));
        let cut = sixel_cut(&frame, (200, 96), true, cell.1);
        assert_eq!((cut.x, place(&cut).0), (0, 0));
        let frame = framed(200, 96, (57, 0, 150, 70));
        let cut = sixel_cut(&frame, (200, 96), true, cell.1);
        assert_eq!((cut.y, place(&cut).1), (0, 0));
        // a slow pan to the right, a pixel a frame, across the cell corner at 20: the cut is
        // the picture, whose place steps from the corner at 10 to the one at 20
        for (edge, at) in [(17, 10), (18, 10), (19, 10), (20, 20), (21, 20), (22, 20)] {
            let frame = framed(200, 96, (edge, 25, edge + 100, 75));
            let cut = sixel_cut(&frame, (200, 96), true, cell.1);
            let expected = SixelCut {
                x: edge,
                y: 25,
                width: 100,
                height: 50,
            };
            assert_eq!(cut, expected, "edge at {edge}");
            assert_eq!(place(&cut), (at, 20), "edge at {edge}");
        }
        // pixels under 128 of alpha are undrawn, so they do not count; 128 and up do
        let mut frame = framed(200, 96, (30, 30, 130, 60));
        frame[29 * 200 + 50] = 0x7f00_0000;
        frame[60 * 200 + 50] = 0x8000_0000;
        let cut = sixel_cut(&frame, (200, 96), true, cell.1);
        assert_eq!((cut.y, cut.y + cut.height), (30, 61));
    }

    #[test]
    fn a_picture_ending_inside_the_frame_that_a_last_band_would_cut_goes_with_the_frame() {
        // cells of 2 rows: from 2, a picture of 92 rows would need bands to 98 of 96
        let frame = framed(200, 96, (50, 2, 150, 94));
        let whole = SixelCut {
            x: 0,
            y: 0,
            width: 200,
            height: 96,
        };
        assert_eq!(sixel_cut(&frame, (200, 96), true, 2), whole);
    }

    #[test]
    fn a_sixel_cuts_last_band_ends_within_the_frame() {
        let cell = (10, 20);
        // from row 15, written from 0, down to the frame's last row: bands to 84, within 96
        let frame = framed(200, 96, (0, 15, 200, 96));
        let cut = sixel_cut(&frame, (200, 96), true, cell.1);
        assert_eq!((cut.y, cut.height), (15, 81));
        // from row 23, written from 20, a last band cut short would reach 98: its last row goes
        let frame = framed(200, 96, (0, 23, 200, 96));
        let cut = sixel_cut(&frame, (200, 96), true, cell.1);
        assert_eq!((cut.y, cut.height), (23, 72));
        assert!(20 + cut.height.div_ceil(6) * 6 <= 96);
    }

    #[test]
    fn placed_sixels_of_a_frame_shorter_than_its_size_are_none() {
        let frame = framed(200, 96, (20, 40, 120, 90));
        let cut = SixelCut {
            x: 20,
            y: 40,
            width: 100,
            height: 50,
        };
        assert!(placed_sixels(&frame, 200, &cut, (10, 20)).is_some());
        assert_eq!(placed_sixels(&frame[..200 * 80], 200, &cut, (10, 20)), None);
        let past = SixelCut { x: 150, ..cut };
        assert_eq!(placed_sixels(&frame, 200, &past, (10, 20)), None);
    }

    // The cut's edges: left, top, right and bottom.
    fn edges(cut: &SixelCut) -> (u32, u32, u32, u32) {
        (cut.x, cut.y, cut.x + cut.width, cut.y + cut.height)
    }

    #[test]
    fn a_picture_of_one_pixel_is_cut_to_it_and_written_from_its_cell() {
        let frame = framed(200, 96, (57, 33, 58, 34));
        let cut = sixel_cut(&frame, (200, 96), true, 20);
        assert_eq!(edges(&cut), (57, 33, 58, 34));
        let sixels = placed_sixels(&frame, 200, &cut, (10, 20)).unwrap();
        assert!(sixels.starts_with("\x1b[2;6H\x1bP"), "{sixels:?}");
        assert!(sixels.contains("\"1;1;1;1#"), "{sixels:?}");
    }

    #[test]
    fn clear_pixels_inside_a_picture_or_at_one_corner_leave_its_bounds() {
        // a clear row and a clear column through its middle
        let mut frame = framed(200, 96, (20, 10, 120, 70));
        for x in 20..120 {
            frame[40 * 200 + x] = 0;
        }
        for y in 10..70 {
            frame[y * 200 + 70] = 0;
        }
        let cut = sixel_cut(&frame, (200, 96), true, 20);
        assert_eq!(edges(&cut), (20, 10, 120, 70));
        // clear pixels at its top left corner only
        let mut frame = framed(200, 96, (20, 10, 120, 70));
        for (x, y) in [(20, 10), (21, 10), (20, 11), (21, 11), (22, 10), (20, 12)] {
            frame[y * 200 + x] = 0;
        }
        let cut = sixel_cut(&frame, (200, 96), true, 20);
        assert_eq!(edges(&cut), (20, 10, 120, 70));
    }

    #[test]
    fn a_frame_of_no_width_or_no_height_is_cut_whole_and_gives_no_sixels() {
        for (width, height) in [(0, 96), (200, 0)] {
            let cut = sixel_cut(&[], (width, height), true, 20);
            let whole = SixelCut {
                x: 0,
                y: 0,
                width,
                height,
            };
            assert_eq!(cut, whole);
            assert_eq!(placed_sixels(&[], width, &cut, (10, 20)), None);
        }
    }

    #[test]
    fn cells_of_1x2_and_30x60_place_the_cursor_and_keep_the_bands_within_the_frame() {
        // cells of 1x2: from 3,5, written from the cell at 3,4
        let frame = framed(200, 96, (3, 5, 150, 90));
        let cut = sixel_cut(&frame, (200, 96), true, 2);
        assert_eq!(edges(&cut), (3, 5, 150, 90));
        let sixels = placed_sixels(&frame, 200, &cut, (1, 2)).unwrap();
        assert!(sixels.starts_with("\x1b[3;4H\x1bP"), "{sixels:?}");
        assert!(sixels.contains("\"1;1;147;85#"), "{sixels:?}");
        assert!(4 + cut.height.div_ceil(6) * 6 <= 96);
        // down to the frame's last row, that row goes: bands from 4 then end at 94, not at 100
        let frame = framed(200, 96, (3, 5, 150, 96));
        let cut = sixel_cut(&frame, (200, 96), true, 2);
        assert_eq!(edges(&cut), (3, 5, 150, 95));
        // cells of 30x60, in a frame 354 high, whole bands: from 95,130, written from 90,120
        let frame = framed(600, 354, (95, 130, 500, 300));
        let cut = sixel_cut(&frame, (600, 354), true, 60);
        assert_eq!(edges(&cut), (95, 130, 500, 300));
        let sixels = placed_sixels(&frame, 600, &cut, (30, 60)).unwrap();
        assert!(sixels.starts_with("\x1b[3;4H\x1bP"), "{sixels:?}");
        assert!(sixels.contains("\"1;1;405;170#"), "{sixels:?}");
        assert!(120 + cut.height.div_ceil(6) * 6 <= 354);
    }

    #[test]
    fn placed_sixels_move_the_cursor_to_the_cuts_cell() {
        let frame = framed(200, 96, (27, 45, 127, 90));
        let cut = sixel_cut(&frame, (200, 96), true, 20);
        let sixels = placed_sixels(&frame, 200, &cut, (10, 20)).unwrap();
        assert!(sixels.starts_with("\x1b[3;3H\x1bP"), "{sixels:?}");
        assert!(sixels.contains("\"1;1;100;45#"), "{sixels:?}");
    }

    #[test]
    fn a_frame_takes_the_terminals_pixels_or_an_enlarged_images_own() {
        let canvas = Size {
            width: 800.0,
            height: 464.0,
        };
        let mut view = Viewport::default();
        view.set_view_size(canvas);
        view.set_image_size(
            Size {
                width: 200.0,
                height: 100.0,
            },
            true,
        );
        view.set_zoom(2.0, None);
        let pixels = |view: &Viewport, image, kitty, sixel| {
            frame_pixels(view, image, kitty, sixel, 20.0, canvas)
        };
        // at 200% on cells of 20 pixels, the terminal scales the image's own pixels up
        assert_eq!(pixels(&view, true, true, false), (400, 232, 0.5));
        // the grid, sixels and half-block cells go at their pixels
        assert_eq!(pixels(&view, false, true, false), (1000, 580, 1.25));
        assert_eq!(pixels(&view, true, false, true), (1000, 580, 1.25));
        assert_eq!(pixels(&view, true, false, false), (800, 464, 1.0));
        // so does an image over the checkerboard, one without antialiasing, and one shown smaller
        let mut checkered = view.clone();
        checkered.alpha_layer = true;
        assert_eq!(pixels(&checkered, true, true, false), (1000, 580, 1.25));
        let mut sharp = view.clone();
        sharp.antialias = false;
        assert_eq!(pixels(&sharp, true, true, false), (1000, 580, 1.25));
        view.set_zoom(0.5, None);
        assert_eq!(pixels(&view, true, true, false), (1000, 580, 1.25));
        // a frame has at least a pixel
        let tiny = Size {
            width: 0.1,
            height: 0.0,
        };
        assert_eq!(
            frame_pixels(&view, true, false, false, 20.0, tiny),
            (1, 1, 1.0)
        );
    }

    // a PNG's color type, size and pixels, through the png crate's decoder
    fn decoded(png: &[u8]) -> (::png::ColorType, u32, u32, Vec<u8>) {
        let mut reader = ::png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(info.buffer_size());
        (info.color_type, info.width, info.height, pixels)
    }

    #[test]
    fn a_png_frame_keeps_its_pixels() {
        let colors = [
            0xffff_0000,
            0xff00_ff00,
            0xff00_00ff,
            0xff12_3456,
            0xff00_0000,
            0xffff_ffff,
        ];
        let rgb: Vec<u8> = colors
            .iter()
            .flat_map(|&c: &u32| [(c >> 16) as u8, (c >> 8) as u8, c as u8])
            .collect();
        assert_eq!(
            decoded(&png(&colors, 3, 2, false).unwrap()),
            (::png::ColorType::Rgb, 3, 2, rgb)
        );
        assert_eq!(png(&colors, 3, 3, false), None, "a frame of another size");
        assert_eq!(png(&[], 0, 0, false), None, "a frame of no pixels");
    }

    #[test]
    fn a_png_frame_with_alpha_keeps_its_pixels_and_their_alpha_straight() {
        // clear pixels among them, and half a green, premultiplied
        let colors = [0, 0xffff_0000, 0x8000_8000, 0xff00_00ff, 0, 0xff12_3456];
        let rgba = vec![
            0, 0, 0, 0, 255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 0, 0, 0, 0, 0x12, 0x34,
            0x56, 255,
        ];
        assert_eq!(
            decoded(&png(&colors, 3, 2, true).unwrap()),
            (::png::ColorType::Rgba, 3, 2, rgba)
        );
    }

    #[test]
    fn a_sixel_image_has_the_exact_size_and_leaves_clear_pixels_undrawn() {
        // 5x7, a band and a row: red, but for a clear pixel, one of alpha 127, which is clear
        // too, and one of half a red, which is drawn red; the last row blue
        let (red, blue) = (0xffff_0000, 0xff00_00ff);
        let mut frame = vec![red; 35];
        frame[..3].copy_from_slice(&[0, 0x7f7f_0000, 0x8080_0000]);
        frame[30..].fill(blue);
        let sent = sixels(&frame, 5, 7).unwrap();
        assert!(sent.starts_with("\x1bP9;1;0q\"1;1;5;7#"), "{sent:?}");
        assert!(sent.ends_with("\x1b\\"));
        assert!(!sent[..sent.len() - 2].ends_with('-'), "{sent:?}");
        // a decoder may take the last band whole, but its rows past the frame stay undrawn
        let image = SixelImage::decode(sent.as_bytes()).unwrap();
        assert_eq!((image.width, image.height), (5, 12));
        assert!(image.pixels[4 * 35..].chunks(4).all(|p| p[3] == 0));
        let pixel = |i: usize| &image.pixels[4 * i..4 * i + 4];
        assert_eq!(pixel(0)[3], 0);
        assert_eq!(pixel(1)[3], 0);
        for (i, color) in [
            (2, [255, 0, 0, 255]),
            (29, [255, 0, 0, 255]),
            (34, [0, 0, 255, 255]),
        ] {
            let off = pixel(i).iter().zip(color).map(|(&a, b)| a.abs_diff(b));
            assert!(off.max() <= Some(3), "pixel {i}: {:?}", pixel(i));
        }
        assert_eq!(sixels(&frame, 5, 6), None, "a frame of another size");
        assert_eq!(sixels(&[], 0, 0), None, "a frame of no pixels");
    }

    #[test]
    fn the_color_depth_comes_from_colorterm_and_term() {
        let depth = color_depth;
        assert_eq!(
            depth(Some("truecolor"), Some("xterm-256color")),
            ColorDepth::True
        );
        assert_eq!(depth(Some("24bit"), None), ColorDepth::True);
        assert_eq!(depth(None, Some("xterm-direct")), ColorDepth::True);
        assert_eq!(depth(None, Some("screen-256color")), ColorDepth::Colors256);
        assert_eq!(depth(Some(""), Some("xterm")), ColorDepth::Colors16);
        assert_eq!(depth(Some("yes"), Some("linux")), ColorDepth::Colors16);
        assert_eq!(depth(None, None), ColorDepth::Colors16);
    }

    #[test]
    fn half_blocks_show_clear_halves_in_the_terminals_background() {
        // upper and lower halves: red over blue, clear over blue, red over clear, clear over clear
        let (red, blue) = (0xffff_0000, 0xff00_00ff);
        let frame = [red, 0, red, 0, blue, blue, 0, 0];
        assert_eq!(
            half_blocks(&frame, 4, 2, 4, 1, ColorDepth::True),
            "\x1b[38;2;255;0;0;48;2;0;0;255m▀\x1b[38;2;0;0;255;49m▄\x1b[38;2;255;0;0m▀ \x1b[0m"
        );
    }

    #[test]
    fn half_blocks_average_their_boxes_in_each_color_depth() {
        // 2x2 boxes: white and black make gray; three red and a clear pixel make red that is
        // opaque, the clear one lending no color; one white of four is clear; four blue
        let (white, black, red, blue) = (0xffff_ffff, 0xff00_0000, 0xffff_0000, 0xff00_00ff);
        #[rustfmt::skip]
        let frame = [
            white, black, 0, 0,
            black, white, 0, white,
            red, red, blue, blue,
            red, 0, blue, blue,
        ];
        let cells = |depth| half_blocks(&frame, 4, 4, 2, 1, depth);
        assert_eq!(
            cells(ColorDepth::True),
            "\x1b[38;2;128;128;128;48;2;255;0;0m▀\x1b[38;2;0;0;255;49m▄\x1b[0m"
        );
        assert_eq!(
            cells(ColorDepth::Colors256),
            "\x1b[38;5;244;48;5;196m▀\x1b[38;5;21;49m▄\x1b[0m"
        );
        assert_eq!(
            cells(ColorDepth::Colors16),
            "\x1b[90;101m▀\x1b[34;49m▄\x1b[0m"
        );
    }

    #[test]
    fn half_blocks_fill_their_cells_row_by_row() {
        // a frame smaller than the cells stretches over them
        assert_eq!(
            half_blocks(&[0xffff_0000, 0xff00_00ff], 1, 2, 1, 2, ColorDepth::True),
            "\x1b[38;2;255;0;0;48;2;255;0;0m▀\r\n\x1b[38;2;0;0;255;48;2;0;0;255m▀\x1b[0m"
        );
        assert_eq!(half_blocks(&[], 0, 0, 3, 3, ColorDepth::True), "");
        assert_eq!(half_blocks(&[0; 3], 2, 2, 3, 3, ColorDepth::True), "");
    }

    #[test]
    fn the_256_and_16_colors_are_the_nearest() {
        assert_eq!(ink([0, 0, 0], ColorDepth::Colors256), Ink::Palette(16));
        assert_eq!(
            ink([255, 255, 255], ColorDepth::Colors256),
            Ink::Palette(231)
        );
        assert_eq!(
            ink([95, 135, 175], ColorDepth::Colors256),
            Ink::Palette(16 + 36 + 12 + 3)
        );
        assert_eq!(ink([8, 8, 8], ColorDepth::Colors256), Ink::Palette(232));
        assert_eq!(
            ink([238, 238, 238], ColorDepth::Colors256),
            Ink::Palette(255)
        );
        assert_eq!(ink([200, 10, 10], ColorDepth::Colors16), Ink::Ansi(1));
        assert_eq!(ink([250, 250, 250], ColorDepth::Colors16), Ink::Ansi(15));
    }

    #[test]
    fn pixels_unpremultiply_and_clear_ones_lose_their_color() {
        assert_eq!(straight(0xff12_3456), [0x12, 0x34, 0x56, 255]);
        assert_eq!(straight(0x8040_2000), [128, 64, 0, 128]);
        assert_eq!(straight(0x0012_3456), [0, 0, 0, 0]);
        // a color past its alpha, which no premultiplied pixel has, stays in range
        assert_eq!(straight(0x10ff_ffff), [255, 255, 255, 16]);
    }
}
