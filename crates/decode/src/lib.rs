//! Image decoding for nsxiv-ng: a file in, RGBA pixels out, oriented and in sRGB.
//!
//! The memory cap holds for the buffers `decode` allocates and for the pyramid a [`Loader`]
//! builds on any decoder's image, and reaches the image crate as its allocation limit, but
//! some decoders allocate outside that limit: ICC text tags in
//! moxcms, the PNG inside an ICO, the inner frame of a WebP, the data window of an EXR, the
//! compressed input a JPEG decoder keeps, the components and tiles a JPEG 2000 header
//! declares, the color profile a JPEG XL header declares and the reference frames its file
//! keeps for later frames, and an SVG's document tree, the images it embeds and the layers its filters,
//! masks, patterns and group opacity draw into. A hostile file can still push memory past
//! the cap there; only a memory limit the kernel enforces on the decoding process closes
//! that. Time has no cap either: an SVG's patterns, references and filters can keep its
//! worker busy for as long as they ask, which only a decoding process that can be killed
//! would bound. And a chain of SVG references, as of masks or clip paths that each name the
//! next, can run a worker's stack out, which aborts the process.
//!
//! Decodes that share a [`DecodeBudget`], as the grid's thumbnails do, hold no more together
//! than its bytes, by the same estimate the cap checks, while decodes end: under a decode that
//! stalls, the others go ahead after its stall time. Those that decode in a helper process,
//! SVG, and HEIC and AVIF on Linux, stay outside it: at worst every helper, two to four,
//! decodes at once beside the budget, each within the cap. On macOS HEIC and AVIF decode in
//! the viewer through ImageIO, and take their share once their size is known.
#![forbid(unsafe_code)]

use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use image::metadata::Orientation;
use image::{
    DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, ImageResult, Limits,
};
use image_extras::{xbm::XbmDecoder, xpm::XpmDecoder};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};

mod animation;
mod icns;
mod jpeg;
mod jpegxl;
mod loader;
mod psd;
mod pyramid;
mod raw;
mod svg;
mod thumbs;

pub use animation::Animation;
pub use loader::{Loaded, Loader, Stamp};
pub use pyramid::Pyramid;
pub use resvg::usvg::fontdb;
pub use thumbs::Thumbnails;

/// The file extensions this build decodes, for directory scans.
pub const EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "apng", "gif", "webp", "tif", "tiff", "bmp", "ico", "tga",
    "pbm", "pgm", "ppm", "pnm", "pam", "qoi", "ff", "exr", "hdr", "jp2", "jpf", "jpx", "j2k",
    "j2c", "jpc", "xpm", "xbm", "svg", "svgz", "jxl", "heic", "heif", "avif", "cr2", "cr3", "nef",
    "nrw", "arw", "srf", "sr2", "dng", "pef", "srw", "erf", "orf", "rw2", "raf", "psd", "psb",
    "icns",
];

/// An image's pixels: RGBA, 8 bits a channel, rows top to bottom, alpha not premultiplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The file may hold frames after this first one, as any GIF and an animated PNG or WebP
    /// do; an [`Animation`] decodes them.
    pub may_animate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Decoding the file would need more memory than the cap allows. The size is missing
    /// when the decoder's own limits stopped it before the header could report one.
    TooLarge { size: Option<(u32, u32)> },
    /// The file could not be read or decoded; the message says why.
    Unreadable(String),
    /// A camera RAW file with no embedded JPEG, which is all of a RAW the viewer shows.
    NoPreview,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::TooLarge {
                size: Some((width, height)),
            } => write!(f, "too large: {width}x{height}"),
            DecodeError::TooLarge { size: None } => f.write_str("too large"),
            DecodeError::Unreadable(why) => f.write_str(why),
            DecodeError::NoPreview => f.write_str("no preview in RAW"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn unreadable(e: impl fmt::Display) -> DecodeError {
    DecodeError::Unreadable(e.to_string())
}

/// The bytes an image of this size takes as RGBA with its pyramid ([`Pyramid`]), saturating
/// rather than overflowing for sizes no file could hold. The levels add about a third to an
/// image, and up to as much again to one a pixel wide or high.
pub fn memory_needed(width: u32, height: u32) -> u64 {
    let pixels: u128 = std::iter::once((width, height))
        .chain(pyramid::smaller(width, height))
        .map(|(w, h)| u128::from(w) * u128::from(h))
        .sum();
    u64::try_from(pixels * 4).unwrap_or(u64::MAX)
}

/// Decodes the first frame of the file at `path`, whose content, not its name, decides the
/// format. `max_bytes` caps the memory decoding may take: a file larger than that, an image
/// whose native pixels plus their RGBA copy would exceed it, or one whose RGBA pixels and
/// pyramid ([`memory_needed`]) would, is refused before its pixels are allocated. EXIF
/// orientation applies, and pixels tagged with an RGB ICC profile come out in sRGB.
pub fn decode(path: &Path, max_bytes: u64) -> Result<Image, DecodeError> {
    // one handle for the size check and the decode, so a file swapped in between cannot slip by
    decode_file(File::open(path).map_err(unreadable)?, path, max_bytes)
}

/// [`decode`] for a file already open, as a decoding helper gets it; `path` only names it,
/// for the formats its name picks, and is never opened.
pub fn decode_file(file: File, path: &Path, max_bytes: u64) -> Result<Image, DecodeError> {
    guarded(file, path, max_bytes, true, (0, None))
}

/// [`decode_file`] for the viewer's own process, refusing the formats that need a helper
/// ([`needs_helper`]), so a file that changed after that check cannot reach their decoders.
/// `side` is 0 for the whole image, or for a thumbnail the side in pixels it must cover, as
/// the HEIF helper takes it: a camera RAW then gives the smallest of its previews that serves,
/// and only RAW reads it today.
/// With a `share`, the decode waits after the header until its budget has room for it, and
/// takes that room into the share, where it stays until the share is dropped.
pub fn decode_here(
    file: File,
    path: &Path,
    max_bytes: u64,
    side: u32,
    share: Option<&DecodeShare>,
) -> Result<Image, DecodeError> {
    guarded(file, path, max_bytes, false, (side, share))
}

fn guarded(
    file: File,
    path: &Path,
    max_bytes: u64,
    helped: bool,
    (side, share): (u32, Option<&DecodeShare>),
) -> Result<Image, DecodeError> {
    // a decoder bug fails its file; some panic on hostile input, or while describing an error
    std::panic::catch_unwind(|| decode_open(file, path, max_bytes, helped, (side, share)))
        .unwrap_or_else(|_| Err(DecodeError::Unreadable("the decoder crashed".to_owned())))
}

/// Bytes that decodes running at once may hold together. Each takes, once its header is read
/// and before its pixels are allocated, the memory the cap measures it by, into a
/// [`DecodeShare`] its caller keeps as long as it keeps the picture, and that gives it back
/// when dropped, after an error and a panic too. One that does not fit waits until decodes in
/// flight end; one larger than the whole budget runs alone. A large decode may wait while
/// smaller ones pass it, until the work thins out: accepted. A waiter that has seen no decode
/// end for the stall time, as when a file on a mount that stopped answering keeps its decode
/// in flight, goes ahead and is counted. So the bound holds while decodes end, and one stuck
/// decode stops no other: the rest still run, a stall time late each time one has to wait.
pub struct DecodeBudget {
    total: u64,
    stall: Duration,
    /// The bytes taken, exact past a u64 as a decode that runs alone adds its own, and by how
    /// many decodes.
    taken: Mutex<(u128, usize)>,
    freed: Condvar,
}

impl DecodeBudget {
    /// A budget of `total` bytes, whose waiters go ahead after `stall` with no decode ended.
    pub fn new(total: u64, stall: Duration) -> DecodeBudget {
        DecodeBudget {
            total,
            stall,
            taken: Mutex::new((0, 0)),
            freed: Condvar::new(),
        }
    }

    /// A share to decode into, empty until a decode takes its bytes.
    pub fn share(&self) -> DecodeShare<'_> {
        DecodeShare {
            budget: self,
            taken: Mutex::new((0, 0)),
        }
    }

    fn take(&self, bytes: u64) {
        let mut taken = self.taken.lock().unwrap();
        while taken.1 > 0 && taken.0 + u128::from(bytes) > u128::from(self.total) {
            // every decode that ends wakes the waiters, so a wait that times out saw none end
            let (guard, waited) = self.freed.wait_timeout(taken, self.stall).unwrap();
            taken = guard;
            if waited.timed_out() {
                break;
            }
        }
        taken.0 += u128::from(bytes);
        taken.1 += 1;
    }
}

/// What decodes took from a budget for one picture, given back when dropped: kept by whoever
/// keeps the picture, so one still being scaled stays counted.
pub struct DecodeShare<'a> {
    budget: &'a DecodeBudget,
    /// The bytes taken, and by how many decodes.
    taken: Mutex<(u128, usize)>,
}

impl DecodeShare<'_> {
    /// Waits until the budget has room for `bytes`, as a decode does once it knows its size, and
    /// holds them until the share is dropped.
    pub fn take(&self, bytes: u64) {
        self.budget.take(bytes);
        let mut taken = self.taken.lock().unwrap();
        taken.0 += u128::from(bytes);
        taken.1 += 1;
    }
}

impl Drop for DecodeShare<'_> {
    fn drop(&mut self) {
        let (bytes, decodes) = *self.taken.lock().unwrap();
        if decodes == 0 {
            return;
        }
        // nothing panics while the lock is held, so it is never poisoned
        let mut taken = self.budget.taken.lock().unwrap();
        taken.0 -= bytes;
        taken.1 -= decodes;
        self.budget.freed.notify_all();
    }
}

/// A format the decoders here leave to another: SVG and, on Linux, HEIC and AVIF to a helper
/// process; on macOS HEIC and AVIF to ImageIO in the viewer's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Helped {
    /// SVG, whose reference chains can run a stack out and whose patterns can run forever.
    Svg,
    /// HEIC and AVIF, which on Linux libheif decodes, and only a helper loads; on macOS ImageIO
    /// decodes them in the viewer's own process.
    Heif,
}

/// The helper format the file holds, if any. The file is read from where it stands, and
/// left there.
pub fn needs_helper(file: &mut File, path: &Path) -> std::io::Result<Option<Helped>> {
    let at = file.stream_position()?;
    let mut start = Vec::with_capacity(START as usize);
    (&mut *file).take(START).read_to_end(&mut start)?;
    file.seek(std::io::SeekFrom::Start(at))?;
    Ok(match kind_of(&start, path) {
        Some(Kind::Svg) => Some(Helped::Svg),
        Some(Kind::Heif) => Some(Helped::Heif),
        _ => None,
    })
}

