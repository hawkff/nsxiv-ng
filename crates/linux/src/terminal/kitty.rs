//! Commands of the kitty graphics protocol, and what the terminal answers to them.
//!
//! In tmux, `tmux` is true: commands go in tmux's passthrough to the terminal around it, and
//! images show through virtual placements that tmux's placeholders name.

use std::path::Path;

use base64::prelude::{BASE64_STANDARD, Engine};
use nsxiv_ng_core::Size;
use nsxiv_ng_core::viewport::Viewport;

use super::tmux::passthrough;

/// A kitty graphics command with `keys`, which may end in `;` and a payload.
pub fn command(keys: &str, tmux: bool) -> String {
    let command = format!("\x1b_G{keys}\x1b\\");
    if tmux { passthrough(&command) } else { command }
}

/// Every image on screen, quietly; nothing in tmux, where it would take those of other panes.
pub fn delete_all(tmux: bool) -> String {
    if tmux {
        String::new()
    } else {
        command("a=d,d=A,q=2", tmux)
    }
}

/// Image `id` and its placements out of the terminal.
pub fn delete(id: u32, tmux: bool) -> String {
    command(&format!("a=d,d=I,i={id},q=2"), tmux)
}

/// A PNG as a kitty image, in the protocol's chunks of at most 4096 bytes of base64, each a
/// command of its own, with `keys` on the first. q=2 keeps the terminal's answers from arriving
/// as keys.
pub fn kitty_image(png: &[u8], keys: &str, tmux: bool) -> Vec<u8> {
    let payload = BASE64_STANDARD.encode(png);
    let mut out = Vec::with_capacity(payload.len() + payload.len() / 4096 * 64 + 64);
    let mut start = 0;
    loop {
        let end = payload.len().min(start + 4096);
        let more = u8::from(end < payload.len());
        let keys = if start == 0 {
            format!("{keys},f=100,t=d,q=2,m={more};{}", &payload[start..end])
        } else {
            format!("m={more},q=2;{}", &payload[start..end])
        };
        // base64 holds no escape for tmux to double
        out.extend_from_slice(command(&keys, tmux).as_bytes());
        start = end;
        if start >= payload.len() {
            return out;
        }
    }
}

/// A frame as the raw RGB bytes, or RGBA with `alpha`, of a pixel file the terminal reads: straight
/// alpha, so a clear pixel is all zero. Without `alpha`, color stays as it is premultiplied,
/// which is the frame over black.
pub fn pixel_file(frame: &[u32], alpha: bool) -> Vec<u8> {
    let channels = if alpha { 4 } else { 3 };
    let mut bytes = Vec::with_capacity(channels * frame.len());
    for &pixel in frame {
        if alpha {
            bytes.extend_from_slice(&super::frame::straight(pixel));
        } else {
            bytes.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
        }
    }
    bytes
}

/// The command that sends a pixel file of `width × height` pixels, RGB or RGBA with `alpha`,
/// with `keys` before its own. The terminal deletes a `temporary` file once it read it (t=t),
/// which it does only for a file in a temporary directory whose path holds
/// `tty-graphics-protocol`; it leaves any other (t=f).
pub fn kitty_file(
    path: &Path,
    keys: &str,
    width: u32,
    height: u32,
    alpha: bool,
    temporary: bool,
    tmux: bool,
) -> String {
    let format = if alpha { 32 } else { 24 };
    let medium = if temporary { 't' } else { 'f' };
    let file = BASE64_STANDARD.encode(path.as_os_str().as_encoded_bytes());
    command(
        &format!("{keys},f={format},s={width},v={height},t={medium},q=2;{file}"),
        tmux,
    )
}

/// The keys of placement 1 of image `id` over the `columns × rows` cells above the bar: at the
/// top left, or in tmux a virtual one, which the placeholders show, or for a crop over its box.
pub fn placement(id: u32, crop: Option<&Crop>, columns: usize, rows: usize, tmux: bool) -> String {
    if tmux {
        let rows = rows.min(super::tmux::DIACRITICS.len());
        return format!("U=1,i={id},p=1,c={columns},r={rows}");
    }
    match crop {
        None => format!("i={id},p=1,C=1,c={columns},r={rows}"),
        Some(crop) => format!(
            "i={id},p=1,C=1,c={},r={},X={},Y={}",
            crop.columns, crop.rows, crop.x, crop.y
        ),
    }
}

