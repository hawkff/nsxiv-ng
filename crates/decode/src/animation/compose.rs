//! The frames of APNG and animated WebP files, composed here, as the image crate composes
//! both wrongly: its APNG decoder saves the canvas for previous disposal before it clears the
//! area of a frame with background disposal, and image-webp ignores background disposal
//! unless given a background color, and then clears the whole canvas under a frame that fills
//! it. GIF composes right there and stays with it.

use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};

use image::codecs::webp::WebPDecoder;
use image::error::{DecodingError, ImageFormatHint};
use image::{
    Delay, DynamicImage, Frame, Frames, ImageDecoder, ImageError, ImageResult, Limits, RgbaImage,
};

/// What a frame leaves of its area once the next one comes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dispose {
    /// The frame stays.
    Keep,
    /// Its area turns transparent.
    Background,
    /// Its area goes back to what it was before the frame.
    Previous,
}

/// Where a frame goes, how it meets the canvas, and what it leaves behind.
#[derive(Clone, Copy, Debug)]
pub(super) struct Placement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The frame blends over the canvas, or else replaces what it covers.
    pub over: bool,
    pub dispose: Dispose,
}

/// The canvas frames compose onto, transparent at first.
pub(super) struct Canvas {
    image: RgbaImage,
    /// The frame drawn last, whose disposal comes before the next one, with what its area held
    /// before it where it goes back to that.
    last: Option<(Placement, Vec<u8>)>,
}

impl Canvas {
    pub(super) fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            image: RgbaImage::new(width, height),
            last: None,
        }
    }

    /// Whether a frame so placed lies inside the canvas.
    pub(super) fn holds(&self, placement: Placement) -> bool {
        let inside = |start: u32, length: u32, end: u32| {
            start.checked_add(length).is_some_and(|stop| stop <= end)
        };
        let (width, height) = self.image.dimensions();
        placement.width > 0
            && placement.height > 0
            && inside(placement.x, placement.width, width)
            && inside(placement.y, placement.height, height)
    }

    /// Disposes of the frame before as it said, then draws this frame's straight RGBA pixels,
    /// rows top to bottom, and gives a copy of the canvas they make, once they are gone.
    pub(super) fn draw(&mut self, rgba: Vec<u8>, placement: Placement) -> ImageResult<RgbaImage> {
        let pixels = u64::from(placement.width) * u64::from(placement.height);
        if !self.holds(placement) || rgba.len() as u64 != pixels * 4 {
            return Err(malformed("a frame outside its canvas"));
        }
        if let Some((last, saved)) = self.last.take() {
            let rows = rows(&mut self.image, last);
            match last.dispose {
                Dispose::Keep => {}
                Dispose::Background => rows.for_each(|row| row.fill(0)),
                Dispose::Previous => {
                    let saved = saved.chunks_exact(last.width as usize * 4);
                    rows.zip(saved)
                        .for_each(|(row, saved)| row.copy_from_slice(saved));
                }
            }
        }
        // saved after the disposal above, so it holds what this frame covered, in a vector of
        // its exact size, as the memory cap counts it
        let mut saved = Vec::new();
        if placement.dispose == Dispose::Previous {
            saved.reserve_exact(rgba.len());
            rows(&mut self.image, placement).for_each(|row| saved.extend_from_slice(row));
        }
        let frame = rgba.chunks_exact(placement.width as usize * 4);
        for (row, frame) in rows(&mut self.image, placement).zip(frame) {
            if placement.over {
                let under = row.as_chunks_mut::<4>().0;
                for (under, &top) in under.iter_mut().zip(frame.as_chunks::<4>().0) {
                    *under = over(top, *under);
                }
            } else {
                row.copy_from_slice(frame);
            }
        }
        self.last = Some((placement, saved));
        drop(rgba);
        Ok(self.image.clone())
    }
}

// The rows of the placed area, which the canvas holds.
fn rows(image: &mut RgbaImage, placement: Placement) -> impl Iterator<Item = &mut [u8]> {
    let stride = image.width() as usize * 4;
    let (start, end) = (
        placement.x as usize * 4,
        (placement.x + placement.width) as usize * 4,
    );
    (image.chunks_exact_mut(stride))
        .skip(placement.y as usize)
        .take(placement.height as usize)
        .map(move |row| &mut row[start..end])
}