/// Turns premultiplied RGBA into straight alpha, in place.
pub fn unpremultiply(rgba: &mut [u8]) {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha != 0 && alpha != 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
}

/// Converts RGBA rows tagged with an ICC profile to sRGB in place, as the decoders here do;
/// a decoding helper calls it for pixels libheif gives.
pub fn icc_to_srgb(rgba: &mut [u8], width: usize, icc: &[u8]) {
    to_srgb(rgba, width, icc);
}

/// Converts RGBA rows in the color space that CICP code points name, as HEIF's nclx box does,
/// to sRGB in place. Code points moxcms has no primaries or curve for leave the pixels alone.
pub fn cicp_to_srgb(rgba: &mut [u8], width: usize, primaries: u8, transfer: u8) {
    // unspecified primaries are sRGB's, and BT.709's family of curves (1, 6, 14, 15) or none
    // named shows with the sRGB curve, as browsers show them
    let primaries = if primaries == 2 { 1 } else { primaries };
    let transfer = if matches!(transfer, 1 | 2 | 6 | 14 | 15) {
        13
    } else {
        transfer
    };
    // shortcut: HDR, PQ (16) or HLG (18), shows as stored, washed out but whole, as moxcms
    // maps PQ to SDR without tone mapping, near black; tone map it once such files matter
    if (primaries == 1 && transfer == 13) || matches!(transfer, 16 | 18) {
        return;
    }
    let (Ok(color_primaries), Ok(transfer_characteristics)) = (
        moxcms::CicpColorPrimaries::try_from(primaries),
        moxcms::TransferCharacteristics::try_from(transfer),
    ) else {
        return;
    };
    let mut source = ColorProfile::new_srgb();
    source.cicp = None;
    source.update_rgb_colorimetry_from_cicp(moxcms::CicpProfile {
        color_primaries,
        transfer_characteristics,
        matrix_coefficients: moxcms::MatrixCoefficients::Identity,
        full_range: true,
    });
    // the update answers false whatever happens, and sets cicp once it takes the primaries
    if source.cicp.is_some() {
        convert(rgba, width, &source);
    }
}

/// Sets the fonts SVG text draws with, before any SVG decodes: a decoding helper hands over
/// fonts it has mapped for good, as it cannot open files once it is sandboxed. False when
/// the fonts are set already.
pub fn set_svg_fonts(fonts: fontdb::Database) -> bool {
    svg::set_fonts(fonts)
}

fn decode_open(
    file: File,
    path: &Path,
    max_bytes: u64,
    helped: bool,
    (side, share): (u32, Option<&DecodeShare>),
) -> Result<Image, DecodeError> {
    let file_bytes = file.metadata().map_err(unreadable)?.len();
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_bytes);
    // what a JPEG 2000 decoder reads from, as it takes the whole file
    let mut data = Vec::new();
    let reader = BufReader::new(file);
    let opened = open(reader, path, (file_bytes, limits), &mut data, helped, side);
    let (mut decoder, kind) = match opened {
        Ok(opened) => opened,
        Err(Opening::Image(ImageError::Limits(_))) => {
            return Err(DecodeError::TooLarge { size: None });
        }
        Err(Opening::Image(e)) => return Err(unreadable(e)),
        Err(Opening::Decode(e)) => return Err(e),
    };
    // only these formats define their float samples as linear light
    let linear = matches!(kind, Kind::Image(ImageFormat::OpenExr | ImageFormat::Hdr));
    let may_animate = matches!(
        kind,
        Kind::Image(ImageFormat::Gif | ImageFormat::Png | ImageFormat::WebP)
    );
    // metadata a decoder cannot read leaves the pixels as stored
    let icc = decoder.icc_profile().ok().flatten();
    // a RAW's own orientation wins over the one its preview carries
    let orientation = match kind {
        Kind::Raw(_, Some(raw)) => raw,
        _ => decoder.orientation().unwrap_or(Orientation::NoTransforms),
    };
    let (width, height) = decoder.dimensions();
    // Peak memory: decoders such as TIFF's and EXR's hold a native-size buffer of their own
    // while they fill ours, so the native pixels count twice for every format; then the
    // native pixels and their RGBA copy live at once while converting, and a quarter turn
    // makes a second RGBA buffer after the native ones are gone.
    let rgba_bytes = (u64::from(width) * u64::from(height)).saturating_mul(4);
    let native = match kind {
        // the samples, wavelet coefficients and transform scratch as floats: up to about twenty
        // bytes for each byte of output, as a long gray strip of one row takes, beside the
        // whole file
        Kind::Jpeg2000 => decoder
            .total_bytes()
            .saturating_mul(21)
            .saturating_add(file_bytes),
        // a float plane sample for each byte of output, beside the byte, and the compressed
        // sections the decoder holds while it decodes them
        Kind::JpegXl => decoder
            .total_bytes()
            .saturating_mul(5)
            .saturating_add(file_bytes),
        // the cap as when the image crate decoded such a JPEG into RGB beside its RGBA copy;
        // a RAW's preview, beside the preview's bytes, counts as a JPEG does
        Kind::Image(ImageFormat::Jpeg) | Kind::Raw(..)
            if decoder.color_type() == image::ColorType::Rgba8 =>
        {
            (u64::from(width) * u64::from(height)).saturating_mul(3 * 2)
        }
        // and what the reader keeps beside the pixels: a CMYK document's profile, an icon's PNG
        // or the pixels it unpacked
        Kind::Psd(kept) | Kind::Icns(kept) => {
            decoder.total_bytes().saturating_mul(2).saturating_add(kept)
        }
        _ => decoder.total_bytes().saturating_mul(2),
    };
    // the ICC profile read above stays alive through the decode too
    let profile = icc.as_ref().map_or(0, |icc| icc.len() as u64);
    let converting = native.saturating_add(rgba_bytes).saturating_add(profile);
    let turning = if turns(orientation) {
        rgba_bytes.saturating_mul(2).saturating_add(profile)
    } else {
        0
    };
    let needed = converting.max(turning).max(memory_needed(width, height));
    if needed > max_bytes {
        return Err(DecodeError::TooLarge {
            size: Some((width, height)),
        });
    }
    if let Some(share) = share {
        share.take(needed);
    }
    let native = DynamicImage::from_decoder(decoder).map_err(|e| match e {
        ImageError::Limits(_) => DecodeError::TooLarge {
            size: Some((width, height)),
        },
        e => unreadable(e),
    })?;
    let mut image = DynamicImage::ImageRgba8(to_rgba8(native, linear));
    image.apply_orientation(orientation);
    let mut rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    if let Some(icc) = icc {
        to_srgb(&mut rgba, width as usize, &icc);
    }
    Ok(Image {
        width,
        height,
        rgba: rgba.into_raw(),
        may_animate,
    })
}

#[derive(Clone, Copy)]
enum Kind {
    /// A format the image crate decodes itself.
    Image(ImageFormat),
    Jpeg2000,
    JpegXl,
    Xpm,
    Xbm,
    Svg,
    Heif,
    /// Camera RAW, by its container, and once opened the orientation the RAW gives.
    Raw(raw::Container, Option<Orientation>),
    /// A Photoshop document or an icon file, and once opened the bytes its reader keeps.
    Psd(u64),
    Icns(u64),
}

/// Why a file could not be opened: what a decoder said, or what this crate found.
enum Opening {
    Image(ImageError),
    Decode(DecodeError),
}

impl From<ImageError> for Opening {
    fn from(e: ImageError) -> Opening {
        Opening::Image(e)
    }
}

impl From<std::io::Error> for Opening {
    fn from(e: std::io::Error) -> Opening {
        Opening::Image(e.into())
    }
}

// The file's decoder, picked by its content, or by its name for TGA, SVG and RAW on a TIFF
// header. A JPEG 2000 decoder reads the whole file, or an icon file's JPEG 2000 icon, which
// `data` holds.
fn open<'a>(
    mut reader: BufReader<File>,
    path: &Path,
    (file_bytes, limits): (u64, Limits),
    data: &'a mut Vec<u8>,
    helped: bool,
    side: u32,
) -> Result<(Box<dyn ImageDecoder + 'a>, Kind), Opening> {
    // a read may come back short, as from a network share, so the start comes whole or to EOF
    let mut start = Vec::with_capacity(START as usize);
    (&mut reader).take(START).read_to_end(&mut start)?;
    reader.rewind()?;
    let kind = match kind_of(&start, path) {
        Some(kind) => kind,
        None => Kind::Image(ImageFormat::from_path(path)?),
    };
    // SVG decodes in this process only when it is a helper's
    if matches!(kind, Kind::Svg) && !helped {
        return Err(outside_helper("SVG").into());
    }
    // some decoders read the whole file before the header, as the JPEG one does; of a RAW only
    // its preview is read, which counts instead
    if !matches!(kind, Kind::Raw(..)) && file_bytes > limits.max_alloc.unwrap_or(u64::MAX) {
        return Err(Opening::Decode(DecodeError::TooLarge { size: None }));
    }
    let mut decoder: Box<dyn ImageDecoder + 'a> = match kind {
        Kind::Raw(container, _) => {
            let max_bytes = limits.max_alloc.unwrap_or(u64::MAX);
            let preview =
                raw::preview(&mut reader, container, side, max_bytes).map_err(Opening::Decode)?;
            let kind = Kind::Raw(container, preview.orientation);
            // the preview's bytes go to the decoder that reads YCbCr, held once; the image
            // crate's decoder copies them, so there they count twice, before the copy is made
            let jpeg = match jpeg::take(preview.jpeg)? {
                Ok(mut decoder) => {
                    decoder.set_limits(limits)?;
                    return Ok((Box::new(decoder), kind));
                }
                Err(jpeg) => jpeg,
            };
            if (jpeg.len() as u64).saturating_mul(2) > max_bytes {
                return Err(Opening::Decode(DecodeError::TooLarge { size: None }));
            }
            let mut reader =
                ImageReader::with_format(std::io::Cursor::new(jpeg), ImageFormat::Jpeg);
            reader.limits(limits);
            return Ok((Box::new(reader.into_decoder()?), kind));
        }
        Kind::Image(format) => {
            // JPEG stored as YCbCr straight into RGBA, any other as before
            if format == ImageFormat::Jpeg
                && let jpeg::Opened::Rgba(mut decoder) = jpeg::open(&mut reader)?
            {
                decoder.set_limits(limits)?;
                return Ok((Box::new(decoder), kind));
            }
            let mut reader = ImageReader::with_format(reader, format);
            reader.limits(limits);
            return Ok((Box::new(reader.into_decoder()?), kind));
        }
        Kind::Jpeg2000 => {
            read_capped(reader, data, limits.max_alloc.unwrap_or(u64::MAX))?;
            let settings = hayro_jpeg2000::DecodeSettings::default();
            Box::new(hayro_jpeg2000::Image::new(data, &settings).map_err(ImageError::from)?)
        }
        Kind::JpegXl => Box::new(jpegxl::JxlDecoder::new(
            reader,
            limits.max_alloc.unwrap_or(u64::MAX),
        )?),
        // a helper decodes it through libheif, which this crate does not load
        Kind::Heif => return Err(outside_helper("HEIF").into()),
        Kind::Psd(_) => {
            let decoder = psd::PsdDecoder::new(reader)?;
            let kept = decoder.kept();
            let mut decoder: Box<dyn ImageDecoder + 'a> = Box::new(decoder);
            decoder.set_limits(limits)?;
            return Ok((decoder, Kind::Psd(kept)));
        }
        Kind::Icns(_) => match icns::largest(&mut reader).map_err(Opening::Decode)? {
            icns::Icon::Png(png) => {
                let kept = png.len() as u64;
                let mut reader =
                    ImageReader::with_format(std::io::Cursor::new(png), ImageFormat::Png);
                reader.limits(limits);
                return Ok((Box::new(reader.into_decoder()?), Kind::Icns(kept)));
            }
            // counted as a JPEG 2000 file is
            icns::Icon::Jpeg2000(jp2) => {
                *data = jp2;
                let settings = hayro_jpeg2000::DecodeSettings::default();
                let mut decoder: Box<dyn ImageDecoder + 'a> = Box::new(
                    hayro_jpeg2000::Image::new(data, &settings).map_err(ImageError::from)?,
                );
                decoder.set_limits(limits)?;
                return Ok((decoder, Kind::Jpeg2000));
            }
            icns::Icon::Rgba(pixels) => {
                let kept = pixels.bytes();
                let mut decoder: Box<dyn ImageDecoder + 'a> = Box::new(pixels);
                decoder.set_limits(limits)?;
                return Ok((decoder, Kind::Icns(kept)));
            }
        },
        Kind::Xpm => Box::new(XpmDecoder::new(reader)?),
        Kind::Xbm => Box::new(XbmDecoder::new(reader)?),
        Kind::Svg => Box::new(svg::SvgDecoder::new(
            reader,
            start.starts_with(svg::GZIP),
            limits.max_alloc.unwrap_or(u64::MAX),
        )?),
    };
    decoder.set_limits(limits)?;
    Ok((decoder, kind))
}

