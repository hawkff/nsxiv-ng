//! The thumbnail grid: thumbnails fitted into their cells, with the selection and the marks.

use nsxiv_ng_core::viewer::Viewer;

use crate::canvas::{self, Canvas};
use crate::image_view::{self, Image, Turn};

/// The colors the grid draws besides its thumbnails, as canvas pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridColors {
    /// The square in a cell whose thumbnail is missing, in the bar's background color.
    pub placeholder: u32,
    pub selection: u32,
    pub mark: u32,
}

/// A thumbnail as it came out in its cell: the file's index, the size it was drawn at, and
/// its pixels, rows of `size.0`, which a later frame may copy in place of drawing it again
/// where the thumbnail, the size and the background under it are the same.
#[derive(Debug)]
pub struct Drawn {
    pub index: usize,
    pub size: (u32, u32),
    pub pixels: Vec<u32>,
}

/// Draws the cells of the files in view of the viewer's grid onto the canvas, which is the
/// grid's area at device `scale`: the thumbnail `thumbnail` gives for a file's index, fitted
/// into its padded cell and never enlarged, or the placeholder square when it gives none; the
/// selection border around the current file, and a dot on each marked one.
pub fn draw<'a>(
    canvas: &mut Canvas,
    viewer: &Viewer,
    scale: f64,
    colors: GridColors,
    thumbnail: impl FnMut(usize) -> Option<Image<'a>>,
) {
    draw_kept(
        canvas,
        viewer,
        scale,
        colors,
        thumbnail,
        |_, _| None,
        |_, _| false,
    );
}

/// [`draw`], copying a thumbnail from the pixels `kept` gives for its file's index and size
/// where it gives them, which must be those [`Drawn`] gave for the same thumbnail over the
/// same background. Gives the thumbnails it drew whole on the canvas where `room` says for
/// the index and the size that they may be kept, to keep.
pub fn draw_kept<'a>(
    canvas: &mut Canvas,
    viewer: &Viewer,
    scale: f64,
    colors: GridColors,
    mut thumbnail: impl FnMut(usize) -> Option<Image<'a>>,
    kept: impl Fn(usize, (u32, u32)) -> Option<&'a [u32]>,
    mut room: impl FnMut(usize, (u32, u32)) -> bool,
) -> Vec<Drawn> {
    let mut drawn = Vec::new();
    let grid = viewer.grid();
    let held = cells_hold_their_drawings(grid.cell_size() * scale, grid.thumb_size(), scale);
    for i in grid.visible() {
        let Some(entry) = viewer.files().get(i) else {
            continue;
        };
        let r = grid.cell_rect(i);
        let (x, y, size) = (r.x * scale, r.y * scale, r.width * scale);
        let pad = (grid.cell_size() - grid.thumb_size()) / 2.0 * scale;
        let inner = size - 2.0 * pad;
        // the sizes and corners truncate toward zero, as whole pixels do in the reference grid
        if let Some(thumb) = thumbnail(i) {
            let (tw, th) = (f64::from(thumb.width), f64::from(thumb.height));
            let fit = (inner / tw).min(inner / th).min(1.0);
            let (dw, dh) = (tw * fit, th * fit);
            let at = (
                (x + pad + (inner - dw) / 2.0) as i64,
                (y + pad + (inner - dh) / 2.0) as i64,
            );
            // a thin thumbnail keeps a line of a pixel, rather than vanishing
            let side = size;
            let size = ((dw as i64).max(1), (dh as i64).max(1));
            let cell = (size.0 as u32, size.1 as u32);
            // a thumbnail is kept, or copied, only where it lies a pixel inside its own cell's
            // pixels, which no other cell's thumbnail, placeholder, selection or mark reaches;
            // where cells are too small for that, it is drawn as it comes
            let inside = |at: i64, length: i64, from: f64| {
                at > from.ceil() as i64 && at + length < (from + side).floor() as i64
            };
            let alone = held && inside(at.0, size.0, x) && inside(at.1, size.1, y);
            let kept = alone
                .then(|| kept(i, cell))
                .flatten()
                .filter(|pixels| pixels.len() == (size.0 * size.1) as usize);
            match kept {
                Some(pixels) => copy(canvas, at, size, pixels),
                None => {
                    let region = (at.0, at.1, size.0, size.1);
                    image_view::blit(
                        canvas,
                        &thumb,
                        Turn::default(),
                        region,
                        (at, size),
                        true,
                        None,
                    );
                    let taken = (alone && room(i, cell)).then(|| taken(canvas, at, size));
                    if let Some(pixels) = taken.flatten() {
                        drawn.push(Drawn {
                            index: i,
                            size: cell,
                            pixels,
                        });
                    }
                }
            }
        } else {
            let quarter = inner / 4.0;
            let (px, py, side) = (x + pad + quarter, y + pad + quarter, inner / 2.0);
            let (pw, ph) = (side as i32, side as i32);
            canvas.fill(px as i32, py as i32, pw, ph, colors.placeholder);
        }
        if i == viewer.current() {
            let inset = 2.0 * scale;
            let (bx, by) = ((x + inset) as i32, (y + inset) as i32);
            let side = (size - 2.0 * inset) as i32;
            let t = ((3.0 * scale).round() as i32).max(1);
            let selection = colors.selection;
            canvas.fill(bx, by, side, t, selection);
            canvas.fill(bx, by + side - t, side, t, selection);
            canvas.fill(bx, by, t, side, selection);
            canvas.fill(bx + side - t, by, t, side, selection);
        }
        if entry.marked {
            let d = 8.0 * scale;
            let center = (
                (x + size - 6.0 * scale - d / 2.0) as i64,
                (y + 6.0 * scale + d / 2.0) as i64,
            );
            dot(canvas, center, (d / 2.0) as i64, colors.mark);
        }
    }
    drawn
}