/// Where a placement goes: the top left, where a frame starts, or a crop's first cell.
pub fn cursor(crop: Option<&Crop>) -> String {
    crop.map_or_else(String::new, |crop| {
        format!("\x1b[{};{}H", crop.row + 1, crop.column + 1)
    })
}

/// An image the view shows whole, with room around it, goes as a frame of its own, without the
/// transparent rest of the view: fewer bytes for the terminal, and for herdr to send on to its
/// client, and opaque ones, which need no alpha. Its box spans whole cells, its right and
/// bottom edges on cell boundaries and the left and top ones offset within their cell, so the
/// terminal, and a herdr client after it, shows it at its size; the box sits within a cell of
/// where the view has the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    /// The box, in cells.
    pub column: usize,
    pub row: usize,
    pub columns: usize,
    pub rows: usize,
    /// The image's offset in the box's first cell, in pixels.
    pub x: usize,
    pub y: usize,
    /// The frame's pixels.
    pub width: u32,
    pub height: u32,
}

/// The crop of `view` on a canvas of `canvas` units in `columns × rows` cells of `cell` pixels
/// (width, height), the frame drawn at `scale`; None when the image fills the view or runs
/// past it. A unit is a sixteenth of a cell's height.
pub fn crop(
    view: &Viewport,
    canvas: Size,
    cell: (f64, f64),
    scale: f64,
    columns: usize,
    rows: usize,
) -> Option<Crop> {
    let image = view.image_rect();
    let pixel = cell.1 / 16.0; // the terminal's pixels in a unit of the view
    let (cell_width, cell_height) = (cell.0 as i64, cell.1 as i64);
    let (columns, rows) = (columns as i64, rows as i64);
    let inside = image.x >= -0.01
        && image.y >= -0.01
        && image.max_x() <= canvas.width + 0.01
        && image.max_y() <= canvas.height + 0.01;
    if cell_width <= 0 || cell_height <= 0 || !inside {
        return None;
    }
    let width = (image.width * pixel).round() as i64;
    let height = (image.height * pixel).round() as i64;
    if width <= 0 || height <= 0 || (width >= columns * cell_width && height >= rows * cell_height)
    {
        return None;
    }
    let box_columns = (width + cell_width - 1) / cell_width;
    let box_rows = (height + cell_height - 1) / cell_height;
    if box_columns > columns || box_rows > rows {
        return None;
    }
    let x = box_columns * cell_width - width;
    let y = box_rows * cell_height - height;
    let left = image.x * pixel;
    let top = (canvas.height - image.max_y()) * pixel;
    let column =
        (((left - x as f64) / cell_width as f64).round() as i64).clamp(0, columns - box_columns);
    let row = (((top - y as f64) / cell_height as f64).round() as i64).clamp(0, rows - box_rows);
    Some(Crop {
        column: column as usize,
        row: row as usize,
        columns: box_columns as usize,
        rows: box_rows as usize,
        x: x as usize,
        y: y as usize,
        width: ((image.width * scale).round() as u32).max(1),
        height: ((image.height * scale).round() as u32).max(1),
    })
}

/// What the terminal answered to the support probe: kitty graphics, the viewer's pixel files,
/// and sixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Graphics {
    pub kitty: bool,
    pub files: bool,
    pub sixel: bool,
}