// A straight-alpha pixel over another, rounded: an opaque one hides what is under it, and a
// transparent one leaves it as it is.
fn over(top: [u8; 4], under: [u8; 4]) -> [u8; 4] {
    let (top_alpha, under_alpha) = (u32::from(top[3]), u32::from(under[3]));
    // in 255ths of 255: what shows of the pixel under, and the alpha of both
    let shown = under_alpha * (255 - top_alpha);
    let alpha = top_alpha * 255 + shown;
    if alpha == 0 {
        return [0; 4];
    }
    let channel = |c: usize| {
        let sum = u32::from(top[c]) * top_alpha * 255 + u32::from(under[c]) * shown;
        ((sum + alpha / 2) / alpha) as u8
    };
    [
        channel(0),
        channel(1),
        channel(2),
        ((alpha + 127) / 255) as u8,
    ]
}

fn malformed(why: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormatHint::Unknown, why))
}

// Frames from `next` until it gives None or an error, the error included.
fn frames(mut next: impl FnMut() -> ImageResult<Option<Frame>> + 'static) -> Frames<'static> {
    let mut failed = false;
    Frames::new(Box::new(std::iter::from_fn(move || {
        if failed {
            return None;
        }
        let frame = next().transpose();
        failed = matches!(frame, Some(Err(_)));
        frame
    })))
}

/// An APNG's frames, composed, from a reader at the start of the file. A default image the
/// animation does not count is passed over. `max_bytes` caps what the PNG decoder allocates.
pub(super) fn apng(
    reader: impl BufRead + Seek + 'static,
    max_bytes: u64,
) -> ImageResult<Frames<'static>> {
    let limits = png::Limits {
        bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX),
    };
    let mut decoder = png::Decoder::new_with_limits(reader, limits);
    // 8 or 16 bits, gray and alpha or RGBA, whatever the file holds
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(malformed)?;
    let info = reader.info();
    let mut left = info
        .animation_control()
        .map_or(0, |control| control.num_frames);
    let mut default_image = info.frame_control().is_none();
    let (width, height) = info.size();
    // interlaced frames come out at the whole image's stride, the others packed
    let whole_stride = if info.interlaced {
        Some(
            reader
                .output_line_size(width)
                .ok_or_else(|| malformed("too wide"))?,
        )
    } else {
        None
    };
    let frame_bytes = reader.output_buffer_size();
    let mut raw = vec![0; frame_bytes.ok_or_else(|| malformed("too large"))?];
    let mut canvas = Canvas::new(width, height);
    Ok(frames(move || {
        if left == 0 {
            return Ok(None);
        }
        left -= 1;
        if default_image {
            default_image = false;
            reader.next_frame(&mut raw).map_err(malformed)?;
        }
        let output = reader.next_frame(&mut raw).map_err(malformed)?;
        let control = *reader
            .info()
            .frame_control()
            .ok_or_else(|| malformed("no fcTL"))?;
        let mut line = output.line_size;
        let mut stride = whole_stride.unwrap_or(line);
        if line == 0 {
            return Err(malformed("an empty frame"));
        }
        if output.bit_depth == png::BitDepth::Sixteen {
            // rounded to 8 bits, as a still PNG decodes, in place; the high byte would be 1 off
            let used = stride.saturating_mul(output.height as usize).min(raw.len());
            for i in 0..used / 2 {
                let sample = u16::from_be_bytes([raw[2 * i], raw[2 * i + 1]]);
                raw[i] = ((u32::from(sample) + 128) / 257) as u8;
            }
            (line, stride) = (line / 2, stride / 2);
        }
        // in a vector of its exact size, as the memory cap counts it
        let pixels = output.width as usize * output.height as usize;
        let mut rgba = Vec::with_capacity(pixels.saturating_mul(4));
        for row in raw.chunks_exact(stride).take(output.height as usize) {
            match output.color_type {
                png::ColorType::Rgba => rgba.extend_from_slice(&row[..line]),
                png::ColorType::GrayscaleAlpha => {
                    let pairs = row[..line].as_chunks::<2>().0;
                    rgba.extend(pairs.iter().flat_map(|&[l, a]| [l, l, l, a]));
                }
                _ => return Err(malformed("a color type the expansion does not give")),
            }
        }
        let placement = Placement {
            x: control.x_offset,
            y: control.y_offset,
            width: control.width,
            height: control.height,
            over: control.blend_op == png::BlendOp::Over,
            dispose: match control.dispose_op {
                png::DisposeOp::None => Dispose::Keep,
                png::DisposeOp::Background => Dispose::Background,
                // the first frame's goes back to the transparent canvas, as background does
                png::DisposeOp::Previous => Dispose::Previous,
            },
        };
        let image = canvas.draw(rgba, placement)?;
        // a ratio in seconds, where a denominator of 0 means 100
        let denominator = match control.delay_den {
            0 => 100,
            d => u32::from(d),
        };
        let delay = Delay::from_numer_denom_ms(u32::from(control.delay_num) * 1000, denominator);
        Ok(Some(Frame::from_parts(image, 0, 0, delay)))
    }))
}