/// Whether every pixel a cell `size` pixels a side draws, its thumbnail of `thumb_size` units
/// across at most, its placeholder, its selection and its mark, at `scale` pixels a unit, lies
/// within the cell, whatever its corner, which the drawing truncates by up to a pixel either
/// way: so no cell's drawing reaches another's thumbnail. Worked from the drawing in `draw_kept`.
fn cells_hold_their_drawings(size: f64, thumb_size: f64, scale: f64) -> bool {
    // the padding around the thumbnail, which holds a thumbnail or a placeholder of the inner
    // square, each placed and sized by truncation, to its cell
    let pad = (size - thumb_size * scale) / 2.0;
    // the mark: a disk of radius r, ⌊4 × scale⌋, on pixels from its center's r either way,
    // its center 10 × scale in from the cell's top and right edges, truncated
    let (reach, radius) = (10.0 * scale, (4.0 * scale).floor());
    let mark = reach - radius >= 2.0 && size - reach - radius >= 2.0;
    // the selection: a border from 2 × scale in, truncated, as wide as the cell less
    // 4 × scale, truncated, max(1, round(3 × scale)) thick
    let thickness = (3.0 * scale).round().max(1.0);
    let selection = 2.0 * scale >= 1.0 && size - 4.0 * scale - 1.0 >= thickness;
    pad >= 1.0 && mark && selection
}

/// Copies `pixels`, rows of `size.0`, to the canvas with their top left corner at `at`, as
/// far as they lie on it.
fn copy(canvas: &mut Canvas, at: (i64, i64), size: (i64, i64), pixels: &[u32]) {
    let (width, height) = (i64::from(canvas.width()), i64::from(canvas.height()));
    let (left, right) = (at.0.max(0), (at.0 + size.0).min(width));
    if left >= right {
        return;
    }
    let canvas = canvas.pixels_mut();
    for row in at.1.max(0)..(at.1 + size.1).min(height) {
        let from = ((row - at.1) * size.0 + left - at.0) as usize;
        let to = (row * width + left) as usize;
        let length = (right - left) as usize;
        canvas[to..to + length].copy_from_slice(&pixels[from..from + length]);
    }
}

