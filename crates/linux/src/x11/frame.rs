//! The window's pixels: the visual whose format is the canvas's own, the shared memory a frame
//! is drawn into, and the pieces a frame goes in where there is none.

use std::ffi::c_void;
use std::os::fd::OwnedFd;

use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{ImageOrder, Screen, Setup, VisualClass, Visualid};

/// A TrueColor visual of depth 24 or 32 whose pixels are the canvas's premultiplied
/// `0xAARRGGBB` words as they lie in memory, with its depth: the root's own where it is one,
/// else one of depth 24, then 32. None where the server has no such visual.
pub(crate) fn visual(setup: &Setup, screen: &Screen) -> Option<(Visualid, u8)> {
    let native = if cfg!(target_endian = "little") {
        ImageOrder::LSB_FIRST
    } else {
        ImageOrder::MSB_FIRST
    };
    if setup.image_byte_order != native {
        return None;
    }
    let words = |depth: u8| {
        setup
            .pixmap_formats
            .iter()
            .any(|f| f.depth == depth && f.bits_per_pixel == 32)
    };
    screen
        .allowed_depths
        .iter()
        .filter(|d| matches!(d.depth, 24 | 32) && words(d.depth))
        .flat_map(|d| d.visuals.iter().map(move |v| (v, d.depth)))
        .filter(|(v, _)| {
            v.class == VisualClass::TRUE_COLOR
                && (v.red_mask, v.green_mask, v.blue_mask) == (0xff_0000, 0xff00, 0xff)
        })
        // the root's own needs no colormap of its own
        .min_by_key(|&(v, depth)| (v.visual_id != screen.root_visual, depth))
        .map(|(v, depth)| (v.visual_id, depth))
}

/// The pixels and the bytes of a frame `width × height`, None where the bytes overflow or
/// pass `max_bytes`, which for memory is `isize::MAX`, the most one slice may span.
pub(crate) fn frame_size(width: u16, height: u16, max_bytes: usize) -> Option<(usize, usize)> {
    let count = usize::from(width).checked_mul(usize::from(height))?;
    let bytes = count.checked_mul(4).filter(|&bytes| bytes <= max_bytes)?;
    Some((count, bytes))
}

/// The rows of each PutImage request for a frame `width` pixels wide and `height` high, as
/// the first row and the count, so each request stays within `max_bytes`.
// shortcut: a row and the header past `max_bytes` still go as one request, which the server
// refuses; servers take 262140 bytes even without BIG-REQUESTS, more than a row of the widest
// window, so rows are not split
pub(crate) fn pieces(
    width: u16,
    height: u16,
    max_bytes: usize,
) -> impl Iterator<Item = (u16, u16)> {
    // the request's header, with the length BIG-REQUESTS adds
    let room = max_bytes.saturating_sub(28);
    let rows = (room / (usize::from(width).max(1) * 4)).clamp(1, usize::from(u16::MAX)) as u16;
    (0..height)
        .step_by(usize::from(rows))
        .map(move |y| (y, rows.min(height - y)))
}

/// A MIT-SHM segment the server made and passed as a descriptor, mapped here.
pub(crate) struct Segment {
    pub(crate) id: u32,
    map: *mut c_void,
    len: usize,
}