/// An animated WebP's frames, composed over a transparent canvas of `size`, which the file's
/// header gave, from a reader anywhere in the file. The background color the file names is
/// only a hint, which libwebp and browsers ignore, as here. `max_bytes` caps a frame's
/// compressed data and its decoding, beside the canvas.
pub(super) fn webp(
    mut reader: impl Read + Seek + 'static,
    (width, height): (u32, u32),
    max_bytes: u64,
) -> ImageResult<Frames<'static>> {
    let mut header = [0; 12];
    reader.seek(SeekFrom::Start(0))?;
    reader.read_exact(&mut header)?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WEBP" {
        return Err(malformed("no WebP"));
    }
    // The chunks run from here to the end the RIFF header gives, and each must end within it
    // and within the file; bytes after the RIFF are not the image's. Positions are 64 bits
    // wide, which no run of 32-bit sizes within a file can overflow.
    let riff_end = 8 + u64::from(u32::from_le_bytes([
        header[4], header[5], header[6], header[7],
    ]));
    let end = riff_end.min(reader.seek(SeekFrom::End(0))?);
    let mut at = 12u64;
    let mut canvas = Canvas::new(width, height);
    Ok(frames(move || {
        while at < riff_end {
            let (kind, size, next) = chunk(&mut reader, at, end)?;
            let data = at + 8;
            at = next;
            if &kind == b"ANMF" {
                return webp_frame(&mut reader, data, size, &mut canvas, max_bytes).map(Some);
            }
        }
        Ok(None)
    }))
}

// The chunk at `at`: its kind, the size its header gives, and where the next one starts, past
// the byte of padding that follows an odd size. A chunk that does not end by `end` is an error.
fn chunk(reader: &mut (impl Read + Seek), at: u64, end: u64) -> ImageResult<([u8; 4], u32, u64)> {
    if at + 8 > end {
        return Err(malformed("a chunk cut short"));
    }
    let mut header = [0; 8];
    reader.seek(SeekFrom::Start(at))?;
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let next = at + 8 + u64::from(size) + u64::from(size % 2);
    if next > end {
        return Err(malformed("a chunk past the end of what holds it"));
    }
    Ok(([header[0], header[1], header[2], header[3]], size, next))
}