/// The pixels of the rectangle at `at` of `size`, rows of `size.0`, where all of it lies on
/// the canvas.
fn taken(canvas: &Canvas, at: (i64, i64), size: (i64, i64)) -> Option<Vec<u32>> {
    let (width, height) = (i64::from(canvas.width()), i64::from(canvas.height()));
    if at.0 < 0 || at.1 < 0 || at.0 + size.0 > width || at.1 + size.1 > height {
        return None;
    }
    let pixels = canvas.pixels();
    let mut taken = Vec::with_capacity((size.0 * size.1) as usize);
    for row in at.1..at.1 + size.1 {
        let from = (row * width + at.0) as usize;
        taken.extend_from_slice(&pixels[from..from + size.0 as usize]);
    }
    Some(taken)
}

/// A disk of `radius` around the center of the pixel at `center`, its edge antialiased.
fn dot(canvas: &mut Canvas, center: (i64, i64), radius: i64, color: u32) {
    let (width, height) = (i64::from(canvas.width()), i64::from(canvas.height()));
    let pixels = canvas.pixels_mut();
    for row in (center.1 - radius).max(0)..=(center.1 + radius).min(height - 1) {
        for column in (center.0 - radius).max(0)..=(center.0 + radius).min(width - 1) {
            let distance =
                ((column - center.0).pow(2) as f64 + (row - center.1).pow(2) as f64).sqrt();
            let cover = (radius as f64 + 0.5 - distance).clamp(0.0, 1.0);
            let alpha = (cover * 255.0).round() as u32;
            if alpha == 0 {
                continue;
            }
            // the color's channels, alpha included, scaled to the part of the pixel covered
            let faded = [0, 8, 16, 24]
                .into_iter()
                .map(|shift| ((((color >> shift) & 0xff) * alpha + 127) / 255) << shift)
                .sum();
            let pixel = &mut pixels[(row * width + column) as usize];
            *pixel = canvas::over(faded, *pixel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsxiv_ng_core::Size;
    use nsxiv_ng_core::config::Config;
    use nsxiv_ng_core::file_list::FileEntry;
    use nsxiv_ng_core::options::Options;

    const BACKGROUND: u32 = canvas::rgb(255, 255, 255);
    const COLORS: GridColors = GridColors {
        placeholder: canvas::rgb(0xee, 0xee, 0xee),
        selection: canvas::rgb(0x34, 0x78, 0xf6),
        mark: canvas::rgb(0xff, 0xcc, 0x00),
    };

    // a grid of `count` files in a view of two 148-pixel cells side by side, at 128 a thumbnail
    fn viewer(count: usize) -> Viewer {
        let options = Options {
            thumbnail_mode: true,
            ..Options::default()
        };
        let files = (0..count)
            .map(|i| FileEntry::new(format!("/pics/{i}.png").into()))
            .collect();
        let mut viewer = Viewer::new(options, files);
        viewer.resized(Size {
            width: 296.0,
            height: 148.0,
        });
        viewer
    }

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        rgba.repeat((w * h) as usize)
    }

    fn drawn<'a>(
        viewer: &Viewer,
        scale: f64,
        thumbnail: impl FnMut(usize) -> Option<Image<'a>>,
    ) -> Vec<u32> {
        let (width, height) = ((296.0 * scale) as u32, (148.0 * scale) as u32);
        let mut pixels = vec![BACKGROUND; (width * height) as usize];
        let mut canvas = Canvas::new(&mut pixels, width, height).unwrap();
        draw(&mut canvas, viewer, scale, COLORS, thumbnail);
        pixels
    }

    #[test]
    fn thumbnails_fit_their_padded_cells_and_are_never_enlarged() {
        let (large, small) = (
            solid(320, 240, [255, 0, 0, 255]),
            solid(50, 20, [0, 255, 0, 255]),
        );
        let thumbs = [
            Image {
                width: 320,
                height: 240,
                levels: &[&large],
                opaque: true,
            },
            Image {
                width: 50,
                height: 20,
                levels: &[&small],
                opaque: true,
            },
        ];
        let mut viewer = viewer(2);
        viewer.select(1);
        let pixels = drawn(&viewer, 1.0, |i| thumbs.get(i).copied());
        let at = |x: usize, y: usize| pixels[y * 296 + x];
        let (red, green) = (canvas::rgb(255, 0, 0), canvas::rgb(0, 255, 0));
        // 320 × 240 into 128 × 128 gives 128 × 96, centered from row 26
        assert_eq!([at(10, 26), at(137, 121)], [red, red]);
        assert_eq!(
            [at(9, 26), at(138, 121), at(10, 25), at(10, 122)],
            [BACKGROUND; 4]
        );
        // 50 × 20 stays its size, centered in the second cell's 158..286 by 10..138
        assert_eq!([at(197, 64), at(246, 83)], [green, green]);
        assert_eq!(
            [at(196, 64), at(247, 83), at(197, 63), at(246, 84)],
            [BACKGROUND; 4]
        );
    }

    #[test]
    fn the_padding_and_the_thumbnail_size_come_from_the_layout() {
        // 100 a thumbnail and 24 around it: the same 148 cells, thumbnails further in
        let config = Config {
            thumb_sizes: vec![100.0],
            thumb_size_index: 0,
            thumb_padding: 24.0,
            ..Config::default()
        };
        let options = Options {
            thumbnail_mode: true,
            ..Options::default()
        };
        let mut viewer =
            Viewer::with_config(options, vec![FileEntry::new("/pics/0.png".into())], &config);
        viewer.resized(Size {
            width: 296.0,
            height: 148.0,
        });
        let large = solid(320, 240, [255, 0, 0, 255]);
        let thumb = Image {
            width: 320,
            height: 240,
            levels: &[&large],
            opaque: true,
        };
        let pixels = drawn(&viewer, 1.0, |_| Some(thumb));
        let at = |x: usize, y: usize| pixels[y * 296 + x];
        // 320 × 240 into 100 × 100 from (24, 24) gives 100 × 75 from row 36
        let red = canvas::rgb(255, 0, 0);
        assert_eq!([at(24, 36), at(123, 110)], [red, red]);
        assert_eq!(
            [at(23, 36), at(124, 110), at(24, 35), at(24, 111)],
            [BACKGROUND; 4]
        );
    }

    #[test]
    fn a_thin_thumbnail_keeps_a_line_of_a_pixel() {
        // 256 × 1 and 1 × 256 at half size would be half a pixel thick
        let (wide, tall) = (
            solid(256, 1, [255, 0, 0, 255]),
            solid(1, 256, [0, 255, 0, 255]),
        );
        let thumbs = [
            Image {
                width: 256,
                height: 1,
                levels: &[&wide],
                opaque: true,
            },
            Image {
                width: 1,
                height: 256,
                levels: &[&tall],
                opaque: true,
            },
        ];
        let pixels = drawn(&viewer(2), 1.0, |i| thumbs.get(i).copied());
        let at = |x: usize, y: usize| pixels[y * 296 + x];
        let (red, green) = (canvas::rgb(255, 0, 0), canvas::rgb(0, 255, 0));
        // centered at 10 + 63.75 in each padded cell, across and down
        assert_eq!([at(10, 73), at(137, 73)], [red, red]);
        assert_eq!([at(10, 72), at(10, 74)], [BACKGROUND; 2]);
        assert_eq!([at(221, 10), at(221, 137)], [green, green]);
        assert_eq!([at(220, 10), at(222, 10)], [BACKGROUND; 2]);
    }

    #[test]
    fn a_kept_thumbnail_copied_is_the_thumbnail_drawn_where_it_lies_on_the_canvas() {
        // thumbnails with alpha over the background, each row its own, in two rows of two cells
        let pixels: Vec<Vec<u8>> = (0..4u8)
            .map(|i| {
                (0..150u8)
                    .flat_map(|row| [60 * i, row, 90, 100 + 40 * i].repeat(200))
                    .collect()
            })
            .collect();
        let levels: Vec<[&[u8]; 1]> = pixels.iter().map(|p| [&p[..]]).collect();
        let thumbs: Vec<Image> = levels
            .iter()
            .map(|levels| Image {
                width: 200,
                height: 150,
                levels,
                opaque: false,
            })
            .collect();
        let mut viewer = viewer(4);
        let (width, height) = (296, 148);
        let mut first = vec![BACKGROUND; width * height];
        let mut canvas = Canvas::new(&mut first, width as u32, height as u32).unwrap();
        let none = |_: usize, _: (u32, u32)| None;
        let kept = draw_kept(
            &mut canvas,
            &viewer,
            1.0,
            COLORS,
            |i| thumbs.get(i).copied(),
            none,
            |_, _| true,
        );
        assert_eq!(
            kept.iter().map(|d| (d.index, d.size)).collect::<Vec<_>>(),
            [(0, (128, 96)), (1, (128, 96))],
            "the thumbnails whole on the canvas"
        );
        // half a cell down, the second row half in view and the kept first row half out
        let _ = viewer.scroll_grid(74.0);
        let lookup = |i: usize, size| {
            let drawn = kept.iter().find(|d| d.index == i && d.size == size)?;
            Some(&drawn.pixels[..])
        };
        let mut copied = vec![BACKGROUND; width * height];
        let mut canvas = Canvas::new(&mut copied, width as u32, height as u32).unwrap();
        draw_kept(
            &mut canvas,
            &viewer,
            1.0,
            COLORS,
            |i| thumbs.get(i).copied(),
            lookup,
            |_, _| true,
        );
        let afresh = drawn(&viewer, 1.0, |i| thumbs.get(i).copied());
        assert_eq!(copied, afresh);
    }

    #[test]
    fn a_cell_too_small_to_hold_its_thumbnail_alone_is_neither_kept_nor_copied() {
        // two transparent thumbnails at a thousandth of a pixel a unit, both a pixel at the
        // canvas's corner, under the first file's mark
        let clear = solid(200, 150, [10, 20, 30, 0]);
        let thumb = Image {
            width: 200,
            height: 150,
            levels: &[&clear],
            opaque: false,
        };
        let mut viewer = viewer(2);
        viewer.toggle_mark(0);
        let scale = 0.001;
        let mut pixels = vec![BACKGROUND; 4];
        let mut canvas = Canvas::new(&mut pixels, 2, 2).unwrap();
        let none = |_: usize, _: (u32, u32)| None;
        let kept = draw_kept(
            &mut canvas,
            &viewer,
            scale,
            COLORS,
            |_| Some(thumb),
            none,
            |_, _| true,
        );
        // the mark gone, a cell kept from the frame before must not bring it back
        viewer.toggle_mark(0);
        let lookup = |i: usize, size| {
            let drawn = kept.iter().find(|d| d.index == i && d.size == size)?;
            Some(&drawn.pixels[..])
        };
        let mut copied = vec![BACKGROUND; 4];
        let mut canvas = Canvas::new(&mut copied, 2, 2).unwrap();
        draw_kept(
            &mut canvas,
            &viewer,
            scale,
            COLORS,
            |_| Some(thumb),
            lookup,
            |_, _| true,
        );
        let mut afresh = vec![BACKGROUND; 4];
        let mut canvas = Canvas::new(&mut afresh, 2, 2).unwrap();
        draw(&mut canvas, &viewer, scale, COLORS, |_| Some(thumb));
        assert_eq!(copied, afresh);
        assert!(kept.is_empty(), "nothing kept from cells that small");
    }

    #[test]
    fn a_cell_a_neighbor_s_mark_reaches_is_neither_kept_nor_copied() {
        // cells of 12 pixels, a thumbnail of 8 in each, one above the other: the first cell's
        // mark, from row 6 to 14, reaches the second's thumbnail, from row 14
        let config = Config {
            thumb_sizes: vec![8.0],
            thumb_size_index: 0,
            thumb_padding: 2.0,
            ..Config::default()
        };
        let options = Options {
            thumbnail_mode: true,
            ..Options::default()
        };
        let files = (0..2)
            .map(|i| FileEntry::new(format!("/pics/{i}.png").into()))
            .collect();
        let mut viewer = Viewer::with_config(options, files, &config);
        viewer.resized(Size {
            width: 12.0,
            height: 24.0,
        });
        viewer.toggle_mark(0);
        let clear = solid(100, 100, [10, 20, 30, 0]);
        let thumb = Image {
            width: 100,
            height: 100,
            levels: &[&clear],
            opaque: false,
        };
        let mut pixels = vec![BACKGROUND; 12 * 24];
        let mut canvas = Canvas::new(&mut pixels, 12, 24).unwrap();
        let none = |_: usize, _: (u32, u32)| None;
        let room = |_: usize, _: (u32, u32)| true;
        let kept = draw_kept(
            &mut canvas,
            &viewer,
            1.0,
            COLORS,
            |_| Some(thumb),
            none,
            room,
        );
        // the mark gone, a cell kept from the frame before must not bring it back
        viewer.toggle_mark(0);
        let lookup = |i: usize, size| {
            let drawn = kept.iter().find(|d| d.index == i && d.size == size)?;
            Some(&drawn.pixels[..])
        };
        let mut copied = vec![BACKGROUND; 12 * 24];
        let mut canvas = Canvas::new(&mut copied, 12, 24).unwrap();
        draw_kept(
            &mut canvas,
            &viewer,
            1.0,
            COLORS,
            |_| Some(thumb),
            lookup,
            room,
        );
        let mut afresh = vec![BACKGROUND; 12 * 24];
        let mut canvas = Canvas::new(&mut afresh, 12, 24).unwrap();
        draw(&mut canvas, &viewer, 1.0, COLORS, |_| Some(thumb));
        assert_eq!(copied, afresh);
        assert!(kept.is_empty(), "nothing kept from cells that small");
    }

    #[test]
    fn a_thumbnail_with_no_room_to_be_kept_is_not_taken_from_the_canvas() {
        let red = solid(200, 150, [255, 0, 0, 255]);
        let thumb = Image {
            width: 200,
            height: 150,
            levels: &[&red],
            opaque: true,
        };
        let viewer = viewer(2);
        let mut pixels = vec![BACKGROUND; 296 * 148];
        let mut canvas = Canvas::new(&mut pixels, 296, 148).unwrap();
        let mut asked = Vec::new();
        let none = |_: usize, _: (u32, u32)| None;
        let room = |i: usize, size| {
            asked.push((i, size));
            false
        };
        let kept = draw_kept(
            &mut canvas,
            &viewer,
            1.0,
            COLORS,
            |_| Some(thumb),
            none,
            room,
        );
        assert!(kept.is_empty());
        assert_eq!(asked, [(0, (128, 96)), (1, (128, 96))]);
    }

    #[test]
    fn a_missing_thumbnail_gets_the_placeholder() {
        let pixels = drawn(&viewer(1), 1.0, |_| None);
        let at = |x: usize, y: usize| pixels[y * 296 + x];
        // the middle half of the padded cell, 42..106 both ways
        let placeholder = COLORS.placeholder;
        assert_eq!([at(42, 42), at(105, 105)], [placeholder, placeholder]);
        assert_eq!(
            [at(41, 42), at(106, 105), at(42, 41), at(42, 106)],
            [BACKGROUND; 4]
        );
    }

    #[test]
    fn the_current_file_gets_the_border_and_a_marked_one_its_dot() {
        let mut viewer = viewer(2);
        viewer.select(1);
        viewer.toggle_mark(0);
        // at scale 2, the second cell spans 296..592, its border 4 in and 6 thick
        let pixels = drawn(&viewer, 2.0, |_| None);
        let at = |x: usize, y: usize| pixels[y * 592 + x];
        let selection = COLORS.selection;
        assert_eq!(
            [at(300, 4), at(305, 150), at(587, 150), at(450, 291)],
            [selection; 4]
        );
        assert_eq!(
            [at(299, 150), at(306, 150), at(450, 3), at(450, 285)],
            [BACKGROUND; 4]
        );
        assert_eq!(at(150, 4), BACKGROUND, "the other cell has no border");
        // the dot of radius 8 centered 6 + 4 in from the first cell's top right corner
        let mark = COLORS.mark;
        assert_eq!([at(276, 20), at(269, 20), at(276, 27)], [mark; 3]);
        assert_eq!([at(266, 20), at(276, 30), at(270, 13)], [BACKGROUND; 3]);
        // half of the pixel on the edge
        assert_eq!(at(268, 20), canvas::over(0x8080_6600, BACKGROUND));
        assert_eq!(at(560, 20), BACKGROUND, "the other file is not marked");
    }
}
