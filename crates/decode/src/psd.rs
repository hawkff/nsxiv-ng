//! Photoshop documents, PSD and its large variant PSB: the composite image a file ends with,
//! which Photoshop writes while Maximize Compatibility is on. The layers are not read.
//!
//! Every offset is checked against the file's length before a read, and nothing is allocated
//! by a field before it is checked against the file: the color table, the ICC profile and the
//! row byte counts lie inside it. The pixels come after the caller checked their size.

use std::io::{Read, Seek, SeekFrom};

use image::error::{DecodingError, ImageFormatHint, UnsupportedError, UnsupportedErrorKind};
use image::{ColorType, ImageDecoder, ImageError, ImageResult};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};

/// Image resources, and tagged blocks after the layers, read before the rest is given up;
/// real files have a few dozen.
const MAX_BLOCKS: usize = 256;

/// The tagged blocks whose length takes eight bytes in a PSB, as the specification lists them.
const LONG_KEYS: [&[u8; 4]; 13] = [
    b"LMsk", b"Lr16", b"Lr32", b"Layr", b"Mt16", b"Mt32", b"Mtrn", b"Alph", b"FMsk", b"lnk2",
    b"FEid", b"FXid", b"PxSD",
];

/// Whether a file that starts so is a PSD or PSB.
pub(crate) fn is_psd(start: &[u8]) -> bool {
    start.starts_with(b"8BPS") && matches!(start.get(4..6), Some([0, 1] | [0, 2]))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Bitmap,
    /// Grayscale, and duotone, whose composite is gray.
    Gray,
    Indexed,
    Rgb,
    Cmyk,
}

pub(crate) struct PsdDecoder<R> {
    reader: R,
    /// The file's length.
    len: u64,
    width: u32,
    height: u32,
    /// PSB, whose row byte counts take four bytes.
    big: bool,
    channels: u16,
    sixteen: bool,
    mode: Mode,
    /// Indexed color's table, 256 reds, then the greens, then the blues.
    palette: Vec<u8>,
    /// The index shown transparent, for indexed color.
    transparent: Option<u8>,
    /// Whether the first channel past the colors is the composite's transparency.
    alpha: bool,
    rle: bool,
    /// Where the image data starts, past its compression method.
    data: u64,
    icc: Option<Vec<u8>>,
}

impl<R: Read + Seek> PsdDecoder<R> {
    /// The bytes it keeps for itself beside the pixels, both for a CMYK document: its profile,
    /// which it converts with rather than handing it over, and its alpha, a byte a pixel, which
    /// waits apart from the inks until they are converted.
    pub(crate) fn kept(&self) -> u64 {
        if self.mode != Mode::Cmyk {
            return 0;
        }
        let profile = self.icc.as_ref().map_or(0, |icc| icc.len() as u64);
        let alpha = if self.alpha {
            u64::from(self.width) * u64::from(self.height)
        } else {
            0
        };
        profile + alpha
    }