// The frame of the ANMF chunk whose `size` bytes of data start at `data`, composed.
fn webp_frame(
    reader: &mut (impl Read + Seek),
    data: u64,
    size: u32,
    canvas: &mut Canvas,
    max_bytes: u64,
) -> ImageResult<Frame> {
    if size < 16 {
        return Err(malformed("an ANMF chunk too short for its header"));
    }
    let mut header = [0; 16];
    reader.seek(SeekFrom::Start(data))?;
    reader.read_exact(&mut header)?;
    let field = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], 0]);
    let placement = Placement {
        x: field(0) * 2,
        y: field(3) * 2,
        width: field(6) + 1,
        height: field(9) + 1,
        over: header[15] & 2 == 0,
        dispose: if header[15] & 1 == 0 {
            Dispose::Keep
        } else {
            Dispose::Background
        },
    };
    if !canvas.holds(placement) {
        return Err(malformed("a frame outside its canvas"));
    }
    // the frame's own chunks: ALPH and VP8, or VP8L, are kept, two at most, and any other is
    // passed over unread, however large
    let (mut kept, mut kept_bytes) = (Vec::with_capacity(2), 0);
    let end = data + u64::from(size);
    let mut at = data + 16;
    while at < end {
        let (kind, _, next) = chunk(reader, at, end)?;
        if matches!(&kind, b"ALPH" | b"VP8 " | b"VP8L") {
            if kept.len() == 2 {
                return Err(malformed("a frame of more than one bitstream"));
            }
            kept.push((kind, at, next - at));
            kept_bytes += next - at;
        }
        at = next;
    }
    let kinds: Vec<&[u8; 4]> = kept.iter().map(|(kind, ..)| kind).collect();
    let alpha = match kinds[..] {
        [b"VP8 "] => false,
        [b"ALPH", b"VP8 "] | [b"VP8L"] => true,
        _ => return Err(malformed("a frame without one bitstream")),
    };
    // Those chunks become a still WebP of the frame's size behind an extended header, whose
    // alpha flag they set; image-webp refuses a bitstream of another size than that header's.
    // Beside the canvas, the still and about three frames' worth while it decodes (see
    // `open` in animation.rs) must fit the cap.
    let still_bytes = 30 + kept_bytes;
    let frame_bytes = u64::from(placement.width) * u64::from(placement.height) * 4;
    let canvas_bytes = canvas.image.len() as u64;
    if canvas_bytes + still_bytes + 3 * frame_bytes > max_bytes {
        return Err(malformed("a frame too large for the memory cap"));
    }
    let riff_size =
        u32::try_from(still_bytes - 8).map_err(|_| malformed("a frame too large for RIFF"))?;
    let mut still = Vec::with_capacity(still_bytes as usize);
    still.extend_from_slice(b"RIFF");
    still.extend_from_slice(&riff_size.to_le_bytes());
    still.extend_from_slice(b"WEBPVP8X\x0a\0\0\0");
    still.extend_from_slice(&[if alpha { 0x10 } else { 0 }, 0, 0, 0]);
    still.extend_from_slice(&(placement.width - 1).to_le_bytes()[..3]);
    still.extend_from_slice(&(placement.height - 1).to_le_bytes()[..3]);
    for &(_, at, length) in &kept {
        let from = still.len();
        still.resize(from + length as usize, 0);
        reader.seek(SeekFrom::Start(at))?;
        reader.read_exact(&mut still[from..])?;
    }
    let mut decoder = WebPDecoder::new(Cursor::new(still))?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_bytes);
    decoder.set_limits(limits)?;
    if decoder.dimensions() != (placement.width, placement.height) {
        return Err(malformed("a frame of another size than its header gives"));
    }
    let rgba = DynamicImage::from_decoder(decoder)?.into_rgba8().into_raw();
    let image = canvas.draw(rgba, placement)?;
    let delay = Delay::from_numer_denom_ms(field(12), 1);
    Ok(Frame::from_parts(image, 0, 0, delay))
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: [u8; 4] = [255, 0, 0, 255];
    const G: [u8; 4] = [0, 255, 0, 255];
    const B: [u8; 4] = [0, 0, 255, 255];
    const CLEAR: [u8; 4] = [0; 4];
    const HALF: [u8; 4] = [255, 255, 255, 128];

    fn place(x: u32, y: u32, width: u32, height: u32, over: bool, dispose: Dispose) -> Placement {
        Placement {
            x,
            y,
            width,
            height,
            over,
            dispose,
        }
    }

    fn solid(color: [u8; 4], pixels: usize) -> Vec<u8> {
        color.repeat(pixels)
    }

    fn pixels(image: ImageResult<RgbaImage>) -> Vec<[u8; 4]> {
        image.unwrap().as_chunks::<4>().0.to_vec()
    }

    #[test]
    fn disposal_clears_or_restores_the_area_of_the_frame_before() {
        use Dispose::*;
        let mut canvas = Canvas::new(3, 2);
        let full = canvas.draw(solid(R, 6), place(0, 0, 3, 2, false, Keep));
        assert_eq!(pixels(full), vec![R; 6], "a frame that fills it");
        let shown = canvas.draw(solid(G, 2), place(0, 0, 2, 1, true, Background));
        assert_eq!(pixels(shown), vec![G, G, R, R, R, R]);
        // blue across the area just cleared, which previous disposal puts back cleared
        let shown = canvas.draw(solid(B, 4), place(1, 0, 2, 2, false, Previous));
        assert_eq!(pixels(shown), vec![CLEAR, B, B, R, B, B]);
        let shown = canvas.draw(solid(CLEAR, 1), place(2, 1, 1, 1, true, Keep));
        assert_eq!(pixels(shown), vec![CLEAR, CLEAR, R, R, R, R]);
        // a first frame that goes back to what was before it leaves the canvas transparent
        let mut first = Canvas::new(1, 1);
        first
            .draw(solid(R, 1), place(0, 0, 1, 1, false, Previous))
            .unwrap();
        let shown = first.draw(solid(CLEAR, 1), place(0, 0, 1, 1, true, Keep));
        assert_eq!(pixels(shown), vec![CLEAR]);
    }

    #[test]
    fn blending_over_mixes_by_alpha_and_source_replaces() {
        let mut canvas = Canvas::new(2, 1);
        canvas
            .draw(solid(R, 2), place(0, 0, 2, 1, false, Dispose::Keep))
            .unwrap();
        let shown = canvas.draw(
            [HALF, CLEAR].concat(),
            place(0, 0, 2, 1, true, Dispose::Keep),
        );
        assert_eq!(pixels(shown), vec![[255, 128, 128, 255], R]);
        let shown = canvas.draw(
            [HALF, CLEAR].concat(),
            place(0, 0, 2, 1, false, Dispose::Keep),
        );
        assert_eq!(pixels(shown), vec![HALF, CLEAR]);
        // half blue over half red: alpha 0.75, a third of it red's
        assert_eq!(over([0, 0, 255, 128], [255, 0, 0, 128]), [85, 0, 170, 192]);
    }

    #[test]
    fn frames_outside_the_canvas_or_of_another_size_are_refused() {
        let mut canvas = Canvas::new(2, 2);
        for (pixels, placement) in [
            (2, place(1, 0, 2, 1, false, Dispose::Keep)),
            (2, place(u32::MAX, 0, 2, 1, false, Dispose::Keep)),
            (1, place(0, 0, 2, 1, false, Dispose::Keep)),
            (0, place(0, 0, 0, 1, false, Dispose::Keep)),
        ] {
            assert!(
                canvas.draw(solid(R, pixels), placement).is_err(),
                "{placement:?}"
            );
        }
    }

    fn fixture(name: &str) -> Vec<u8> {
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures");
        std::fs::read(fixtures.join(name)).unwrap()
    }

    // the frames until the end or an error, and whether there was one
    fn all(frames: ImageResult<Frames<'static>>) -> (Vec<RgbaImage>, bool) {
        let Ok(frames) = frames else {
            return (Vec::new(), true);
        };
        let mut images = Vec::new();
        for frame in frames {
            match frame {
                Ok(frame) => images.push(frame.into_buffer()),
                Err(_) => return (images, true),
            }
        }
        (images, false)
    }

    #[test]
    fn a_file_cut_short_gives_its_first_frames_and_an_error() {
        let webp_frames = |bytes: &[u8]| all(webp(Cursor::new(bytes.to_vec()), (4, 1), 1 << 30));
        let apng_frames = |bytes: &[u8]| all(apng(Cursor::new(bytes.to_vec()), 1 << 30));
        for (name, frames) in [
            (
                "disposal.webp",
                &webp_frames as &dyn Fn(&[u8]) -> (Vec<RgbaImage>, bool),
            ),
            ("disposal.apng", &apng_frames),
        ] {
            let bytes = fixture(name);
            let (whole, failed) = frames(&bytes);
            assert_eq!((whole.len(), failed), (4, false), "{name}");
            for cut in 0..bytes.len() {
                let (some, failed) = frames(&bytes[..cut]);
                // the APNG's last frame is whole before its file ends
                let ends = name == "disposal.apng" && some.len() == 4;
                assert!(
                    (failed || ends) && whole.starts_with(&some),
                    "{name} cut at {cut}: {} frames, failed {failed}",
                    some.len()
                );
            }
        }
    }

    #[test]
    fn a_webp_that_lies_about_sizes_gives_fewer_frames() {
        let bytes = fixture("disposal.webp");
        let anmf: Vec<usize> = (bytes.windows(4).enumerate())
            .filter(|(_, fourcc)| fourcc == b"ANMF")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(anmf.len(), 4);
        let lie = |at: usize, value: &[u8]| {
            let mut bytes = bytes.clone();
            bytes[at..at + value.len()].copy_from_slice(value);
            let (frames, failed) = all(webp(Cursor::new(bytes), (4, 1), 1 << 30));
            (frames.len(), failed)
        };
        // the first chunk says it is larger than the cap, then larger than the file
        assert_eq!(lie(anmf[0] + 4, &0xffff_fff0u32.to_le_bytes()), (0, true));
        assert_eq!(lie(anmf[0] + 4, &0x0fff_fff0u32.to_le_bytes()), (0, true));
        // the third frame, at x 2, three pixels wide, past the canvas
        assert_eq!(lie(anmf[2] + 14, &[2, 0, 0]), (2, true));
        // the second frame three pixels wide, where its bitstream has two
        assert_eq!(lie(anmf[1] + 14, &[2, 0, 0]), (1, true));
        // the RIFF ends before the first frame
        assert_eq!(lie(4, &4u32.to_le_bytes()), (0, false));
    }

    // a chunk as a file holds it: kind, size, data and the padding an odd size takes
    fn chunk_bytes(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let padding: &[u8] = if data.len() % 2 == 1 { &[0] } else { &[] };
        [&kind[..], &(data.len() as u32).to_le_bytes(), data, padding].concat()
    }

    // a WebP of these chunks, and the chunks of one
    fn riff(chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let body: Vec<u8> = (chunks.iter())
            .flat_map(|(kind, data)| chunk_bytes(kind, data))
            .collect();
        let size = (body.len() as u32 + 4).to_le_bytes();
        [&b"RIFF"[..], &size, b"WEBP", &body].concat()
    }

    fn chunks(bytes: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let (mut chunks, mut at) = (Vec::new(), 12);
        while at < bytes.len() {
            let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let kind = bytes[at..at + 4].try_into().unwrap();
            chunks.push((kind, bytes[at + 8..at + 8 + size].to_vec()));
            at += 8 + size + size % 2;
        }
        chunks
    }

    fn webp_frames(bytes: Vec<u8>, max_bytes: u64) -> (Vec<RgbaImage>, bool) {
        all(webp(Cursor::new(bytes), (4, 1), max_bytes))
    }

    #[test]
    fn a_webp_frame_keeps_its_bitstream_and_passes_over_other_chunks_unread() {
        let parts = chunks(&fixture("disposal.webp"));
        assert_eq!(parts.iter().filter(|(kind, _)| kind == b"ANMF").count(), 4);
        // a cap of 1 MiB, which holds the frames, but not 4 MiB of an unknown chunk
        const CAP: u64 = 1 << 20;
        let (whole, failed) = webp_frames(riff(&parts), CAP);
        assert_eq!((whole.len(), failed), (4, false));
        // The second frame, green and a transparent pixel, gets an unknown chunk of 4 MiB before
        // its bitstream and one of an odd size after: the frames come out the same, so the
        // first did not count against the cap, and the bitstream that comes after it still
        // sets the alpha flag.
        let mut padded = parts.clone();
        let data = &parts[3].1;
        padded[3].1 = [
            &data[..16],
            &chunk_bytes(b"XYZW", &vec![7; 4 << 20]),
            &data[16..],
            &chunk_bytes(b"ABCD", b"odd"),
        ]
        .concat();
        assert_eq!(webp_frames(riff(&padded), CAP), (whole.clone(), false));
        // a third frame whose bitstream chunk runs on for 4 MiB composes under a cap that holds
        // it, and under one that does not, the animation ends there
        let mut long = parts.clone();
        let data = &parts[4].1;
        let bitstream = chunks(&[&[0; 12][..], &data[16..]].concat()).remove(0);
        assert_eq!(&bitstream.0, b"VP8L");
        let longer = [&bitstream.1[..], &vec![0; 4 << 20]].concat();
        long[4].1 = [&data[..16], &chunk_bytes(b"VP8L", &longer)].concat();
        assert_eq!(webp_frames(riff(&long), 8 << 20), (whole.clone(), false));
        assert_eq!(webp_frames(riff(&long), CAP), (whole[..2].to_vec(), true));
    }

    #[test]
    fn a_webp_frame_without_one_bitstream_ends_the_frames_with_an_error() {
        let parts = chunks(&fixture("disposal.webp"));
        let (whole, _) = webp_frames(riff(&parts), 1 << 30);
        // the third frame's header, with its VP8L chunk, and chunks of the other kinds that
        // are refused before they decode
        let data = &parts[4].1;
        let (header, vp8l) = (&data[..16], &data[16..]);
        let alph = chunk_bytes(b"ALPH", &[0; 4]);
        let vp8 = chunk_bytes(b"VP8 ", &[0; 10]);
        for (what, chunks) in [
            ("no bitstream", chunk_bytes(b"XYZW", &[0; 4])),
            ("two bitstreams", [vp8l, vp8l].concat()),
            ("ALPH before VP8L", [&alph[..], vp8l].concat()),
            ("ALPH after VP8", [&vp8[..], &alph].concat()),
            ("a third chunk to keep", [&alph[..], &alph, &vp8].concat()),
        ] {
            let mut frames = parts.clone();
            frames[4].1 = [header, &chunks].concat();
            assert_eq!(
                webp_frames(riff(&frames), 1 << 30),
                (whole[..2].to_vec(), true),
                "{what}"
            );
        }
    }

    #[test]
    fn a_webp_chunk_past_its_riff_or_its_file_ends_the_frames_with_an_error() {
        let bytes = fixture("disposal.webp");
        let (whole, _) = webp_frames(bytes.clone(), 1 << 30);
        let anmf: Vec<usize> = (bytes.windows(4).enumerate())
            .filter(|(_, fourcc)| fourcc == b"ANMF")
            .map(|(at, _)| at)
            .collect();
        let with_riff_end = |bytes: &[u8], end: usize| {
            let mut bytes = bytes.to_vec();
            bytes[4..8].copy_from_slice(&(end as u32 - 8).to_le_bytes());
            webp_frames(bytes, 1 << 30)
        };
        // the RIFF ends inside the third frame's data, and right before it, which leaves the
        // rest of the file outside the image
        let two = whole[..2].to_vec();
        assert_eq!(with_riff_end(&bytes, anmf[2] + 18), (two.clone(), true));
        assert_eq!(with_riff_end(&bytes, anmf[2]), (two, false));
        let trailing = [&bytes[..], b"trailing bytes"].concat();
        assert_eq!(
            with_riff_end(&trailing, bytes.len()),
            (whole.clone(), false)
        );
        // after the frames, a chunk whose data runs past the end of the file, and a chunk
        // header the file cuts short, within a RIFF that runs on
        let past = [&bytes[..], b"XYZW", &1000u32.to_le_bytes(), &[0; 10]].concat();
        assert_eq!(
            with_riff_end(&past, past.len() + 990),
            (whole.clone(), true)
        );
        let cut = [&bytes[..], b"XYZW"].concat();
        assert_eq!(with_riff_end(&cut, cut.len() + 4), (whole, true));
    }

    #[test]
    fn a_16_bit_apng_animates_rounded_to_8_bits() {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder.set_animated(2, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        // 0x12ff rounds to 19, as a still PNG decodes
        for sample in [0x12ffu16, 0xffff] {
            writer
                .write_image_data(&sample.to_be_bytes().repeat(4))
                .unwrap();
        }
        writer.finish().unwrap();
        let (frames, failed) = all(apng(Cursor::new(bytes), 1 << 30));
        let frames: Vec<Vec<u8>> = frames.into_iter().map(RgbaImage::into_raw).collect();
        assert_eq!((frames, failed), (vec![vec![19; 4], vec![255; 4]], false));
    }
}
