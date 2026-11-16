//! SVG, drawn by resvg at the size its document gives, as Imlib2's loader draws it.
//!
//! In the viewer's process a crafted SVG could abort it through a chain of references, stall
//! a worker, or pass the memory cap, so the viewer sends SVG to a decoding helper
//! ([`crate::needs_helper`]), whose crashes and timeouts end only the helper.

use std::io::Read;
use std::sync::{Arc, OnceLock};

use image::error::{DecodingError, ImageFormatHint, LimitError, LimitErrorKind};
use image::{ColorType, ImageDecoder, ImageError, ImageFormat, ImageResult};

use crate::read_capped;
use resvg::tiny_skia::{PixmapMut, Transform};
use resvg::usvg::{self, fontdb};

pub(crate) const GZIP: &[u8] = &[0x1f, 0x8b];

// usvg recurses over the document, through nested text without any bound, and a worker's
// stack running out aborts the process, so a document nested deeper than drawings go is
// refused before usvg walks it.
const MAX_DEPTH: usize = 256;

/// Whether a file that starts so holds SVG, which has no signature: its text opens as XML
/// does, after any byte order mark and white space, and SVGZ is gzip. A lone `<` is no
/// sign, as a TGA whose image ID is 60 bytes long starts with one.
pub(crate) fn is_svg(start: &[u8]) -> bool {
    let text = start.strip_prefix(b"\xef\xbb\xbf").unwrap_or(start);
    let text = text.trim_ascii_start();
    [&b"<?xml"[..], b"<svg", b"<!--", b"<!DOCTYPE"]
        .iter()
        .any(|opening| text.starts_with(opening))
        || start.starts_with(GZIP)
}

pub(crate) struct SvgDecoder {
    tree: usvg::Tree,
    width: u32,
    height: u32,
}

impl SvgDecoder {
    /// Parses the document, one in SVGZ (`gzip`) decompressed no further than `max_bytes`.
    pub(crate) fn new(reader: impl Read, gzip: bool, max_bytes: u64) -> ImageResult<SvgDecoder> {
        let mut data = Vec::new();
        if gzip {
            // decompressed as it is read, so the compressed bytes never sit beside the text
            read_capped(flate2::read::GzDecoder::new(reader), &mut data, max_bytes)?;
        } else {
            read_capped(reader, &mut data, max_bytes)?;
        }
        let text = std::str::from_utf8(&data).map_err(svg_error)?;
        let xml = roxmltree::ParsingOptions {
            allow_dtd: true,
            ..roxmltree::ParsingOptions::default()
        };
        let document = roxmltree::Document::parse_with_options(text, xml).map_err(svg_error)?;
        // each node looks no further up than the limit, so the check stays cheap
        if document
            .descendants()
            .any(|node| node.ancestors().nth(MAX_DEPTH).is_some())
        {
            return Err(svg_error("nested too deeply"));
        }
        let options = usvg::Options {
            fontdb: fonts(),
            image_href_resolver: usvg::ImageHrefResolver {
                // Raster images embedded as data draw, picked by content; an SVG embedded in
                // an SVG does not, as usvg would parse it past the checks on this one.
                resolve_data: Box::new(|_, data, _| match image::guess_format(&data) {
                    Ok(ImageFormat::Jpeg) => Some(usvg::ImageKind::JPEG(data)),
                    Ok(ImageFormat::Png) => Some(usvg::ImageKind::PNG(data)),
                    Ok(ImageFormat::Gif) => Some(usvg::ImageKind::GIF(data)),
                    Ok(ImageFormat::WebP) => Some(usvg::ImageKind::WEBP(data)),
                    _ => None,
                }),
                // No file an SVG names gets read, as a FIFO would block the worker and
                // /dev/zero would never end.
                resolve_string: Box::new(|_, _| None),
            },
            ..usvg::Options::default()
        };
        let tree = usvg::Tree::from_xmltree(&document, &options).map_err(svg_error)?;
        let size = tree.size().to_int_size();
        Ok(SvgDecoder {
            width: size.width(),
            height: size.height(),
            tree,
        })
    }
}

fn svg_error(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> ImageError {
    ImageError::Decoding(DecodingError::new(
        ImageFormatHint::Name("SVG".to_owned()),
        e,
    ))
}

static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();

pub(crate) fn set_fonts(fonts: fontdb::Database) -> bool {
    FONTS.set(Arc::new(fonts)).is_ok()
}

// The system's fonts, for text in SVG files, loaded once unless set_fonts came first. The
// first SVG pays for the scan, on the worker that decodes it.
fn fonts() -> Arc<fontdb::Database> {
    Arc::clone(FONTS.get_or_init(|| {
        let mut fonts = fontdb::Database::new();
        fonts.load_system_fonts();
        Arc::new(fonts)
    }))
}

impl ImageDecoder for SvgDecoder {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let mut pixmap = PixmapMut::from_bytes(buf, self.width, self.height).ok_or_else(|| {
            ImageError::Limits(LimitError::from_kind(LimitErrorKind::DimensionError))
        })?;
        resvg::render(&self.tree, Transform::default(), &mut pixmap);
        // tiny-skia draws premultiplied alpha, and the pixels go out straight
        for pixel in buf.as_chunks_mut::<4>().0 {
            let alpha = u16::from(pixel[3]);
            if alpha != 0 && alpha != 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}