    /// Reads the sections before the composite: the header, indexed color's table, the image
    /// resources with the ICC profile, and from the layers only whether the composite has
    /// transparency.
    pub(crate) fn new(mut reader: R) -> ImageResult<PsdDecoder<R>> {
        let len = reader.seek(SeekFrom::End(0))?;
        reader.rewind()?;
        let mut header = [0; 26];
        reader.read_exact(&mut header)?;
        if !is_psd(&header) {
            return Err(error("not a Photoshop document"));
        }
        let short = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
        let long = |at: usize| {
            u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        let big = short(4) == 2;
        let (channels, height, width) = (short(12), long(14), long(18));
        let (depth, mode) = (short(22), short(24));
        let most = if big { 300_000 } else { 30_000 };
        if !(1..=56).contains(&channels)
            || !(1..=most).contains(&width)
            || !(1..=most).contains(&height)
        {
            return Err(error("a size or channel count out of range"));
        }
        let mode = match (mode, depth) {
            (0, 1) => Mode::Bitmap,
            // the specification has readers show duotone as the gray it stores
            (1 | 8, 8 | 16) => Mode::Gray,
            (2, 8) => Mode::Indexed,
            (3, 8 | 16) => Mode::Rgb,
            (4, 8 | 16) => Mode::Cmyk,
            // shortcut: 32-bit, Lab and multichannel documents are refused by name; read them
            // once such files turn up
            (_, 32) => return Err(unsupported("32 bits a channel")),
            (7, _) => return Err(unsupported("multichannel mode")),
            (9, _) => return Err(unsupported("Lab color")),
            _ => return Err(error(format!("mode {mode} at {depth} bits a channel"))),
        };
        if channels < colors(mode) as u16 {
            return Err(error("fewer channels than its colors take"));
        }
        let within = |at: u64| {
            (at <= len)
                .then_some(at)
                .ok_or_else(|| error("a section past the file's end"))
        };
        // the color mode data, which for indexed color is its table
        let resources = within(30 + u64::from(read_u32(&mut reader)?))?;
        let mut palette = Vec::new();
        if mode == Mode::Indexed {
            if resources < 30 + 768 {
                return Err(error("indexed color with no color table"));
            }
            palette = vec![0; 768];
            reader.read_exact(&mut palette)?;
        }
        reader.seek(SeekFrom::Start(resources))?;
        let layers = within(resources + 4 + u64::from(read_u32(&mut reader)?))?;
        let mut icc = None;
        let mut transparent = None;
        let mut merged = true;
        let mut at = resources + 4;
        for _ in 0..MAX_BLOCKS {
            // a signature, an ID, a name of at least its length byte and a pad, and a size
            if at + 12 > layers {
                break;
            }
            reader.seek(SeekFrom::Start(at))?;
            let mut head = [0; 7];
            reader.read_exact(&mut head)?;
            if &head[..4] != b"8BIM" {
                break;
            }
            let id = u16::from_be_bytes([head[4], head[5]]);
            // the name's length byte and the name, padded to an even length
            let size_at = at + 6 + (u64::from(head[6]) + 2) / 2 * 2;
            if size_at + 4 > layers {
                break;
            }
            reader.seek(SeekFrom::Start(size_at))?;
            let size = u64::from(read_u32(&mut reader)?);
            let end = size_at + 4 + size;
            if end > layers {
                break;
            }
            match id {
                1039 => {
                    let mut profile = vec![0; size as usize];
                    reader.read_exact(&mut profile)?;
                    icc = Some(profile);
                }
                1047 if size >= 2 => {
                    transparent = u8::try_from(read_u16(&mut reader)?).ok();
                }
                // the version info: a version, then whether the composite holds the image
                1057 if size >= 5 => {
                    let mut version = [0; 5];
                    reader.read_exact(&mut version)?;
                    merged = version[4] != 0;
                }
                _ => {}
            }
            at = end + (size & 1);
        }
        if !merged {
            return Err(error(
                "saved without its composite image, as with Maximize Compatibility off",
            ));
        }
        let length = |reader: &mut R| {
            if big {
                read_u64(reader)
            } else {
                read_u32(reader).map(u64::from)
            }
        };
        let width_of_length = if big { 8 } else { 4 };
        reader.seek(SeekFrom::Start(layers))?;
        let image = layers
            .saturating_add(width_of_length)
            .saturating_add(length(&mut reader)?);
        if image.saturating_add(2) > len {
            return Err(error("a section past the file's end"));
        }
        let mut alpha = false;
        if image > layers + width_of_length {
            // a negative layer count says the first alpha channel is the composite's
            // transparency
            let info = length(&mut reader)?;
            if info >= 2 {
                alpha = (read_u16(&mut reader)? as i16) < 0;
            }
            let mask = (layers + 2 * width_of_length).saturating_add(info);
            if mask.saturating_add(4) <= image {
                reader.seek(SeekFrom::Start(mask))?;
                let blocks = mask + 4 + u64::from(read_u32(&mut reader)?);
                alpha |= tagged_transparency(&mut reader, blocks, image, big)?;
            }
        }
        reader.seek(SeekFrom::Start(image))?;
        let rle = match read_u16(&mut reader)? {
            0 => false,
            1 => true,
            _ => return Err(unsupported("a ZIP-compressed composite")),
        };
        Ok(PsdDecoder {
            reader,
            len,
            width,
            height,
            big,
            channels,
            sixteen: depth == 16,
            mode,
            palette,
            transparent,
            alpha: alpha
                && matches!(mode, Mode::Gray | Mode::Rgb | Mode::Cmyk)
                && channels > colors(mode) as u16,
            rle,
            data: image + 2,
            icc,
        })
    }
}

/// Whether the tagged blocks between `at` and `end` say the composite has transparency: a
/// block that says so outright, or the layers of a 16-bit or 32-bit document, which are kept
/// here, with a negative count.
fn tagged_transparency(
    reader: &mut (impl Read + Seek),
    mut at: u64,
    end: u64,
    big: bool,
) -> ImageResult<bool> {
    for _ in 0..MAX_BLOCKS {
        if at + 12 > end {
            break;
        }
        reader.seek(SeekFrom::Start(at))?;
        let mut head = [0; 12];
        reader.read_exact(&mut head)?;
        // a block may start after up to three bytes that pad the one before
        let Some(skip) = (0..4).find(|&i| matches!(&head[i..i + 4], b"8BIM" | b"8B64")) else {
            break;
        };
        at += skip as u64;
        if at + 12 > end {
            break;
        }
        reader.seek(SeekFrom::Start(at + 4))?;
        let mut key = [0; 4];
        reader.read_exact(&mut key)?;
        let (size, body) = if big && LONG_KEYS.contains(&&key) {
            (read_u64(reader)?, at + 16)
        } else {
            (u64::from(read_u32(reader)?), at + 12)
        };
        let Some(next) = body.checked_add(size).filter(|&next| next <= end) else {
            break;
        };
        match &key {
            b"Mtrn" | b"Mt16" | b"Mt32" => return Ok(true),
            // the layer info with its length left out, so the count comes first
            b"Layr" | b"Lr16" | b"Lr32" if size >= 2 && (read_u16(reader)? as i16) < 0 => {
                return Ok(true);
            }
            _ => {}
        }
        at = next;
    }
    Ok(false)
}

/// The color channels a mode takes.
fn colors(mode: Mode) -> usize {
    match mode {
        Mode::Rgb => 3,
        Mode::Cmyk => 4,
        _ => 1,
    }
}

impl<R: Read + Seek> ImageDecoder for PsdDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        match (self.mode, self.sixteen, self.alpha) {
            (Mode::Bitmap, ..) | (Mode::Gray, false, false) => ColorType::L8,
            (Mode::Gray, false, true) => ColorType::La8,
            (Mode::Gray, true, false) => ColorType::L16,
            (Mode::Gray, true, true) => ColorType::La16,
            (Mode::Indexed, ..) if self.transparent.is_some() => ColorType::Rgba8,
            (Mode::Indexed, ..) | (Mode::Rgb, false, false) => ColorType::Rgb8,
            (Mode::Rgb, false, true) => ColorType::Rgba8,
            (Mode::Rgb, true, false) => ColorType::Rgb16,
            (Mode::Rgb, true, true) => ColorType::Rgba16,
            // converted to RGB here, at 8 bits, with its profile
            (Mode::Cmyk, ..) => ColorType::Rgba8,
        }
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(match self.mode {
            Mode::Gray | Mode::Indexed | Mode::Rgb => self.icc.take(),
            Mode::Bitmap | Mode::Cmyk => None,
        })
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        let (width, height) = (self.width as usize, self.height as usize);
        let pixel = self.color_type().bytes_per_pixel() as usize;
        let sample_bytes = if self.sixteen { 2 } else { 1 };
        let row_len = if self.mode == Mode::Bitmap {
            width.div_ceil(8)
        } else {
            width * sample_bytes
        };
        // 16-bit samples stay 16-bit, but for CMYK, which converts at 8
        let wide = self.sixteen && self.mode != Mode::Cmyk;
        let planes = colors(self.mode) + usize::from(self.alpha);
        let count_bytes: u64 = if self.big { 4 } else { 2 };
        let table = u64::from(self.channels) * height as u64 * count_bytes;
        // the row byte counts for every channel, then the rows
        let mut at = self.data + if self.rle { table } else { 0 };
        if at > self.len {
            return Err(error("the image data past the file's end"));
        }
        let mut counts = vec![
            0;
            if self.rle {
                height * count_bytes as usize
            } else {
                0
            }
        ];
        let mut row = vec![0; row_len];
        // CMYK's alpha, kept apart while the inks still fill each pixel
        let cmyk_alpha = self.mode == Mode::Cmyk && self.alpha;
        let mut apart = vec![0; if cmyk_alpha { width * height } else { 0 }];
        for plane in 0..planes {
            if self.rle {
                let first = self.data + plane as u64 * height as u64 * count_bytes;
                self.reader.seek(SeekFrom::Start(first))?;
                self.reader.read_exact(&mut counts)?;
            }
            self.reader.seek(SeekFrom::Start(at))?;
            for (y, out) in buf.chunks_exact_mut(width * pixel).enumerate() {
                if self.rle {
                    let count = if self.big {
                        let c = &counts[y * 4..y * 4 + 4];
                        u32::from_be_bytes([c[0], c[1], c[2], c[3]]) as usize
                    } else {
                        usize::from(u16::from_be_bytes([counts[y * 2], counts[y * 2 + 1]]))
                    };
                    // a writer may pack a row loosely, but not past the file's end
                    if at + count as u64 > self.len {
                        return Err(error("a row past the file's end"));
                    }
                    unpack(&mut self.reader, count as u64, &mut row)?;
                    at += count as u64;
                } else {
                    self.reader.read_exact(&mut row)?;
                }
                if cmyk_alpha && plane == 4 {
                    let out = &mut apart[y * width..(y + 1) * width];
                    place(&row, out, self.mode, 0, 1, sample_bytes, false);
                } else {
                    place(&row, out, self.mode, plane, pixel, sample_bytes, wide);
                }
            }
            if !self.rle {
                at += (height * row_len) as u64;
            }
        }
        // the writer blended CMYK with white in its own space, toward no ink, which conversion
        // does not keep once black ink or a profile is in play
        if self.mode == Mode::Cmyk {
            if cmyk_alpha {
                unmatte(buf, 4, false, Some(&apart));
            }
            cmyk_to_rgb(buf, width, self.icc.as_deref());
            for (pixel, &alpha) in buf.as_chunks_mut::<4>().0.iter_mut().zip(&apart) {
                pixel[3] = alpha;
            }
        }
        if self.mode == Mode::Indexed {
            for pixel in buf.chunks_exact_mut(pixel) {
                let index = usize::from(pixel[0]);
                pixel[0] = self.palette[index];
                pixel[1] = self.palette[256 + index];
                pixel[2] = self.palette[512 + index];
                if let Some(alpha) = pixel.get_mut(3) {
                    *alpha = if self.transparent == Some(index as u8) {
                        0
                    } else {
                        255
                    };
                }
            }
        }
        if self.alpha && self.mode != Mode::Cmyk {
            unmatte(buf, pixel, wide, None);
        }
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

/// Puts one row of a channel's samples into its place in each pixel of `out`.
fn place(
    row: &[u8],
    out: &mut [u8],
    mode: Mode,
    slot: usize,
    pixel: usize,
    sample_bytes: usize,
    wide: bool,
) {
    match (mode, sample_bytes, wide) {
        // a set bit is black
        (Mode::Bitmap, ..) => {
            for (x, value) in out.iter_mut().enumerate() {
                *value = if row[x / 8] & (0x80 >> (x % 8)) != 0 {
                    0
                } else {
                    255
                };
            }
        }
        (_, 1, _) => {
            for (value, &sample) in out.chunks_exact_mut(pixel).zip(row) {
                value[slot] = sample;
            }
        }
        // the high byte, for CMYK
        (_, _, false) => {
            for (value, sample) in out.chunks_exact_mut(pixel).zip(row.as_chunks::<2>().0) {
                value[slot] = sample[0];
            }
        }
        (_, _, true) => {
            for (value, sample) in out.chunks_exact_mut(pixel).zip(row.as_chunks::<2>().0) {
                let sample = u16::from_be_bytes([sample[0], sample[1]]);
                value[slot * 2..slot * 2 + 2].copy_from_slice(&sample.to_ne_bytes());
            }
        }
    }
}

/// Unpacks a PackBits row `count` bytes long from `reader` straight into `row`, which it must
/// fill, and reads past whatever a loose writer left after it.
fn unpack(reader: &mut impl Read, count: u64, row: &mut [u8]) -> ImageResult<()> {
    let mut packed = reader.take(count);
    let early = |_| error("a row ends early");
    let mut to = 0;
    while to < row.len() {
        let mut head = [0];
        packed.read_exact(&mut head).map_err(early)?;
        let (length, literal) = match head[0] as i8 {
            -128 => continue,
            n @ 0.. => (n as usize + 1, true),
            n => ((1 - isize::from(n)) as usize, false),
        };
        let Some(into) = row.get_mut(to..to + length) else {
            return Err(error("a row runs past its width"));
        };
        if literal {
            packed.read_exact(into).map_err(early)?;
        } else {
            packed.read_exact(&mut head).map_err(early)?;
            into.fill(head[0]);
        }
        to += length;
    }
    std::io::copy(&mut packed, &mut std::io::sink())?;
    Ok(())
}

/// Turns the CMYK in each pixel's four bytes into RGB with an opaque alpha, through the file's
/// CMYK profile where it has one. Photoshop stores 0 for full ink, the inverse of a profile's.
fn cmyk_to_rgb(buf: &mut [u8], width: usize, icc: Option<&[u8]>) {
    for value in buf.iter_mut() {
        *value = 255 - *value;
    }
    let transform = icc
        .and_then(|icc| ColorProfile::new_from_slice(icc).ok())
        .filter(|profile| profile.color_space == DataColorSpace::Cmyk)
        .and_then(|profile| {
            let srgb = ColorProfile::new_srgb();
            let options = TransformOptions::default();
            profile
                .create_transform_8bit(Layout::Rgba, &srgb, Layout::Rgba, options)
                .ok()
        });
    let mut row = vec![0; width * 4];
    for pixels in buf.chunks_exact_mut(width * 4) {
        row.copy_from_slice(pixels);
        if transform
            .as_ref()
            .is_none_or(|transform| transform.transform(&row, pixels).is_err())
        {
            // with no profile, ink takes away its share of the light
            for (pixel, cmyk) in pixels
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(row.as_chunks::<4>().0)
            {
                let light = |ink: u8| u32::from(255 - ink);
                for channel in 0..3 {
                    pixel[channel] = (light(cmyk[channel]) * light(cmyk[3]) / 255) as u8;
                }
            }
        }
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
}

/// Takes the white out of a composite whose colors were blended with it by their alpha.
///
/// Adobe's specification names the composite's transparency, the first channel past the
/// colors where the layer count is negative or a Mtrn, Mt16 or Mt32 block is present, but says
/// nothing of a matte. Photoshop blends with white: a red layer at 80% is stored as
/// (255, 51, 51) at alpha 204, and fully transparent pixels as white. GIMP 3.0.4's exporter
/// blends the same, "as Photoshop does", and its importer takes the white out. ImageMagick
/// 7.1.1 and Krita 5.2.9 store straight colors under the same marks. So the white comes out
/// only where the pixels could have been blended with it: every transparent one white, and
/// every partly transparent one with each color at least the full value less its alpha. The
/// alpha is each pixel's last sample, or for CMYK `apart` from the pixels, a byte each, whose
/// four inks are then the colors, stored with white as the full value.
// shortcut: straight colors whose transparent pixels are all white and whose partial ones are
// all at least that light read as blended, and darken by up to a quarter of the range once
// shown over a background; resource 1057 names the writer, if such files turn up
fn unmatte(buf: &mut [u8], pixel: usize, wide: bool, apart: Option<&[u8]>) {
    let (size, most) = if wide { (2, 0xffff) } else { (1, 0xff) };
    let get = |sample: &[u8]| {
        if wide {
            u64::from(u16::from_ne_bytes([sample[0], sample[1]]))
        } else {
            u64::from(sample[0])
        }
    };
    let colors = if apart.is_some() { pixel } else { pixel - size };
    let alpha =
        |i: usize, value: &[u8]| apart.map_or_else(|| get(&value[colors..]), |a| u64::from(a[i]));
    let blended = buf.chunks_exact(pixel).enumerate().all(|(i, value)| {
        let alpha = alpha(i, value);
        value[..colors].chunks_exact(size).all(|color| match alpha {
            0 => get(color) == most,
            alpha => get(color) + alpha >= most,
        })
    });
    if !blended {
        return;
    }
    for (i, value) in buf.chunks_exact_mut(pixel).enumerate() {
        let alpha = alpha(i, value);
        let colors = &mut value[..colors];
        if alpha == 0 || alpha == most {
            continue;
        }
        for color in colors.chunks_exact_mut(size) {
            let straight = ((get(color) + alpha - most) * most + alpha / 2) / alpha;
            if wide {
                color.copy_from_slice(&(straight as u16).to_ne_bytes());
            } else {
                color[0] = straight as u8;
            }
        }
    }
}

fn read_u16(reader: &mut impl Read) -> ImageResult<u16> {
    let mut bytes = [0; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> ImageResult<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> ImageResult<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

fn error(why: impl Into<String>) -> ImageError {
    ImageError::Decoding(DecodingError::new(
        ImageFormatHint::Name("PSD".to_owned()),
        why.into(),
    ))
}

fn unsupported(feature: &str) -> ImageError {
    ImageError::Unsupported(UnsupportedError::from_format_and_kind(
        ImageFormatHint::Name("PSD".to_owned()),
        UnsupportedErrorKind::GenericFeature(feature.to_owned()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fixtures")
            .join(name);
        std::fs::read(path).unwrap()
    }

    #[test]
    fn the_profile_is_handed_over_not_copied() {
        let mut decoder = PsdDecoder::new(Cursor::new(fixture("psd-p3.psd"))).unwrap();
        assert!(decoder.icc_profile().unwrap().is_some());
        assert!(decoder.icc.is_none());
        assert_eq!(decoder.kept(), 0);
    }

    #[test]
    fn a_layer_info_length_at_its_most_adds_up_without_overflow() {
        // the PSB's layer info length, past its layer section's own, as large as it goes
        let mut file = fixture("psd-gray16.psb");
        file[42..50].copy_from_slice(&[0xff; 8]);
        assert!(PsdDecoder::new(Cursor::new(file)).is_ok());
    }
}