impl Segment {
    /// A segment of `len` bytes, or None where the server cannot make one or pass it, as over
    /// the network.
    pub(crate) fn new(conn: &impl Connection, len: usize) -> Option<Segment> {
        if len > isize::MAX as usize {
            return None;
        }
        let id = conn.generate_id().ok()?;
        let size = u32::try_from(len).ok()?;
        let reply = conn
            .shm_create_segment(id, size, false)
            .ok()?
            .reply()
            .ok()?;
        let fd: OwnedFd = reply.shm_fd;
        let flags = ProtFlags::READ | ProtFlags::WRITE;
        // SAFETY: a new mapping at an address the kernel picks, of a descriptor that is ours,
        // which aliases no memory Rust knows of; the server keeps its own mapping of it.
        let map = unsafe { mmap(std::ptr::null_mut(), len, flags, MapFlags::SHARED, &fd, 0) };
        match map {
            Ok(map) => Some(Segment { id, map, len }),
            Err(_) => {
                let _ = conn.shm_detach(id);
                None
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The first `count` pixels of the segment, which must hold them.
    pub(crate) fn pixels(&mut self, count: usize) -> &mut [u32] {
        assert!(count <= self.len / 4, "a frame larger than its segment");
        // SAFETY: the slice's `count * 4` bytes are at most `len`, which `new` kept within
        // `isize::MAX` and mapped whole, readable and writable, page-aligned and so aligned
        // for u32, alive until `drop`, and borrowed through `&mut self` alone. The server
        // reads it only between a put and its completion, during which nothing draws.
        unsafe { std::slice::from_raw_parts_mut(self.map.cast::<u32>(), count) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, of `len` bytes, which no borrow outlives, as
        // `pixels` borrows `self`.
        let _ = unsafe { munmap(self.map, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{Depth, Format, Visualtype};

    fn true_color(visual_id: u32, masks: (u32, u32, u32)) -> Visualtype {
        Visualtype {
            visual_id,
            class: VisualClass::TRUE_COLOR,
            red_mask: masks.0,
            green_mask: masks.1,
            blue_mask: masks.2,
            ..Visualtype::default()
        }
    }

    const RGB: (u32, u32, u32) = (0xff_0000, 0xff00, 0xff);

    fn server(root_visual: u32, depths: Vec<(u8, Vec<Visualtype>)>) -> (Setup, Screen) {
        let native = if cfg!(target_endian = "little") {
            ImageOrder::LSB_FIRST
        } else {
            ImageOrder::MSB_FIRST
        };
        let format = |depth, bits_per_pixel| Format {
            depth,
            bits_per_pixel,
            scanline_pad: 32,
        };
        let setup = Setup {
            image_byte_order: native,
            pixmap_formats: vec![format(16, 16), format(24, 32), format(32, 32)],
            ..Setup::default()
        };
        let screen = Screen {
            root_visual,
            allowed_depths: depths
                .into_iter()
                .map(|(depth, visuals)| Depth { depth, visuals })
                .collect(),
            ..Screen::default()
        };
        (setup, screen)
    }

    #[test]
    fn the_visual_is_one_whose_pixels_are_the_canvas_words() {
        let (setup, screen) = server(0x21, vec![(24, vec![true_color(0x21, RGB)])]);
        assert_eq!(visual(&setup, &screen), Some((0x21, 24)));
        // a root of 16 bits, with depth 32 and 24 offered: 24 before 32
        let (setup, screen) = server(
            0x21,
            vec![
                (16, vec![true_color(0x21, (0xf800, 0x7e0, 0x1f))]),
                (32, vec![true_color(0x40, RGB)]),
                (24, vec![true_color(0x30, RGB)]),
            ],
        );
        assert_eq!(visual(&setup, &screen), Some((0x30, 24)));
        let (setup, screen) = server(0x21, vec![(32, vec![true_color(0x40, RGB)])]);
        assert_eq!(visual(&setup, &screen), Some((0x40, 32)));
        // the root's own before another of depth 24
        let (setup, screen) = server(
            0x22,
            vec![(24, vec![true_color(0x21, RGB), true_color(0x22, RGB)])],
        );
        assert_eq!(visual(&setup, &screen), Some((0x22, 24)));
    }

    #[test]
    fn no_visual_where_the_pixels_differ_from_the_canvas() {
        let bgr = (0xff, 0xff00, 0xff_0000);
        let (setup, screen) = server(0x21, vec![(24, vec![true_color(0x21, bgr)])]);
        assert_eq!(visual(&setup, &screen), None, "blue in the high byte");
        let mut direct = true_color(0x21, RGB);
        direct.class = VisualClass::DIRECT_COLOR;
        let (setup, screen) = server(0x21, vec![(24, vec![direct])]);
        assert_eq!(visual(&setup, &screen), None, "DirectColor");
        let (mut setup, screen) = server(0x21, vec![(24, vec![true_color(0x21, RGB)])]);
        setup.pixmap_formats = vec![Format {
            depth: 24,
            bits_per_pixel: 24,
            scanline_pad: 32,
        }];
        assert_eq!(visual(&setup, &screen), None, "three bytes a pixel");
        let (mut setup, screen) = server(0x21, vec![(24, vec![true_color(0x21, RGB)])]);
        setup.image_byte_order = if cfg!(target_endian = "little") {
            ImageOrder::MSB_FIRST
        } else {
            ImageOrder::LSB_FIRST
        };
        assert_eq!(visual(&setup, &screen), None, "the other byte order");
    }

    #[test]
    fn a_frame_goes_in_pieces_of_whole_rows_within_the_request_size() {
        // three rows of 100 pixels fit
        let three: Vec<_> = pieces(100, 10, 28 + 3 * 400 + 399).collect();
        assert_eq!(three, [(0, 3), (3, 3), (6, 3), (9, 1)]);
        assert_eq!(pieces(100, 10, 1 << 24).collect::<Vec<_>>(), [(0, 10)]);
        assert_eq!(pieces(100, 0, 1 << 20).count(), 0);
        // 65535 * 4 bytes, the most without BIG-REQUESTS
        let rows: u32 = pieces(1920, 1080, 262_140).map(|(_, n)| u32::from(n)).sum();
        assert_eq!(rows, 1080);
        assert!(
            pieces(1920, 1080, 262_140).all(|(_, n)| 28 + usize::from(n) * 1920 * 4 <= 262_140)
        );
    }

    #[test]
    fn a_row_too_wide_for_a_request_still_goes_whole_which_is_the_limit() {
        assert_eq!(
            pieces(100, 2, 100).collect::<Vec<_>>(),
            [(0, 1), (1, 1)],
            "the limit: one row a request, though it passes the request size"
        );
    }

    #[test]
    fn a_frame_whose_bytes_pass_the_limit_is_refused() {
        let most = isize::MAX as usize;
        assert_eq!(frame_size(800, 600, most), Some((480_000, 1_920_000)));
        assert_eq!(frame_size(0, 600, most), Some((0, 0)));
        assert_eq!(
            frame_size(u16::MAX, u16::MAX, most).map(|(_, bytes)| bytes as u64),
            (usize::BITS > 32).then_some(4 * 65535 * 65535),
            "on 32 bits the bytes overflow"
        );
        // where usize is 32 bits, as isize::MAX is 2^31 - 1 there
        let most_32 = i32::MAX as usize;
        assert_eq!(frame_size(40_000, 30_000, most_32), None, "4.8 GB");
        // 23170 * 23170 * 4 is 2147395600, under 2^31 - 1; one more row passes it
        assert_eq!(
            frame_size(23_170, 23_170, most_32),
            Some((536_848_900, 2_147_395_600))
        );
        assert_eq!(frame_size(23_170, 23_171, most_32), None);
        assert_eq!(frame_size(2, 2, 15), None, "one byte short");
        assert_eq!(frame_size(2, 2, 16), Some((4, 16)));
    }
}
