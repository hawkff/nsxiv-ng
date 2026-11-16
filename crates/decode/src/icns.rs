//! Apple icon files: the largest icon an .icns holds. Icons stored as PNG or JPEG 2000 go to
//! those decoders; the older kinds, RGB run-length coded or as it stands with a separate mask,
//! and run-length ARGB, are unpacked here. The indexed kinds of before Mac OS X are not read.
//!
//! Every entry is checked against the file's length before it is read, and only the chosen
//! one is.

use std::io::{Read, Seek, SeekFrom};

use image::{ColorType, ImageDecoder, ImageResult};

use crate::DecodeError;

/// Entries read before the rest is given up; real files have a few dozen.
const MAX_ENTRIES: usize = 256;

/// A kind of icon: its type, the side it holds, and the type of its mask.
type Kind = (&'static [u8; 4], u32, Option<&'static [u8; 4]>);

/// The kinds of icon read, largest first, with the side each holds and the mask that goes with
/// the RGB ones. Of one side, the kinds that hold PNG or JPEG 2000 come first.
const KINDS: [Kind; 17] = [
    (b"ic10", 1024, None),
    (b"ic09", 512, None),
    (b"ic14", 512, None),
    (b"ic08", 256, None),
    (b"ic13", 256, None),
    (b"ic07", 128, None),
    (b"it32", 128, Some(b"t8mk")),
    (b"icp6", 64, None),
    (b"ic12", 64, None),
    (b"ih32", 48, Some(b"h8mk")),
    (b"ic11", 32, None),
    // PNG or JPEG 2000, or in older files RGB as il32's and is32's
    (b"icp5", 32, Some(b"l8mk")),
    // PNG, or ARGB
    (b"ic05", 32, None),
    (b"il32", 32, Some(b"l8mk")),
    (b"icp4", 16, Some(b"s8mk")),
    (b"ic04", 16, None),
    (b"is32", 16, Some(b"s8mk")),
];

/// Whether a file that starts so is an icon file.
pub(crate) fn is_icns(start: &[u8]) -> bool {
    start.starts_with(b"icns")
}

/// The largest icon, as the file stores it.
pub(crate) enum Icon {
    Png(Vec<u8>),
    Jpeg2000(Vec<u8>),
    Rgba(Pixels),
}