fn outside_helper(format: &str) -> ImageError {
    ImageError::Unsupported(image::error::UnsupportedError::from_format_and_kind(
        image::error::ImageFormatHint::Name(format.to_owned()),
        image::error::UnsupportedErrorKind::GenericFeature("decoding outside a helper".to_owned()),
    ))
}

// How much of a file's start shows its format: enough for an ISO BMFF file type box with
// a dozen brands.
const START: u64 = 64;

// HEIC and AVIF: an ISO BMFF file type box with a HEIF brand, major or compatible, checked
// before the image crate's guess, which takes AVIF for itself.
fn is_heif(start: &[u8]) -> bool {
    const BRANDS: [&[u8; 4]; 14] = [
        b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"mif2",
        b"mif3", b"msf1", b"avif", b"avis",
    ];
    let (Some(size), Some(b"ftyp")) = (start.first_chunk::<4>(), start.get(4..8)) else {
        return false;
    };
    let size = match u32::from_be_bytes(*size) {
        // 0 runs to the end of the file, and 1 puts a 64-bit size before the brands, which no
        // brand matches
        0 | 1 => start.len(),
        size => usize::try_from(size).unwrap_or(usize::MAX),
    };
    let (brands, _) = start[8..size.clamp(8, start.len())].as_chunks::<4>();
    // the major brand, then past the minor version the compatible ones
    brands
        .iter()
        .enumerate()
        .any(|(i, brand)| i != 1 && BRANDS.contains(&brand))
}

// The format the file's start shows, or its name for SVG, which carries no signature; None
// leaves the name to the image crate, as for TGA.
fn kind_of(start: &[u8], path: &Path) -> Option<Kind> {
    Some(
        if start.starts_with(b"\x00\x00\x00\x0cjP  ") || start.starts_with(b"\xff\x4f\xff\x51") {
            Kind::Jpeg2000
        } else if jpegxl::is_jxl(start) {
            Kind::JpegXl
        } else if is_heif(start) {
            Kind::Heif
        } else if start.starts_with(b"/* XPM */") {
            Kind::Xpm
        } else if start.starts_with(b"#define") && matches!(start.get(7), Some(b' ' | b'\t')) {
            Kind::Xbm
        } else if let Some(container) = raw::detect(start, path) {
            Kind::Raw(container, None)
        } else if psd::is_psd(start) {
            Kind::Psd(0)
        } else if icns::is_icns(start) {
            Kind::Icns(0)
        } else if let Ok(format) = image::guess_format(start) {
            Kind::Image(format)
        } else if svg::is_svg(start)
            || path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"))
        {
            Kind::Svg
        } else {
            return None;
        },
    )
}

// Reads all of `reader` into `data`, no more than `max_bytes`: a file may grow after its
// size was checked, and a stream may decompress to any length.
fn read_capped(reader: impl Read, data: &mut Vec<u8>, max_bytes: u64) -> ImageResult<()> {
    reader.take(max_bytes.saturating_add(1)).read_to_end(data)?;
    if data.len() as u64 > max_bytes {
        return Err(ImageError::Limits(image::error::LimitError::from_kind(
            image::error::LimitErrorKind::InsufficientMemory,
        )));
    }
    Ok(())
}

// 8-bit RGBA; linear float samples, as EXR and Radiance HDR hold them, get the sRGB curve
// on the way, where a plain conversion would only clamp and scale them. Other float
// samples, as in a float TIFF, are already encoded and only scale.
fn to_rgba8(image: DynamicImage, linear: bool) -> image::RgbaImage {
    let (width, height) = (image.width(), image.height());
    let (samples, channels) = match &image {
        DynamicImage::ImageRgb32F(buffer) if linear => (buffer.as_raw(), 3),
        DynamicImage::ImageRgba32F(buffer) if linear => (buffer.as_raw(), 4),
        _ => return image.into_rgba8(),
    };
    let rgba = linear_to_srgb8(samples, channels);
    image::RgbaImage::from_raw(width, height, rgba).expect("4 bytes for each pixel")
}

fn linear_to_srgb8(samples: &[f32], channels: usize) -> Vec<u8> {
    let encode = |v: f32| {
        let v = v.clamp(0.0, 1.0); // NaN clamps to NaN, which `as u8` turns into 0
        let srgb = if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (srgb * 255.0).round() as u8
    };
    samples
        .chunks_exact(channels)
        .flat_map(|p| {
            let alpha = p
                .get(3)
                .map_or(255, |a| (a.clamp(0.0, 1.0) * 255.0).round() as u8);
            [encode(p[0]), encode(p[1]), encode(p[2]), alpha]
        })
        .collect()
}

