//! JPEG stored as YCbCr decoded by zune-jpeg straight into RGBA, where the image crate decodes
//! into RGB, which then takes a copy as RGBA. The options are the image crate's, so the pixels
//! are the same bytes; other JPEG goes through the image crate as before.

use std::io::{Read, Seek};

use image::error::{
    DecodingError, ImageFormatHint, LimitError, LimitErrorKind, UnsupportedError,
    UnsupportedErrorKind,
};
use image::{ColorType, ImageDecoder, ImageError, ImageFormat, ImageResult, LimitSupport, Limits};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

/// A JPEG stored as YCbCr, whose pixels decode straight into RGBA.
pub(crate) struct Rgba {
    input: Vec<u8>,
    width: u32,
    height: u32,
    icc: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
}

/// What a JPEG file opens as: one that decodes straight into RGBA, or one for the image crate,
/// as stored as RGB, gray, CMYK or YCCK, where zune-jpeg maps no colors into RGBA.
pub(crate) enum Opened {
    Rgba(Rgba),
    Other,
}

// the image crate's options for its zune decoder, with no limits of their own
fn options() -> DecoderOptions {
    DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(usize::MAX)
        .set_max_height(usize::MAX)
}

// the image crate's errors for zune's, as it maps them
fn error(e: zune_jpeg::errors::DecodeErrors) -> ImageError {
    use zune_jpeg::errors::DecodeErrors;
    match e {
        DecodeErrors::Unsupported(desc) => {
            ImageError::Unsupported(UnsupportedError::from_format_and_kind(
                ImageFormatHint::Exact(ImageFormat::Jpeg),
                UnsupportedErrorKind::GenericFeature(format!("{desc:?}")),
            ))
        }
        DecodeErrors::LargeDimensions(_) => {
            ImageError::Limits(LimitError::from_kind(LimitErrorKind::DimensionError))
        }
        e => ImageError::Decoding(DecodingError::new(
            ImageFormatHint::Exact(ImageFormat::Jpeg),
            e,
        )),
    }
}

/// Reads the whole file, as the image crate's decoder does, and its headers. A file the image
/// crate takes is left at its start for it, and its bytes read here are gone.
pub(crate) fn open(reader: &mut (impl Read + Seek)) -> ImageResult<Opened> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    match take(input)? {
        Ok(rgba) => Ok(Opened::Rgba(rgba)),
        Err(_) => {
            reader.rewind()?;
            Ok(Opened::Other)
        }
    }
}

/// A JPEG's bytes, which the decoder that takes YCbCr straight into RGBA owns, with no copy;
/// any other JPEG's come back, for the image crate.
pub(crate) fn take(input: Vec<u8>) -> ImageResult<Result<Rgba, Vec<u8>>> {
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(input.as_slice()), options());
    decoder.decode_headers().map_err(error)?;
    if decoder.input_colorspace() != Some(ColorSpace::YCbCr) {
        return Ok(Err(input));
    }
    let (width, height) = decoder.dimensions().expect("headers were decoded");
    let (icc, exif) = (decoder.icc_profile(), decoder.exif().cloned());
    drop(decoder);
    Ok(Ok(Rgba {
        input,
        width: width as u32,
        height: height as u32,
        icc,
        exif,
    }))
}

impl ImageDecoder for Rgba {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.icc.clone())
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.exif.clone())
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let options = options().jpeg_set_out_colorspace(ColorSpace::RGBA);
        let mut decoder =
            JpegDecoder::new_with_options(ZCursor::new(self.input.as_slice()), options);
        decoder.decode_into(buf).map_err(error)
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&LimitSupport::default())?;
        limits.check_dimensions(self.width, self.height)
    }
}