/// The largest icon in the file at `reader`.
pub(crate) fn largest(reader: &mut (impl Read + Seek)) -> Result<Icon, DecodeError> {
    let unreadable = |why: &str| DecodeError::Unreadable(format!("icon file: {why}"));
    let io = |e: std::io::Error| DecodeError::Unreadable(e.to_string());
    let len = reader.seek(SeekFrom::End(0)).map_err(io)?;
    reader.rewind().map_err(io)?;
    let mut head = [0; 8];
    reader.read_exact(&mut head).map_err(io)?;
    if !is_icns(&head) {
        return Err(unreadable("not an icon file"));
    }
    let end = u64::from(u32::from_be_bytes([head[4], head[5], head[6], head[7]])).min(len);
    // each entry's kind, and where its data lies and how long it is
    let mut entries = Vec::new();
    let mut at = 8;
    for _ in 0..MAX_ENTRIES {
        if at + 8 > end {
            break;
        }
        reader.seek(SeekFrom::Start(at)).map_err(io)?;
        reader.read_exact(&mut head).map_err(io)?;
        let size = u64::from(u32::from_be_bytes([head[4], head[5], head[6], head[7]]));
        if size < 8 || at + size > end {
            break;
        }
        let kind: [u8; 4] = [head[0], head[1], head[2], head[3]];
        entries.push((kind, at + 8, size - 8));
        at += size;
    }
    let find = |kind: &[u8; 4]| entries.iter().find(|entry| &entry.0 == kind);
    let Some((kind, side, mask, entry)) = KINDS
        .iter()
        .find_map(|&(kind, side, mask)| Some((kind, side, mask, find(kind)?)))
    else {
        return Err(unreadable("no icon of a kind known"));
    };
    let data = read(reader, entry).map_err(io)?;
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(Icon::Png(data));
    }
    if data.starts_with(b"\0\0\0\x0cjP  ") || data.starts_with(b"\xff\x4f\xff\x51") {
        return Ok(Icon::Jpeg2000(data));
    }
    if !matches!(
        kind,
        b"ic04" | b"ic05" | b"it32" | b"ih32" | b"il32" | b"is32" | b"icp4" | b"icp5"
    ) {
        return Err(unreadable("an icon neither PNG nor JPEG 2000"));
    }
    // at most 128 a side, as the kinds unpacked here hold
    let pixels = (side * side) as usize;
    let mut rgba = vec![255; pixels * 4];
    if matches!(kind, b"ic04" | b"ic05") {
        let planes = data
            .strip_prefix(b"ARGB")
            .and_then(|packed| unpack(packed, pixels * 4))
            .ok_or_else(|| unreadable("an ARGB icon that does not unpack"))?;
        let (alpha, rgb) = planes.split_at(pixels);
        for (i, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel[0] = rgb[i];
            pixel[1] = rgb[pixels + i];
            pixel[2] = rgb[2 * pixels + i];
            pixel[3] = alpha[i];
        }
    } else {
        // it32's data starts with four bytes of zeros
        let packed = if kind == b"it32" {
            data.get(4..).unwrap_or_default()
        } else {
            &data
        };
        if packed.len() == pixels * 3 {
            // exactly three bytes a pixel is RGB as it stands, as Pillow tells the two apart
            for (pixel, rgb) in rgba
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(packed.as_chunks::<3>().0)
            {
                pixel[..3].copy_from_slice(rgb);
            }
        } else {
            let planes = unpack(packed, pixels * 3)
                .ok_or_else(|| unreadable("an RGB icon that does not unpack"))?;
            for (i, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                pixel[0] = planes[i];
                pixel[1] = planes[pixels + i];
                pixel[2] = planes[2 * pixels + i];
            }
        }
        // the mask, one byte a pixel, where there is one of the right size
        if let Some(entry) = mask.and_then(find).filter(|e| e.2 == pixels as u64) {
            let mask = read(reader, entry).map_err(io)?;
            for (pixel, &alpha) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&mask) {
                pixel[3] = alpha;
            }
        }
    }
    Ok(Icon::Rgba(Pixels { side, rgba }))
}

/// An entry's data, which lies inside the file.
fn read(
    reader: &mut (impl Read + Seek),
    &(_, at, size): &([u8; 4], u64, u64),
) -> std::io::Result<Vec<u8>> {
    let mut data = vec![0; size as usize];
    reader.seek(SeekFrom::Start(at))?;
    reader.read_exact(&mut data)?;
    Ok(data)
}

/// Unpacks `length` bytes of Apple's icon run-length code: a byte under 128 heads that many
/// bytes and one more, as they are; a byte from 128 heads one byte, repeated 125 fewer times
/// than its value.
fn unpack(packed: &[u8], length: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(length);
    let mut bytes = packed.iter().copied();
    while out.len() < length {
        let head = bytes.next()?;
        if head & 0x80 == 0 {
            for _ in 0..=head {
                out.push(bytes.next()?);
            }
        } else {
            let value = bytes.next()?;
            out.extend(std::iter::repeat_n(value, usize::from(head) - 125));
        }
    }
    out.truncate(length);
    Some(out)
}

/// An icon unpacked here, as RGBA.
pub(crate) struct Pixels {
    side: u32,
    rgba: Vec<u8>,
}

impl Pixels {
    /// The bytes it holds.
    pub(crate) fn bytes(&self) -> u64 {
        self.rgba.len() as u64
    }
}

impl ImageDecoder for Pixels {
    fn dimensions(&self) -> (u32, u32) {
        (self.side, self.side)
    }

    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        buf.copy_from_slice(&self.rgba);
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}