/// What the replies to the probe say, None until the device attributes end them. The probe asks
/// for a one-pixel image as id 31 (`i=31,s=1,v=1,a=q,t=d,f=24;AAAA`), then for the pixel in a
/// file as id 32, then for the device attributes (ESC [ c). A terminal with kitty graphics
/// answers the queries with OK before the attributes, whose parameters after the first name
/// the terminal's features, 4 for sixels.
pub fn graphics(reply: &[u8]) -> Option<Graphics> {
    let start = find(reply, b"\x1b[?")?;
    let attributes = &reply[start + 3..];
    let end = attributes.iter().position(|&b| b == b'c')?;
    let answers = &reply[..start];
    let sixel = attributes[..end]
        .split(|&b| b == b';')
        .filter(|feature| !feature.is_empty())
        .skip(1)
        .any(|feature| feature == b"4");
    Some(Graphics {
        kitty: find(answers, b"\x1b_Gi=31;OK").is_some(),
        files: find(answers, b"\x1b_Gi=32;OK").is_some(),
        sixel,
    })
}

/// The kitty images that the terminal's replies say are gone, as when it needed their room.
pub fn missing_images(bytes: &[u8]) -> Vec<u32> {
    let text = String::from_utf8_lossy(bytes);
    text.split("\x1b_Gi=")
        .skip(1)
        .filter_map(|reply| {
            let end = reply.find("\x1b\\")?;
            if reply[..end].ends_with(";OK") {
                return None;
            }
            let digits = reply.find(|c: char| !c.is_numeric()).unwrap_or(reply.len());
            reply[..digits].parse().ok()
        })
        .collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kitty_image_goes_in_chunks_of_4096_bytes_of_base64_the_keys_on_the_first() {
        let png: Vec<u8> = (0..5000).map(|i| (i & 0xff) as u8).collect();
        let sent = kitty_image(&png, "a=T,i=2,p=1,C=1,c=10,r=4", false);
        let text = String::from_utf8(sent).unwrap();
        let chunks: Vec<(&str, &str)> = text
            .split("\x1b\\")
            .filter(|c| !c.is_empty())
            .map(|c| c[3..].split_once(';').unwrap())
            .collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, "a=T,i=2,p=1,C=1,c=10,r=4,f=100,t=d,q=2,m=1");
        assert_eq!(chunks[1].0, "m=0,q=2");
        assert_eq!(chunks[0].1.len(), 4096);
        assert_eq!(
            chunks[0].1.to_owned() + chunks[1].1,
            BASE64_STANDARD.encode(&png)
        );
        // through tmux, each chunk goes in a passthrough of its own
        let through = kitty_image(&png[..3], "a=T,i=2", true);
        assert_eq!(
            through,
            b"\x1bPtmux;\x1b\x1b_Ga=T,i=2,f=100,t=d,q=2,m=0;AAEC\x1b\x1b\\\x1b\\"
        );
        // no payload still makes the one command that carries the keys
        assert_eq!(
            kitty_image(&[], "a=t,i=3", false),
            b"\x1b_Ga=t,i=3,f=100,t=d,q=2,m=0;\x1b\\"
        );
    }

    #[test]
    fn a_pixel_file_holds_straight_rgb_or_rgba_and_its_command_names_it() {
        // opaque red, half a premultiplied green, and clear
        let frame = [0xffff_0000, 0x8000_8000, 0];
        assert_eq!(pixel_file(&frame, false), [255, 0, 0, 0, 128, 0, 0, 0, 0]);
        assert_eq!(
            pixel_file(&frame, true),
            [255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 0, 0]
        );
        let path = Path::new("/tmp/tty-graphics-protocol-nsxiv-a/7");
        let file = BASE64_STANDARD.encode("/tmp/tty-graphics-protocol-nsxiv-a/7");
        assert_eq!(
            kitty_file(path, "a=T,i=7,p=1", 3, 1, false, true, false),
            format!("\x1b_Ga=T,i=7,p=1,f=24,s=3,v=1,t=t,q=2;{file}\x1b\\")
        );
        assert_eq!(
            kitty_file(path, "a=t,i=7", 3, 1, true, false, false),
            format!("\x1b_Ga=t,i=7,f=32,s=3,v=1,t=f,q=2;{file}\x1b\\")
        );
    }

    #[test]
    fn deletes_placements_and_cursors() {
        assert_eq!(delete(5, false), "\x1b_Ga=d,d=I,i=5,q=2\x1b\\");
        assert_eq!(delete_all(false), "\x1b_Ga=d,d=A,q=2\x1b\\");
        assert_eq!(delete_all(true), "", "in tmux, other panes' images stay");
        let crop = Crop {
            column: 6,
            row: 7,
            columns: 88,
            rows: 15,
            x: 5,
            y: 10,
            width: 700,
            height: 232,
        };
        assert_eq!(placement(9, None, 100, 29, false), "i=9,p=1,C=1,c=100,r=29");
        assert_eq!(
            placement(9, Some(&crop), 100, 29, false),
            "i=9,p=1,C=1,c=88,r=15,X=5,Y=10"
        );
        // placeholders name at most as many rows as there are diacritics
        assert_eq!(
            placement(9, None, 100, 400, true),
            "U=1,i=9,p=1,c=100,r=297"
        );
        assert_eq!(cursor(Some(&crop)), "\x1b[8;7H");
        assert_eq!(cursor(None), "");
    }

    #[test]
    fn an_image_shown_whole_goes_alone_in_a_box_of_whole_cells() {
        // a 700x232 image shown whole at 100% in a view of 800x464 units, 100 columns by 29 rows
        // of 10x20 pixels: 875x290 pixels, in a box of 88 columns by 15 rows from column 6 and row
        // 7, its right and bottom edges on cell boundaries; an image that fills the view, or runs
        // past it, goes with the view
        let view_size = Size {
            width: 800.0,
            height: 464.0,
        };
        let mut whole = Viewport::default();
        whole.set_view_size(view_size);
        whole.set_image_size(
            Size {
                width: 700.0,
                height: 232.0,
            },
            true,
        );
        let mut filling = whole.clone();
        filling.set_image_size(
            Size {
                width: 2000.0,
                height: 1160.0,
            },
            true,
        );
        let mut past = whole.clone();
        past.zoom_step(1);
        let crop = |view: &Viewport| crop(view, view_size, (10.0, 20.0), 1.0, 100, 29);
        assert_eq!(
            crop(&whole),
            Some(Crop {
                column: 6,
                row: 7,
                columns: 88,
                rows: 15,
                x: 5,
                y: 10,
                width: 700,
                height: 232
            })
        );
        assert_eq!(crop(&filling), None);
        assert_eq!(crop(&past), None);
    }

    #[test]
    fn the_probe_tells_kitty_graphics_files_and_sixels_once_the_device_attributes_came() {
        let graphics = |reply: &[u8]| graphics(reply).map(|g| (g.kitty, g.files, g.sixel));
        // herdr's answer, that of a terminal that cannot read the viewer's files, that of one
        // without kitty graphics, and an answer still on its way
        assert_eq!(
            graphics(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;OK\x1b\\\x1b[?62;22c"),
            Some((true, true, false))
        );
        assert_eq!(
            graphics(
                b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;EBADF:Failed to read image file\x1b\\\x1b[?62;22c"
            ),
            Some((true, false, false))
        );
        assert_eq!(graphics(b"\x1b[?1;2c"), Some((false, false, false)));
        // foot's attributes, and a VT132's, whose 4 is its class rather than sixels
        assert_eq!(graphics(b"\x1b[?62;4;22c"), Some((false, false, true)));
        assert_eq!(graphics(b"\x1b[?4;2c"), Some((false, false, false)));
        assert_eq!(graphics(b"\x1b_Gi=31;OK\x1b\\\x1b[?62"), None);
    }

    #[test]
    fn a_placement_the_terminal_could_not_make_names_its_image() {
        let answers = b"\x1b_Gi=77,p=1;ENOENT:No such image\x1b\\\x1b_Gi=78;OK\x1b\\";
        assert_eq!(missing_images(answers), [77]);
        // a reply still on its way, and one with no id, name nothing
        assert!(missing_images(b"\x1b_Gi=79;ENOENT").is_empty());
        assert!(missing_images(b"\x1b_Gi=;ENOENT\x1b\\").is_empty());
    }
}