// the orientations that swap width and height, which turning cannot do in place
fn turns(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// Converts RGBA rows tagged with an ICC profile to sRGB in place. An RGB profile converts
/// the pixels, and a gray one the levels of a gray image, which reaches here as RGBA with equal
/// channels. A profile moxcms cannot read, or one of another kind, leaves the pixels as they
/// are; a CMYK JPEG reaches here as RGB but keeps its CMYK profile, which would misread it.
fn to_srgb(rgba: &mut [u8], width: usize, icc: &[u8]) {
    if let Ok(source) = ColorProfile::new_from_slice(icc) {
        convert(rgba, width, &source);
    }
}

fn convert(rgba: &mut [u8], width: usize, source: &ColorProfile) {
    let srgb = ColorProfile::new_srgb();
    let options = TransformOptions::default();
    match source.color_space {
        DataColorSpace::Rgb => {
            let Ok(transform) =
                source.create_transform_8bit(Layout::Rgba, &srgb, Layout::Rgba, options)
            else {
                return;
            };
            // a row at a time, so the copy the transform reads from stays small
            let mut row = vec![0; width * 4];
            for pixels in rgba.chunks_exact_mut(width * 4) {
                row.copy_from_slice(pixels);
                if transform.transform(&row, pixels).is_err() {
                    return;
                }
            }
        }
        DataColorSpace::Gray => {
            // a one-channel profile describes no colors, so a color image ignores it
            if rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] != p[1] || p[1] != p[2])
            {
                return;
            }
            // each of the 256 levels maps to one sRGB color, so a table does it
            let Ok(transform) =
                source.create_transform_8bit(Layout::Gray, &srgb, Layout::Rgb, options)
            else {
                return;
            };
            let levels: Vec<u8> = (0..=255).collect();
            let mut table = vec![0; 256 * 3];
            if transform.transform(&levels, &mut table).is_err() {
                return;
            }
            for pixel in rgba.as_chunks_mut::<4>().0 {
                for (channel, value) in pixel[..3].iter_mut().enumerate() {
                    *value = table[usize::from(*value) * 3 + channel];
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PLENTY: u64 = 1 << 30;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fixtures")
            .join(name)
    }

    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * image.width + x) * 4) as usize;
        image.rgba[at..at + 4].try_into().unwrap()
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nsxiv-ng-decode-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The synthetic RAW fixtures, and the size each shows: its preview's, turned as the RAW,
    /// or for RAF the JPEG itself, says. None for a file with no preview.
    const RAW: [(&str, Option<(u32, u32)>); 15] = [
        // the SubIFD's 64x48, not the 16x12 thumbnail or the lossless raw data, turned by 6
        ("raw-subifd.dng", Some((48, 64))),
        // the first directory's 48x32 over the next one's 24x16
        ("raw-two.cr2", Some((48, 32))),
        // behind MakerNotes: Nikon's (turned by 8), Sony's, Pentax's, Olympus's (by 6),
        ("raw-nikon.nef", Some((48, 64))),
        ("raw-sony.arw", Some((64, 48))),
        ("raw-pentax.pef", Some((64, 48))),
        ("raw.orf", Some((48, 64))),
        // Epson's, the JPEG's first byte EE
        ("raw-epson.erf", Some((64, 48))),
        // the other MakerNote headers: OM System's, Olympus's old one, Pentax's own
        ("raw-om.orf", Some((64, 48))),
        ("raw-olymp.orf", Some((64, 48))),
        ("raw-pentax-own.pef", Some((64, 48))),
        // JpgFromRaw, turned by 8
        ("raw.rw2", Some((48, 64))),
        // PRVW's 48x32 over THMB's 16x12, turned by CMT1's 6
        ("raw.cr3", Some((32, 48))),
        // the same in a box of 64-bit size
        ("raw-large.cr3", Some((32, 48))),
        // the JPEG's own orientation, as the RAF names none
        ("raw.raf", Some((48, 64))),
        ("raw-none.dng", None),
    ];

    #[test]
    fn a_raw_shows_its_largest_preview_turned_as_the_raw_says() {
        for (name, size) in RAW {
            let decoded = decode(&fixture(name), PLENTY).map(|i| (i.width, i.height));
            match size {
                Some(size) => assert_eq!(decoded, Ok(size), "{name}"),
                None => assert_eq!(decoded, Err(DecodeError::NoPreview), "{name}"),
            }
        }
        assert_eq!(DecodeError::NoPreview.to_string(), "no preview in RAW");
    }

    #[test]
    fn a_thumbnail_takes_the_smallest_preview_that_covers_its_side() {
        let two = fixture("raw-two.cr2");
        let shown = |side| {
            let file = File::open(&two).unwrap();
            decode_here(file, &two, PLENTY, side, None).map(|i| (i.width, i.height))
        };
        assert_eq!(shown(20), Ok((24, 16)), "the smaller covers 20");
        assert_eq!(shown(24), Ok((24, 16)));
        assert_eq!(shown(25), Ok((48, 32)), "only the larger covers 25");
        assert_eq!(shown(100), Ok((48, 32)), "none covers 100: the largest");
        assert_eq!(shown(0), Ok((48, 32)), "the whole image");
    }

    #[test]
    fn a_tiff_opens_as_a_tiff_and_a_dng_with_no_preview_says_so() {
        let mut tiff = Vec::new();
        image::RgbImage::new(8, 4)
            .write_to(&mut std::io::Cursor::new(&mut tiff), ImageFormat::Tiff)
            .unwrap();
        let plain = temp_file("plain.tif", &tiff);
        assert_eq!(
            decode(&plain, PLENTY).map(|i| (i.width, i.height)),
            Ok((8, 4))
        );
        let named = temp_file("named.dng", &tiff);
        assert_eq!(decode(&named, PLENTY), Err(DecodeError::NoPreview));
        for path in [plain, named] {
            std::fs::remove_file(path).unwrap();
        }
    }

    // A RAW named .dng whose one directory names a strip: the JPEG at `jpeg`, followed by
    // `padding` bytes, as a strip may run past its JPEG's end.
    fn dng_around(jpeg: &str, padding: usize) -> Vec<u8> {
        let jpeg = std::fs::read(fixture(jpeg)).unwrap();
        let length = (jpeg.len() + padding) as u32;
        let mut file = b"II*\0\x08\0\0\0\x03\0".to_vec();
        for (tag, kind, value) in [
            (0x0103u16, 3u16, 7u32),
            (0x0111, 4, 50),
            (0x0117, 4, length),
        ] {
            file.extend_from_slice(&tag.to_le_bytes());
            file.extend_from_slice(&kind.to_le_bytes());
            file.extend_from_slice(&1u32.to_le_bytes());
            file.extend_from_slice(&value.to_le_bytes());
        }
        file.extend_from_slice(&[0; 4]);
        assert_eq!(file.len(), 50);
        file.extend_from_slice(&jpeg);
        file.resize(file.len() + padding, 0);
        file
    }

    #[test]
    fn a_preview_the_image_crate_copies_counts_twice() {
        // the preview runs 6,000 bytes; the decode of a 37x23 JPEG needs some 8,500 beside it
        // where it goes straight into RGBA, and about 5,100 where the image crate takes it
        let cap = 9000;
        let direct = dng_around("jpeg-420.jpg", 6000 - 820);
        let path = temp_file("direct.dng", &direct);
        assert_eq!(
            decode(&path, cap).map(|i| (i.width, i.height)),
            Ok((37, 23))
        );
        std::fs::remove_file(path).unwrap();
        let copied = dng_around("jpeg-gray.jpg", 6000 - 544);
        let path = temp_file("copied.dng", &copied);
        assert_eq!(
            decode(&path, cap),
            Err(DecodeError::TooLarge { size: None }),
            "6,000 bytes held twice pass the cap"
        );
        assert!(decode(&path, 12_000).is_ok(), "and fit one twice as large");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn raw_files_are_told_by_their_content_where_it_tells() {
        let kind = |start: &[u8], name: &str| raw::detect(start, Path::new(name));
        let tiff = b"II*\0\x08\0\0\0\0\0\0\0";
        assert_eq!(
            kind(b"\0\0\0\x18ftypcrx \0\0\0\x01", "a.jpg"),
            Some(raw::Container::Cr3)
        );
        assert_eq!(
            kind(b"FUJIFILMCCD-RAW 0201", "a"),
            Some(raw::Container::Raf)
        );
        assert_eq!(kind(b"IIRO\x08\0\0\0", "a"), Some(raw::Container::Tiff));
        assert_eq!(kind(b"IIU\0\x08\0\0\0", "a"), Some(raw::Container::Tiff));
        assert_eq!(
            kind(b"II*\0\x10\0\0\0CR\x02\0", "a.tif"),
            Some(raw::Container::Tiff)
        );
        assert_eq!(
            kind(tiff, "a.NEF"),
            Some(raw::Container::Tiff),
            "by its name"
        );
        assert_eq!(kind(tiff, "a.tif"), None, "a plain TIFF is a TIFF");
        // a TIFF whose first directory at 8 holds 21,059 entries, which reads CR there
        assert_eq!(kind(b"II*\0\x08\0\0\0CR\x02\0\0\0\0\0", "a.tif"), None);
    }

    #[test]
    fn a_damaged_raw_gives_an_error_or_an_image_and_never_a_panic() {
        for (name, _) in RAW {
            let bytes = std::fs::read(fixture(name)).unwrap();
            let start = &bytes[..bytes.len().min(START as usize)];
            let container = raw::detect(start, &fixture(name)).unwrap();
            let search = |bytes: &[u8]| {
                // the search itself, unguarded, so a panic in it fails the test
                let mut cursor = std::io::Cursor::new(bytes);
                let found = raw::preview(&mut cursor, container, 0, PLENTY);
                // and the whole decode, which must end in one or the other
                let path = temp_file(name, bytes);
                let decoded = decode(&path, PLENTY);
                std::fs::remove_file(path).unwrap();
                (found.is_ok(), decoded.is_ok())
            };
            for length in 0..bytes.len() {
                search(&bytes[..length]);
            }
            for at in 0..bytes.len() {
                let mut flipped = bytes.clone();
                flipped[at] ^= 1 << (at % 8);
                search(&flipped);
                flipped[at] = 0xff;
                search(&flipped);
            }
        }
    }

    #[test]
    fn photoshop_documents_show_their_composite() {
        let image = |name: &str| decode(&fixture(name), PLENTY).unwrap();
        // run-length coded at 8 bits and raw at 16: the gradient, red at the top, blue below
        for name in ["psd-rgb.psd", "psd-rgb16.psd"] {
            let rgb = image(name);
            assert_eq!((rgb.width, rgb.height), (6, 4), "{name}");
            let (top, bottom) = (pixel(&rgb, 0, 0), pixel(&rgb, 5, 3));
            assert!(
                top[0] > 200 && top[2] < 60 && top[3] == 255,
                "{name}: {top:?}"
            );
            assert!(bottom[2] > 200 && bottom[0] < 60, "{name}: {bottom:?}");
        }
        let gray = image("psd-gray.psd");
        let (top, bottom) = (pixel(&gray, 0, 0), pixel(&gray, 0, 3));
        assert!(
            top[0] < 60 && bottom[0] > 200 && top[0] == top[2],
            "{top:?} {bottom:?}"
        );
        // CMYK with no profile, where ink takes away its share of the light
        let red = pixel(&image("psd-cmyk.psd"), 3, 1);
        assert!(
            red[0] > 240 && red[1] < 15 && red[2] < 15 && red[3] == 255,
            "{red:?}"
        );
        let index = image("psd-index.psd");
        assert_eq!(
            (pixel(&index, 0, 0), pixel(&index, 7, 1)),
            ([255, 0, 0, 255], [0, 255, 0, 255])
        );
        let bitmap = image("psd-bitmap.psd");
        assert_eq!(
            [pixel(&bitmap, 0, 0)[0], pixel(&bitmap, 1, 0)[0]],
            [0, 255],
            "a set bit is black"
        );
        // Photoshop blends a transparent composite's colors with white, which comes out again,
        // and ImageMagick stores them straight, which stay: each as its source, within a level
        let near = |name: &str, width: u32, source: &[[u8; 4]]| {
            let rgba = image(name);
            for (at, &want) in source.iter().enumerate() {
                let got = pixel(&rgba, at as u32 % width, at as u32 / width);
                let off = got.iter().zip(want).map(|(&a, b)| a.abs_diff(b)).max();
                assert!(off <= Some(1), "{name} at {at}: {got:?}, not {want:?}");
            }
        };
        let partial = [[40, 40, 40, 128], [220, 220, 220, 128]];
        near(
            "psd-matte.psd",
            3,
            &[
                [255, 0, 0, 255],
                [0, 0, 255, 102],
                [0, 255, 0, 51],
                partial[0],
                partial[1],
            ],
        );
        near(
            "psd-straight.psd",
            5,
            &[partial[0], partial[1], [40, 160, 40, 51], [255, 0, 0, 255]],
        );
        assert_eq!(pixel(&image("psd-matte.psd"), 2, 1)[3], 0);
        assert_eq!(pixel(&image("psd-straight.psd"), 4, 0)[3], 0);
        // 16-bit gray in a PSB, whose transparency a Mt16 block marks
        let gray16 = image("psd-gray16.psb");
        let (middle, quarter) = (pixel(&gray16, 0, 0), pixel(&gray16, 1, 0));
        assert!(
            middle[0].abs_diff(128) <= 1 && middle[3] == 255,
            "{middle:?}"
        );
        assert!(
            quarter[0].abs_diff(64) <= 1 && quarter[3].abs_diff(128) <= 1,
            "{quarter:?}"
        );
        assert_eq!(pixel(&gray16, 2, 0)[3], 0);
        // 16-bit RGB whose negative layer count sits in a Lr16 block
        let lr16 = image("psd-lr16.psd");
        assert_eq!(
            (pixel(&lr16, 0, 0), pixel(&lr16, 1, 0)[3]),
            ([255, 0, 0, 255], 0)
        );
        let index = image("psd-index-alpha.psd");
        assert_eq!(
            [0, 1, 2].map(|x| pixel(&index, x, 0)),
            [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 0]],
            "the transparent index"
        );
        let loose = image("psd-loose.psd");
        assert_eq!(
            [0, 1, 2].map(|x| pixel(&loose, x, 0)[0]),
            [0, 128, 255],
            "packed loosely"
        );
        // CMYK through its profile, which here takes green away for cyan ink, not red
        let profiled = image("psd-cmyk-icc.psd");
        let (cyan, white) = (pixel(&profiled, 0, 0), pixel(&profiled, 1, 0));
        assert!(
            cyan[0] > 200 && cyan[1] < 60,
            "through the profile: {cyan:?}"
        );
        assert!(white[..3].iter().all(|&v| v >= 250), "{white:?}");
        let red = pixel(&image("psd-cmyk16.psd"), 0, 0);
        assert!(
            red[0] > 250 && red[1] < 5 && red[2] < 5 && red[3] == 255,
            "{red:?}"
        );
        near(
            "psd-cmyk-alpha.psd",
            3,
            &[[255, 0, 0, 255], [0, 255, 255, 128]],
        );
        assert_eq!(pixel(&image("psd-cmyk-alpha.psd"), 2, 0)[3], 0);
        // CMYK blended with white in its own samples, with black ink and through a profile:
        // each pixel at half comes out as its source opaque beside it, converted
        for name in ["psd-cmyk-black.psd", "psd-cmyk-icc-alpha.psd"] {
            let cmyk = image(name);
            for x in [0, 2, 4] {
                let (half, opaque) = (pixel(&cmyk, x, 0), pixel(&cmyk, x + 1, 0));
                let off = (0..3).map(|c| half[c].abs_diff(opaque[c])).max().unwrap();
                assert!(
                    off <= 2 && half[3] == 128,
                    "{name} at {x}: {half:?} for {opaque:?}"
                );
            }
            assert_eq!(pixel(&cmyk, 6, 0)[3], 0);
        }
        // cyan with a quarter black, opaque as converted plainly, and full black at half
        let black = image("psd-cmyk-black.psd");
        assert_eq!(pixel(&black, 1, 0), [0, 191, 191, 255]);
        assert_eq!(pixel(&black, 2, 0), [0, 0, 0, 128]);
        // and straight CMYK, as ImageMagick writes it, stays as stored
        near(
            "psd-cmyk-straight.psd",
            5,
            &[
                [40, 40, 40, 128],
                [220, 220, 220, 128],
                [0, 160, 160, 128],
                [255, 0, 0, 255],
            ],
        );
        assert_eq!(pixel(&image("psd-cmyk-straight.psd"), 4, 0)[3], 0);
        // a spot channel past the transparency is not shown
        let spot = image("psd-spot.psd");
        assert_eq!(
            (pixel(&spot, 0, 0), pixel(&spot, 1, 0)[3]),
            ([255, 0, 0, 255], 0)
        );
        let duotone = image("psd-duotone.psd");
        assert_eq!(
            (pixel(&duotone, 0, 0), pixel(&duotone, 1, 0)),
            ([0, 0, 0, 255], [255; 4])
        );
        // a muted red in Display P3, more saturated in sRGB
        let p3 = pixel(&image("psd-p3.psd"), 4, 4);
        assert!(p3[0] > 205 && p3[1] < 97, "{p3:?}");
        // told by content, whatever its name
        let renamed = temp_file("psd.png", &std::fs::read(fixture("psd-rgb.psd")).unwrap());
        assert!(decode(&renamed, PLENTY).is_ok());
        std::fs::remove_file(renamed).unwrap();
        // refused, each with its reason
        for (name, why) in [
            ("psd-no-composite.psd", "without its composite"),
            ("psd-zip.psd", "ZIP"),
            ("psd-lab.psd", "Lab"),
            ("psd-32.psd", "32 bits"),
        ] {
            match decode(&fixture(name), PLENTY) {
                Err(DecodeError::Unreadable(text)) => assert!(text.contains(why), "{name}: {text}"),
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_psb_s_size_counts_before_its_pixels() {
        let mut bytes = std::fs::read(fixture("psd-gray16.psb")).unwrap();
        bytes[14..18].copy_from_slice(&300_000u32.to_be_bytes());
        bytes[18..22].copy_from_slice(&300_000u32.to_be_bytes());
        let path = temp_file("huge.psb", &bytes);
        assert_eq!(
            decode(&path, 1 << 30),
            Err(DecodeError::TooLarge {
                size: Some((300_000, 300_000))
            })
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn icon_files_show_their_largest_icon() {
        let image = |name: &str| decode(&fixture(name), PLENTY).unwrap();
        // png2icns's PNG at 128 over its run-length RGB at 16 and 32, the gradient red to blue
        let png = image("icns-png.icns");
        assert_eq!((png.width, png.height), (128, 128));
        assert!(pixel(&png, 0, 0)[0] > 200 && pixel(&png, 0, 127)[2] > 200);
        // and without the PNG, its run-length RGB at 32 with its mask
        let small = image("icns-small.icns");
        assert_eq!((small.width, small.height), (32, 32));
        let (top, bottom) = (pixel(&small, 0, 0), pixel(&small, 0, 31));
        assert!(top[0] > 200 && top[2] < 30 && top[3] == 255, "{top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 30, "{bottom:?}");
        // it32 over il32, opaque with no mask of its own
        let rgb = image("icns-rgb.icns");
        assert_eq!((rgb.width, rgb.height), (128, 128));
        assert_eq!(
            [(0, 0), (127, 0), (0, 127), (127, 127)].map(|(x, y)| pixel(&rgb, x, y)),
            [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255; 4]
            ]
        );
        // ARGB at 32 over RGB at 16
        let argb = image("icns-argb.icns");
        assert_eq!(
            [(0, 0), (31, 0), (0, 31), (31, 31)].map(|(x, y)| pixel(&argb, x, y)),
            [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 255, 128]
            ]
        );
        // JPEG 2000 at 32, as icp5 holds it
        let jp2 = image("icns-jp2.icns");
        assert_eq!((jp2.width, jp2.height), (32, 32));
        // ih32 with its mask
        let ih32 = image("icns-ih32.icns");
        assert_eq!(
            [(0, 0), (47, 0), (0, 47)].map(|(x, y)| pixel(&ih32, x, y)),
            [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]]
        );
        assert_eq!(pixel(&ih32, 47, 47)[3], 0);
        // il32 with a mask of the wrong size, left out
        let unmasked = image("icns-mask-size.icns");
        assert_eq!(
            (unmasked.width, pixel(&unmasked, 5, 5)),
            (32, [9, 9, 9, 255])
        );
        // is32 as RGB that stands as it is
        let raw = image("icns-raw.icns");
        assert_eq!((raw.width, pixel(&raw, 15, 15)), (16, [255, 0, 0, 255]));
    }

    #[test]
    fn what_a_reader_keeps_beside_the_pixels_counts() {
        // a CMYK document of 16x16 whose profile, which its reader keeps, takes 4,096 bytes:
        // its pixels fit a cap of 6,000 alone, and not with the profile
        let mut file = b"8BPS\0\x01".to_vec();
        file.extend([0; 6]);
        file.extend(4u16.to_be_bytes());
        file.extend(16u32.to_be_bytes());
        file.extend(16u32.to_be_bytes());
        file.extend(8u16.to_be_bytes());
        file.extend(4u16.to_be_bytes());
        file.extend(0u32.to_be_bytes());
        file.extend((12 + 4096u32).to_be_bytes());
        file.extend(b"8BIM\x04\x0f\0\0");
        file.extend(4096u32.to_be_bytes());
        file.resize(file.len() + 4096, 0);
        file.extend([0; 6]);
        file.resize(file.len() + 4 * 256, 255);
        let path = temp_file("kept.psd", &file);
        assert_eq!(
            decode(&path, 6000),
            Err(DecodeError::TooLarge {
                size: Some((16, 16))
            })
        );
        assert!(decode(&path, 8000).is_ok());
        std::fs::remove_file(path).unwrap();
        // an icon file whose PNG entry is padded to 100,000 bytes: the 128x128 icon fits a cap
        // of 250,000 alone, and not with the entry it is read from
        let icons = std::fs::read(fixture("icns-png.icns")).unwrap();
        let at = icons.windows(4).position(|w| w == b"ic07").unwrap();
        let size = u32::from_be_bytes(icons[at + 4..at + 8].try_into().unwrap()) as usize;
        let mut entry = icons[at + 8..at + size].to_vec();
        entry.resize(100_000, 0);
        let mut file = b"icns".to_vec();
        file.extend((16 + entry.len() as u32).to_be_bytes());
        file.extend(b"ic07");
        file.extend((8 + entry.len() as u32).to_be_bytes());
        file.extend(entry);
        let path = temp_file("kept.icns", &file);
        assert_eq!(
            decode(&path, 250_000),
            Err(DecodeError::TooLarge {
                size: Some((128, 128))
            })
        );
        assert!(decode(&path, 350_000).is_ok());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_damaged_psd_or_icon_file_gives_an_error_or_an_image_and_never_a_panic() {
        let names = std::fs::read_dir(fixture(""))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap());
        let names: Vec<String> = names
            .filter(|name| name.starts_with("psd-") || name.starts_with("icns-"))
            .collect();
        assert!(names.len() > 20, "{names:?}");
        for name in names {
            let bytes = std::fs::read(fixture(&name)).unwrap();
            let read = |bytes: &[u8]| {
                // the readers themselves, unguarded, so a panic in them fails the test
                let mut cursor = std::io::Cursor::new(bytes);
                if name.starts_with("psd-") {
                    if let Ok(decoder) = psd::PsdDecoder::new(&mut cursor)
                        && decoder.total_bytes() <= 1 << 20
                    {
                        let _ = DynamicImage::from_decoder(decoder);
                    }
                } else {
                    let _ = icns::largest(&mut cursor);
                }
                // and the whole decode, which must end in one or the other
                let path = temp_file(&name, bytes);
                let _ = decode(&path, PLENTY);
                std::fs::remove_file(path).unwrap();
            };
            for length in 0..bytes.len() {
                read(&bytes[..length]);
            }
            // a profile's own bytes are left whole: moxcms parses them, and a CMYK profile
            // takes it tens of milliseconds to build into a transform each time
            let profile = bytes
                .windows(6)
                .position(|w| w == b"8BIM\x04\x0f")
                .map(|at| {
                    let size = u32::from_be_bytes(bytes[at + 8..at + 12].try_into().unwrap());
                    at + 12..at + 12 + size as usize
                });
            for at in 0..bytes.len() {
                if profile
                    .as_ref()
                    .is_some_and(|profile| profile.contains(&at))
                {
                    continue;
                }
                let mut flipped = bytes.clone();
                flipped[at] ^= 1 << (at % 8);
                read(&flipped);
                flipped[at] = 0xff;
                read(&flipped);
            }
        }
    }

    // A two-frame GIF on a 20x10 canvas whose first frame is a red 4x4 patch at 12,3: a clear
    // code before each pixel keeps every LZW code 3 bits wide.
    fn patch_gif() -> Vec<u8> {
        let le = |v: u16| v.to_le_bytes();
        let frame = |x: u16, y: u16, w: u16, h: u16, color: u32| {
            let mut codes: Vec<u32> = (0..w * h).flat_map(|_| [4, color]).collect();
            codes.push(5);
            let (mut data, mut bits, mut count) = (Vec::new(), 0u32, 0);
            for code in codes {
                bits |= code << count;
                count += 3;
                while count >= 8 {
                    data.push(bits as u8);
                    bits >>= 8;
                    count -= 8;
                }
            }
            if count > 0 {
                data.push(bits as u8);
            }
            // 0.1 s, then where the frame lies
            let mut bytes = vec![0x21, 0xf9, 4, 0, 10, 0, 0, 0, 0x2c];
            for v in [x, y, w, h] {
                bytes.extend(le(v));
            }
            bytes.extend([0, 2, data.len() as u8]);
            bytes.extend(data);
            bytes.push(0);
            bytes
        };
        let mut gif = b"GIF89a".to_vec();
        gif.extend(le(20));
        gif.extend(le(10));
        // two colors, black and red
        gif.extend([0x80, 0, 0, 0, 0, 0, 255, 0, 0]);
        gif.extend(frame(12, 3, 4, 4, 1));
        gif.extend(frame(0, 0, 1, 1, 0));
        gif.push(0x3b);
        gif
    }

    #[test]
    fn an_animation_s_first_frame_lies_where_it_lies_on_the_canvas() {
        // what a thumbnail is made from, which must lie where the frame lies
        let path = temp_file("patch.gif", &patch_gif());
        let image = decode(&path, PLENTY).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!((image.width, image.height), (20, 10));
        assert_eq!(pixel(&image, 13, 4), [255, 0, 0, 255], "the patch at 12,3");
        assert_eq!(pixel(&image, 1, 1)[3], 0, "the canvas around it clear");
    }

    // a share of `budget` that holds `bytes`
    fn holding(budget: &DecodeBudget, bytes: u64) -> DecodeShare<'_> {
        let share = budget.share();
        share.take(bytes);
        share
    }

    #[test]
    fn a_decode_waits_for_room_in_its_budget_and_gives_it_back_however_it_ends() {
        let budget = std::sync::Arc::new(DecodeBudget::new(100, Duration::from_secs(60)));
        let taken = holding(&budget, 60);
        let (sent, came) = std::sync::mpsc::channel();
        let other = std::sync::Arc::clone(&budget);
        let waiter = std::thread::spawn(move || {
            sent.send("about to take").unwrap();
            let _taken = holding(&other, 60);
            sent.send("took").unwrap();
        });
        // the silence counts once the waiter is at the budget, not before it has started
        assert_eq!(came.recv().unwrap(), "about to take");
        let wait = std::time::Duration::from_millis(200);
        assert!(came.recv_timeout(wait).is_err(), "no room beside the first");
        drop(taken);
        let ended = came.recv_timeout(std::time::Duration::from_secs(10));
        assert!(ended.is_ok(), "room once it ends");
        waiter.join().unwrap();
        let state = |budget: &DecodeBudget| *budget.taken.lock().unwrap();
        assert_eq!(state(&budget), (0, 0));
        // more than the whole budget runs alone, at once
        drop(holding(&budget, 150));
        // the whole of a u64 holds no two decodes, and gives back exactly what each took
        let wide = std::sync::Arc::new(DecodeBudget::new(u64::MAX, Duration::from_secs(60)));
        let first = holding(&wide, u64::MAX);
        let (sent, came) = std::sync::mpsc::channel();
        let other = std::sync::Arc::clone(&wide);
        let second = std::thread::spawn(move || {
            sent.send(()).unwrap();
            drop(holding(&other, u64::MAX));
        });
        came.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            state(&wide),
            (u128::from(u64::MAX), 1),
            "the second does not fit"
        );
        drop(first);
        second.join().unwrap();
        assert_eq!(state(&wide), (0, 0));
        // a decode that never ends keeps a waiter only for the stall time, which then goes
        // ahead and is counted
        let stalled = std::sync::Arc::new(DecodeBudget::new(100, Duration::from_millis(100)));
        let stuck = holding(&stalled, 60);
        let other = std::sync::Arc::clone(&stalled);
        let ahead = std::thread::spawn(move || {
            let _taken = holding(&other, 60);
            *other.taken.lock().unwrap()
        });
        assert_eq!(ahead.join().unwrap(), (120, 2));
        drop(stuck);
        assert_eq!(state(&stalled), (0, 0));
        // a panic gives back what it took
        let caught = std::panic::catch_unwind(|| {
            let _taken = holding(&budget, 10);
            panic!("a decoder bug");
        });
        assert!(caught.is_err());
        assert_eq!(state(&budget), (0, 0));
        // a decode takes into its share, which holds it after the decode returns, as the caller
        // keeps the picture, and gives it back when dropped; so does one that fails past its
        // header
        fn here(path: &Path, share: &DecodeShare) -> Result<Image, DecodeError> {
            decode_here(File::open(path).unwrap(), path, PLENTY, 0, Some(share))
        }
        let share = budget.share();
        assert!(here(&fixture("gradient.png"), &share).is_ok());
        assert_eq!(state(&budget).1, 1, "still held");
        drop(share);
        assert_eq!(state(&budget), (0, 0));
        let bytes = std::fs::read(fixture("gradient.png")).unwrap();
        let cut = temp_file("cut.png", &bytes[..bytes.len() / 2]);
        drop(here(&cut, &budget.share()));
        std::fs::remove_file(cut).unwrap();
        assert_eq!(state(&budget), (0, 0));
    }

    #[test]
    fn the_fixtures_decode() {
        let gradient = decode(&fixture("gradient.png"), PLENTY).unwrap();
        assert_eq!((gradient.width, gradient.height), (320, 200));
        assert!(
            pixel(&gradient, 160, 0)[0] > 240 && pixel(&gradient, 160, 199)[2] > 240,
            "red on top, blue at the bottom"
        );
        let alpha = decode(&fixture("alpha.png"), PLENTY).unwrap();
        assert_eq!(
            (pixel(&alpha, 80, 80)[3], pixel(&alpha, 0, 0)[3]),
            (255, 0),
            "alpha survives"
        );
        let photo = decode(&fixture("photo.jpg"), PLENTY).unwrap();
        assert_eq!(
            (photo.width, photo.height, photo.rgba.len()),
            (640, 480, 640 * 480 * 4)
        );
        let gif = decode(&fixture("moving.gif"), PLENTY).unwrap();
        assert_eq!(
            (gif.width, gif.height),
            (96, 64),
            "an animation gives its first frame"
        );
    }

    #[test]
    fn jpeg_straight_into_rgba_gives_the_image_crate_s_bytes() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures");
        let (mut straight, mut old, mut failed) = (Vec::new(), Vec::new(), Vec::new());
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "jpg") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).unwrap();
            let theirs = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg)
                .map(DynamicImage::into_rgba8);
            let crate_decoder =
                || image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&bytes)).unwrap();
            let mut reader = std::io::Cursor::new(&bytes);
            let ours = match jpeg::open(&mut reader) {
                Ok(jpeg::Opened::Rgba(mut decoder)) => {
                    let profile = decoder.icc_profile().unwrap();
                    assert_eq!(profile, crate_decoder().icc_profile().unwrap(), "{name}");
                    let orientation = decoder.orientation().unwrap();
                    assert_eq!(
                        orientation,
                        crate_decoder().orientation().unwrap(),
                        "{name}"
                    );
                    straight.push(name.clone());
                    DynamicImage::from_decoder(decoder).map(|rgba| {
                        assert!(matches!(rgba, DynamicImage::ImageRgba8(_)), "{name}");
                        rgba.into_rgba8()
                    })
                }
                Ok(jpeg::Opened::Other) => {
                    assert_eq!(reader.position(), 0, "{name}: left at its start");
                    old.push(name.clone());
                    ImageReader::with_format(reader, ImageFormat::Jpeg)
                        .decode()
                        .map(DynamicImage::into_rgba8)
                }
                Err(e) => Err(e),
            };
            match (ours, theirs) {
                (Ok(ours), Ok(theirs)) => assert!(ours == theirs, "{name}: the same bytes"),
                (Err(ours), Err(theirs)) => {
                    failed.push(name.clone());
                    assert_eq!(ours.to_string(), theirs.to_string(), "{name}");
                    assert_eq!(
                        std::mem::discriminant(&ours),
                        std::mem::discriminant(&theirs),
                        "{name}: {ours:?}"
                    );
                }
                (ours, theirs) => panic!("{name}: {ours:?} against {theirs:?}"),
            }
        }
        for name in [
            "jpeg-420.jpg",
            "jpeg-422.jpg",
            "jpeg-444.jpg",
            "jpeg-progressive.jpg",
            "jpeg-restart.jpg",
            "jpeg-p3.jpg",
            "orientation-6.jpg",
            "photo.jpg",
        ] {
            assert!(straight.iter().any(|n| n == name), "{name} goes straight");
        }
        // zune-jpeg maps no colors into RGBA from RGB, gray or CMYK
        for name in ["jpeg-rgb.jpg", "jpeg-gray.jpg", "jpeg-cmyk.jpg"] {
            assert!(old.iter().any(|n| n == name), "{name} goes the old way");
        }
        assert_eq!(
            failed,
            ["jpeg-arithmetic.jpg"],
            "and only zune's refusal fails"
        );
        assert!(
            decode(&dir.join("jpeg-rgb.jpg"), PLENTY).is_ok(),
            "a JPEG stored as RGB loads"
        );
    }

    #[test]
    fn content_decides_the_format_and_the_cap_holds() {
        let png_named_jpg = temp_file(
            "really-a-png.jpg",
            &std::fs::read(fixture("gradient.png")).unwrap(),
        );
        assert_eq!(decode(&png_named_jpg, PLENTY).map(|i| i.width), Ok(320));
        // a cap the native pixels fit in, but not their RGBA copy as well: the header gives the size
        let native = ImageReader::open(fixture("gradient.png"))
            .unwrap()
            .with_guessed_format()
            .unwrap();
        let native = native.into_decoder().unwrap().total_bytes();
        let refused = decode(&fixture("gradient.png"), native);
        assert_eq!(
            refused,
            Err(DecodeError::TooLarge {
                size: Some((320, 200))
            })
        );
        assert_eq!(refused.unwrap_err().to_string(), "too large: 320x200");
        let tiny = decode(&fixture("gradient.png"), 1000);
        assert_eq!(
            tiny,
            Err(DecodeError::TooLarge { size: None }),
            "a file bigger than the cap is not read"
        );
        assert_eq!(
            memory_needed(u32::MAX, u32::MAX),
            u64::MAX,
            "no overflow for sizes no file could hold"
        );
        let text = temp_file("notes.png", b"not an image");
        assert!(matches!(
            decode(&text, PLENTY),
            Err(DecodeError::Unreadable(_))
        ));
        std::fs::remove_file(png_named_jpg).unwrap();
        std::fs::remove_file(text).unwrap();
    }

    #[test]
    fn a_tga_decodes_by_its_name_and_linear_light_gets_the_srgb_curve() {
        let mut tga = Vec::new();
        image::codecs::tga::TgaEncoder::new(&mut tga)
            .encode(&[255; 3 * 2 * 4], 3, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let path = temp_file("plain.tga", &tga);
        assert_eq!(
            decode(&path, PLENTY).map(|i| (i.width, i.height)),
            Ok((3, 2))
        );
        std::fs::remove_file(path).unwrap();
        // a TGA whose image ID is 60 bytes long starts with `<`, which does not make it SVG
        let mut tga = vec![60, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 32, 0x28];
        tga.extend([b' '; 60]);
        tga.extend([0, 0, 255, 255]); // one red pixel, stored blue first
        let path = temp_file("id.tga", &tga);
        assert_eq!(
            decode(&path, PLENTY).map(|i| pixel(&i, 0, 0)),
            Ok([255, 0, 0, 255])
        );
        std::fs::remove_file(path).unwrap();
        // linear 0.22 is about half brightness once encoded: 129, not the 56 a plain scale gives
        assert_eq!(
            linear_to_srgb8(&[0.22, 0.22, 0.22], 3),
            [129, 129, 129, 255]
        );
        assert_eq!(
            linear_to_srgb8(&[2.0, -1.0, f32::NAN, 0.5], 4),
            [255, 0, 0, 128]
        );
        // a Radiance HDR file holds linear light; a float TIFF holds encoded values
        let mut hdr = Vec::new();
        let gray = [image::Rgb([0.22f32, 0.22, 0.22]); 2];
        image::codecs::hdr::HdrEncoder::new(&mut hdr)
            .encode(&gray, 2, 1)
            .unwrap();
        let path = temp_file("linear.hdr", &hdr);
        let red = pixel(&decode(&path, PLENTY).unwrap(), 0, 0)[0];
        assert!(red.abs_diff(129) <= 1, "the sRGB curve: {red}");
        std::fs::remove_file(path).unwrap();
        use image::ImageEncoder as _;
        let mut tiff = std::io::Cursor::new(Vec::new());
        let half: Vec<u8> = [0.5f32; 3 * 2]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        image::codecs::tiff::TiffEncoder::new(&mut tiff)
            .write_image(&half, 2, 1, image::ExtendedColorType::Rgb32F)
            .unwrap();
        let path = temp_file("encoded.tiff", tiff.get_ref());
        assert_eq!(
            pixel(&decode(&path, PLENTY).unwrap(), 0, 0)[0],
            128,
            "no curve for a float TIFF"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn jpeg_2000_xpm_and_xbm_decode() {
        // red on top, blue at the bottom, in a JP2 box and as a bare codestream
        for name in ["gradient.jp2", "gradient.j2k"] {
            let image = decode(&fixture(name), PLENTY).unwrap();
            assert_eq!((image.width, image.height), (64, 40), "{name}");
            let (top, bottom) = (pixel(&image, 32, 0), pixel(&image, 32, 39));
            assert!(
                top[0] > 200 && top[2] < 60 && bottom[2] > 200 && bottom[0] < 60,
                "{name}: {top:?} {bottom:?}"
            );
        }
        // a cap the RGB pixels fit in many times over, but not the decoder's float samples
        assert_eq!(
            decode(&fixture("gradient.jp2"), 100_000),
            Err(DecodeError::TooLarge {
                size: Some((64, 40))
            })
        );
        let xpm = temp_file(
            "two.xpm",
            b"/* XPM */\nstatic char *two[] = {\n\"2 1 2 1\",\n\"r c #FF0000\",\n\"b c None\",\n\"rb\"\n};\n",
        );
        let image = decode(&xpm, PLENTY).unwrap();
        assert_eq!(
            (pixel(&image, 0, 0), pixel(&image, 1, 0)[3]),
            ([255, 0, 0, 255], 0),
            "a red pixel and a transparent one"
        );
        let xbm = temp_file(
            "bits.xbm",
            b"#define bits_width 8\n#define bits_height 1\nstatic unsigned char bits_bits[] = { 0x0f };\n",
        );
        let image = decode(&xbm, PLENTY).unwrap();
        let row: Vec<u8> = (0..8).map(|x| pixel(&image, x, 0)[0]).collect();
        assert_eq!(row, [0, 0, 0, 0, 255, 255, 255, 255], "set bits are black");
        let tabbed = temp_file(
            "tabbed.xbm",
            b"#define\tbits_width 1\n#define\tbits_height 1\nstatic char bits_bits[] = { 0x01 };\n",
        );
        assert_eq!(decode(&tabbed, PLENTY).map(|i| i.width), Ok(1));
        // a decoder that panics over a bad color fails the file instead of the caller
        let bad = temp_file(
            "bad.xpm",
            b"/* XPM */\nstatic char *bad[] = {\n\"1 1 1 1\",\n\"a c not_a_color\",\n\"a\"\n};\n",
        );
        assert!(matches!(
            decode(&bad, PLENTY),
            Err(DecodeError::Unreadable(_))
        ));
        for path in [xpm, xbm, tabbed, bad] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn svg_draws_at_its_size_and_reads_no_file_it_names() {
        use std::io::Write as _;
        let red = temp_file("red.png", &[]);
        image::RgbaImage::from_pixel(4, 2, image::Rgba([255, 0, 0, 255]))
            .save(&red)
            .unwrap();
        // red on the left, half-transparent blue on the right, and over it all an image file
        let svg = format!(
            r#"<?xml version="1.0"?>
<svg xmlns="http://www.w3.org/2000/svg" width="4" height="2">
  <rect width="2" height="2" fill="red"/>
  <rect x="2" width="2" height="2" fill="blue" fill-opacity="0.5"/>
  <image href="{}" width="4" height="2"/>
</svg>"#,
            red.display()
        );
        let path = temp_file("shapes.svg", svg.as_bytes());
        let image = decode(&path, PLENTY).unwrap();
        assert_eq!((image.width, image.height), (4, 2));
        assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255]);
        let blue = pixel(&image, 3, 1);
        assert!(
            blue[0] == 0 && blue[2] > 250 && blue[3].abs_diff(128) <= 1,
            "straight alpha, and no red from the file the SVG names: {blue:?}"
        );
        // the same as SVGZ; one that decompresses past the cap is refused
        let mut svgz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        svgz.write_all(svg.as_bytes()).unwrap();
        let svgz = temp_file("shapes.svgz", &svgz.finish().unwrap());
        assert_eq!(
            decode(&svgz, PLENTY).map(|i| pixel(&i, 0, 0)),
            Ok([255, 0, 0, 255])
        );
        let mut bomb = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        bomb.write_all(&vec![b' '; 1 << 20]).unwrap();
        let bomb = temp_file("bomb.svgz", &bomb.finish().unwrap());
        assert_eq!(
            decode(&bomb, 100_000),
            Err(DecodeError::TooLarge { size: None })
        );
        // nesting deep enough to run a worker's stack out is refused before usvg walks it
        let deep = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><text>{}{}</text></svg>"#,
            "<tspan>".repeat(50_000),
            "</tspan>".repeat(50_000)
        );
        // and an SVG embedded as data, which usvg would parse past that check, does not draw
        let encoded: String = deep
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let outer = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image width="1" height="1" href="data:image/svg+xml,{encoded}"/></svg>"#
        );
        let deep = temp_file("deep.svg", deep.as_bytes());
        assert!(matches!(
            decode(&deep, PLENTY),
            Err(DecodeError::Unreadable(_))
        ));
        let outer = temp_file("embeds.svg", outer.as_bytes());
        assert_eq!(decode(&outer, PLENTY).map(|i| pixel(&i, 0, 0)), Ok([0; 4]));
        std::fs::remove_file(deep).unwrap();
        std::fs::remove_file(outer).unwrap();
        // a size no cap holds is refused before anything is drawn
        let huge = temp_file(
            "huge.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="100000"/>"#,
        );
        assert_eq!(
            decode(&huge, PLENTY),
            Err(DecodeError::TooLarge {
                size: Some((100_000, 100_000))
            })
        );
        for file in [red, path, svgz, bomb, huge] {
            std::fs::remove_file(file).unwrap();
        }
    }

    #[test]
    fn jpeg_xl_decodes() {
        // red on top, blue at the bottom, lossy, so decoded from XYB
        let image = decode(&fixture("gradient.jxl"), PLENTY).unwrap();
        assert_eq!((image.width, image.height), (320, 200));
        let (top, bottom) = (pixel(&image, 160, 0), pixel(&image, 160, 199));
        assert!(
            top[0] > 200 && top[2] < 60 && bottom[2] > 200 && bottom[0] < 60,
            "{top:?} {bottom:?}"
        );
        // a green disk, lossless, transparent around it
        let disk = decode(&fixture("alpha.jxl"), PLENTY).unwrap();
        let (inside, outside) = (pixel(&disk, 80, 80), pixel(&disk, 0, 0));
        assert!(
            inside[1] > 100 && inside[3] == 255 && outside[3] == 0,
            "{inside:?} {outside:?}"
        );
        // a gray ramp, dark at the top, comes out as gray RGBA
        let ramp = decode(&fixture("gray.jxl"), PLENTY).unwrap();
        let (dark, light) = (pixel(&ramp, 16, 0), pixel(&ramp, 16, 7));
        assert!(
            dark[0] < 30
                && light[0] > 225
                && dark[..3].iter().all(|&c| c == dark[0])
                && light[3] == 255,
            "{dark:?} {light:?}"
        );
        // a muted red in Display P3, which the decoder gives in P3 and to_srgb makes sRGB's
        let red = pixel(&decode(&fixture("p3.jxl"), PLENTY).unwrap(), 4, 4);
        assert!(
            red[0] > 205 && red[1] < 97,
            "more saturated in sRGB: {red:?}"
        );
        // gray PQ, which jxl-rs cannot tone map into a profile moxcms reads, is refused
        assert!(matches!(
            decode(&fixture("gray-pq.jxl"), PLENTY),
            Err(DecodeError::Unreadable(_))
        ));
        // a cap the RGBA pixels fit in twice over, but not the decoder's float planes
        assert_eq!(
            decode(&fixture("gradient.jxl"), 1_000_000),
            Err(DecodeError::TooLarge {
                size: Some((320, 200))
            })
        );
        assert_eq!(
            decode(&fixture("gradient.jxl"), 400_000),
            Err(DecodeError::TooLarge { size: None }),
            "past the decoder's own sample limit, which the cap sets"
        );
        assert!(jpegxl::is_jxl(b"\0\0\0\x0cJXL \r\n\x87\n\0\0\0\x14"));
    }

    #[test]
    fn heif_shows_in_its_major_or_a_compatible_brand() {
        let ftyp = |major: &[u8; 4], compatible: &[u8]| {
            let size = 16 + compatible.len() as u32;
            [&size.to_be_bytes()[..], b"ftyp", major, &[0; 4], compatible].concat()
        };
        assert!(is_heif(&ftyp(b"heic", b"")));
        assert!(is_heif(&ftyp(b"MA1B", b"miafavif")));
        // libheif's compact files put their format in the minor version
        assert!(is_heif(&ftyp(b"mif3", b"")));
        let mut to_the_end = ftyp(b"isom", b"heic");
        to_the_end[..4].fill(0);
        assert!(is_heif(&to_the_end));
        assert!(!is_heif(&ftyp(b"isom", b"iso2mp41")));
        // a brand counts only inside the box
        let past = [ftyp(b"isom", b"iso2"), b"avif".to_vec()].concat();
        assert!(!is_heif(&past));
    }

    #[test]
    fn cicp_code_points_convert_as_profiles_do() {
        // a muted red in Display P3 (primaries 12, the sRGB curve 13) is more saturated in sRGB
        let mut p3 = vec![200, 100, 100, 255];
        cicp_to_srgb(&mut p3, 1, 12, 13);
        assert!(p3[0] > 205 && p3[1] < 97 && p3[3] == 255, "{p3:?}");
        // a curve not named is sRGB's
        let mut unnamed = vec![200, 100, 100, 255];
        cicp_to_srgb(&mut unnamed, 1, 12, 2);
        assert_eq!(unnamed, p3);
        // sRGB itself, BT.709 with its own curve, unspecified, and PQ, which would need tone
        // mapping, stay as stored
        for (primaries, transfer) in [(1, 13), (1, 1), (2, 2), (9, 16)] {
            let mut same = vec![200, 100, 100, 255];
            cicp_to_srgb(&mut same, 1, primaries, transfer);
            assert_eq!(same, [200, 100, 100, 255], "{primaries} {transfer}");
        }
    }

    #[test]
    fn the_viewer_s_own_process_refuses_svg() {
        let svg = temp_file(
            "here.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#,
        );
        let open = || File::open(&svg).unwrap();
        assert_eq!(needs_helper(&mut open(), &svg).unwrap(), Some(Helped::Svg));
        assert!(matches!(
            decode_here(open(), &svg, PLENTY, 0, None),
            Err(DecodeError::Unreadable(_))
        ));
        assert_eq!(decode_file(open(), &svg, PLENTY).map(|i| i.width), Ok(1));
        std::fs::remove_file(svg).unwrap();
    }

    #[test]
    fn every_exif_orientation_comes_out_upright() {
        // Each file stores the same 32x16 image turned back by its tag, 16x32 for the tags that
        // turn a quarter, so all show it upright: red, green over blue, white.
        let quadrants = [
            ((8, 4), [255, 0, 0]),
            ((24, 4), [0, 255, 0]),
            ((8, 12), [0, 0, 255]),
            ((24, 12), [255, 255, 255]),
        ];
        for tag in 1..=8 {
            let image = decode(&fixture(&format!("orientation-{tag}.jpg")), PLENTY).unwrap();
            assert_eq!((image.width, image.height), (32, 16), "orientation {tag}");
            for ((x, y), color) in quadrants {
                let found = pixel(&image, x, y);
                assert!(
                    (0..3).all(|c| found[c].abs_diff(color[c]) <= 8),
                    "orientation {tag} at {x},{y}: {found:?}"
                );
            }
        }
    }

    #[test]
    fn decompression_bombs_are_refused_as_too_large() {
        // Small files that declare, or expand to, images far past the cap are refused as too
        // large, with the size their header gives. This shows the refusal, not when memory was
        // taken: whether a buffer was allocated before it, the test cannot see. The cap does
        // not cover what the crate's documentation lists, such as WebP's inner frame or the
        // input a JPEG decoder keeps, which the file's own size bounds.
        const CAP: u64 = 64 << 20;
        let refused = |name: &str, bytes: &[u8], size| {
            let path = temp_file(name, bytes);
            assert_eq!(
                decode(&path, CAP),
                Err(DecodeError::TooLarge { size: Some(size) }),
                "{name}"
            );
            path
        };
        // PNG and JPEG whose headers say 65535 x 65535, 16 GiB as RGBA
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 65535, 65535);
        encoder.set_color(png::ColorType::Rgba);
        let mut writer = encoder.write_header().unwrap();
        // the start of a zlib stream and no more, then the end the writer adds
        writer.write_chunk(png::chunk::IDAT, &[0x78, 0x9c]).unwrap();
        drop(writer);
        let png = refused("bomb.png", &png, (65535, 65535));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&[0; 8 * 8 * 3], 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let frame = jpeg.windows(2).position(|m| m == [0xff, 0xc0]).unwrap();
        jpeg[frame + 5..frame + 9].fill(0xff);
        let jpeg = refused("bomb.jpg", &jpeg, (65535, 65535));
        // lossless WebP whose header says 16383 x 16383, 1 GiB, in 14 bits each less one
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .encode(&[0; 4], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let bits = u32::from_le_bytes(webp[21..25].try_into().unwrap());
        let bits = (bits & !0x0fff_ffff) | (0x3ffe << 14) | 0x3ffe;
        webp[21..25].copy_from_slice(&bits.to_le_bytes());
        let webp = refused("bomb.webp", &webp, (16383, 16383));
        // a GIF of 1 x 1 whose two frames say 65535 x 65535 each, which the image crate's limit,
        // set from the cap, refuses: as a still, and as an animation, which keeps no frame
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif)
            .encode_frames((0..2).map(|_| image::Frame::new(image::RgbaImage::new(1, 1))))
            .unwrap();
        let descriptor = [0x2c, 0, 0, 0, 0, 1, 0, 1, 0];
        while let Some(at) = gif.windows(9).position(|d| d == descriptor) {
            gif[at + 5..at + 9].fill(0xff);
        }
        let gif = refused("bomb.gif", &gif, (1, 1));
        let (sender, calls) = std::sync::mpsc::channel();
        let animation = Animation::start(gif.clone(), PLENTY, CAP, move |first, _, _| {
            let _ = sender.send(first);
        });
        assert_eq!(
            calls.recv_timeout(std::time::Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
            "the animation ends with no progress"
        );
        assert!(animation.frame(0).is_none(), "and keeps no frame");
        for path in [png, jpeg, webp, gif] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn png_and_lossless_webp_keep_the_color_under_transparent_pixels() {
        // green and blue at alpha 0 and white at half alpha, as the files store them; the
        // pyramid and the drawing weight color by alpha, but decoding changes nothing
        for name in ["hidden.png", "hidden.webp"] {
            assert_eq!(
                decode(&fixture(name), PLENTY).map(|i| i.rgba),
                Ok(vec![
                    255, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 0, 255, 255, 255, 128
                ]),
                "{name}"
            );
        }
    }

    #[test]
    fn a_16_bit_png_rounds_to_8_bits() {
        // 0x12ff is 18.92 in 8 bits, which rounds to 19. Imlib2 keeps the high byte, 18, so
        // it shows 16-bit files up to 1 darker than this decoder does.
        let path = temp_file("deep.png", &[]);
        image::ImageBuffer::<image::Rgba<u16>, _>::from_raw(
            1,
            1,
            vec![0x12ff, 0x1280, 0xff, 0xffff],
        )
        .unwrap()
        .save_with_format(&path, ImageFormat::Png)
        .unwrap();
        assert_eq!(
            decode(&path, PLENTY).map(|i| i.rgba),
            Ok(vec![19, 18, 1, 255])
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn display_p3_pixels_come_out_in_srgb() {
        let icc = ColorProfile::new_display_p3().encode().unwrap();
        let mut rgba = vec![128, 128, 128, 255, 200, 100, 100, 77];
        to_srgb(&mut rgba, 2, &icc);
        assert!(
            rgba[..3].iter().all(|&c| c.abs_diff(128) <= 1),
            "gray stays gray: {rgba:?}"
        );
        assert!(
            rgba[4] > 205 && rgba[5] < 97 && rgba[7] == 77,
            "a P3 red is more saturated in sRGB, alpha kept: {rgba:?}"
        );
        let before = rgba.clone();
        to_srgb(&mut rgba, 2, b"no profile");
        assert_eq!(
            rgba, before,
            "a profile moxcms cannot read leaves them alone"
        );
        // linear 0.22 is about half brightness, which sRGB stores as 129
        let linear = ColorProfile::new_gray_with_gamma(1.0).encode().unwrap();
        let mut gray = vec![56, 56, 56, 255];
        to_srgb(&mut gray, 1, &linear);
        assert!(
            gray[..3].iter().all(|&c| c.abs_diff(129) <= 2) && gray[3] == 255,
            "a gray profile maps its levels to sRGB's: {gray:?}"
        );
        let mut color = vec![56, 100, 200, 255];
        to_srgb(&mut color, 1, &linear);
        assert_eq!(color, [56, 100, 200, 255], "and leaves a color image alone");
    }
}
