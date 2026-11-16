//! JPEG XL, through jxl-rs, which turns the image as its header asks and composes its frames.
//!
//! shortcut: an animated JPEG XL shows its first frame; teach `Animation` its frames once such
//! files turn up.

use std::fs::File;
use std::io::BufReader;

use image::error::{DecodingError, ImageFormatHint, LimitError, LimitErrorKind};
use image::{ColorType, ImageDecoder, ImageError, ImageResult};
use jxl::api::{
    AlphaMode, Event, ExtraChannel, JxlColorEncoding, JxlColorProfile, JxlColorType, JxlDataFormat,
    JxlDecoderOptions, JxlOutputBuffer, JxlPixelFormat, JxlPrimaries, JxlTransferFunction,
    JxlWhitePoint,
};

/// Whether a file that starts so holds JPEG XL: a bare codestream, or one in its container.
pub(crate) fn is_jxl(start: &[u8]) -> bool {
    start.starts_with(&[0xff, 0x0a]) || start.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n")
}

pub(crate) struct JxlDecoder {
    decoder: jxl::api::JxlDecoder,
    reader: BufReader<File>,
    width: u32,
    height: u32,
    gray: bool,
    icc: Option<Vec<u8>>,
}

impl JxlDecoder {
    /// Reads the header. Images with more samples than `max_bytes` holds at four bytes each
    /// fail inside the decoder, as its float planes take that much.
    pub(crate) fn new(mut reader: BufReader<File>, max_bytes: u64) -> ImageResult<JxlDecoder> {
        let mut options = JxlDecoderOptions::default();
        // straight alpha, as every decoder here gives it
        options.alpha_mode = AlphaMode::Unpremultiplied;
        options.sample_limit = Some(usize::try_from(max_bytes / 4).unwrap_or(usize::MAX));
        let mut decoder = jxl::api::JxlDecoder::new(options);
        process_until(&mut decoder, &mut reader, None, |e| e == Event::BasicInfo)?;
        let info = decoder
            .basic_info()
            .ok_or_else(|| jxl_error("no basic info"))?;
        let size = |n: usize| u32::try_from(n).map_err(|_| jxl_error("too wide or too tall"));
        let (width, height) = (size(info.size.0)?, size(info.size.1)?);
        // jxl-rs gives CMYK as RGB from its first three channels, without the black one
        if info
            .extra_channels
            .iter()
            .any(|channel| channel.ec_type == ExtraChannel::Black)
        {
            return Err(jxl_error("CMYK"));
        }
        let extra_channels = info.extra_channels.len();
        // the decoder's default format is gray for a gray image, which comes out gray and alpha
        let gray = decoder.current_pixel_format().is_some_and(|format| {
            matches!(
                format.color_type,
                JxlColorType::Grayscale | JxlColorType::GrayscaleAlpha
            )
        });
        let format = JxlPixelFormat {
            color_type: if gray {
                JxlColorType::GrayscaleAlpha
            } else {
                JxlColorType::Rgba
            },
            color_data_format: Some(JxlDataFormat::U8 { bit_depth: 8 }),
            extra_channel_format: vec![None; extra_channels],
        };
        decoder.set_pixel_format(format).map_err(from_jxl)?;
        // The decoder keeps the file's own color space where it can give it, as a P3 file's;
        // to_srgb converts it from there as it does for every format.
        if decoder.output_color_profile().is_some_and(misread_hdr) {
            return Err(jxl_error("HDR whose profile would come out wrong"));
        }
        let srgb = JxlColorProfile::Simple(JxlColorEncoding::srgb(gray));
        let icc = decoder
            .output_color_profile()
            .filter(|profile| **profile != srgb)
            .and_then(JxlColorProfile::try_as_icc)
            .map(|icc| icc.into_owned());
        Ok(JxlDecoder {
            decoder,
            reader,
            width,
            height,
            gray,
            icc,
        })
    }
}

// Whether the profile is PQ or HLG of a kind jxl-rs cannot tone map into the ICC profile it
// writes: for those it writes a sampled curve that moxcms misreads. The rule is jxl-rs's own,
// from its private can_tone_map_for_icc: RGB with standard primaries and a D65 white point,
// or P3 with DCI's.
fn misread_hdr(profile: &JxlColorProfile) -> bool {
    let JxlColorProfile::Simple(encoding) = profile else {
        return false;
    };
    let transfer = match encoding {
        JxlColorEncoding::RgbColorSpace {
            transfer_function, ..
        }
        | JxlColorEncoding::GrayscaleColorSpace {
            transfer_function, ..
        } => transfer_function,
        JxlColorEncoding::XYB { .. } => return false,
    };
    let hdr = matches!(transfer, JxlTransferFunction::PQ | JxlTransferFunction::HLG);
    let tone_mapped = matches!(
        encoding,
        JxlColorEncoding::RgbColorSpace { white_point, primaries, .. }
            if !matches!(primaries, JxlPrimaries::Chromaticities { .. })
                && (*white_point == JxlWhitePoint::D65
                    || (*white_point == JxlWhitePoint::DCI && *primaries == JxlPrimaries::P3))
    );
    hdr && !tone_mapped
}

// Runs the decoder until `done` takes an event; running out of input before then means the
// file ends early.
fn process_until(
    decoder: &mut jxl::api::JxlDecoder,
    reader: &mut BufReader<File>,
    mut buffers: Option<&mut [JxlOutputBuffer]>,
    done: impl Fn(Event) -> bool,
) -> ImageResult<()> {
    loop {
        let event = decoder
            .process(reader, buffers.as_deref_mut(), None)
            .map_err(from_jxl)?;
        if done(event) {
            return Ok(());
        }
        if matches!(event, Event::NeedMoreInput { .. } | Event::Complete) {
            return Err(jxl_error("the file ends early"));
        }
    }
}

// the decoder's sample limit, which comes from the cap, refuses as the cap does
fn from_jxl(e: jxl::error::Error) -> ImageError {
    match e {
        jxl::error::Error::ImageSizeTooLarge(..) => {
            ImageError::Limits(LimitError::from_kind(LimitErrorKind::InsufficientMemory))
        }
        e => jxl_error(e),
    }
}

fn jxl_error(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> ImageError {
    ImageError::Decoding(DecodingError::new(
        ImageFormatHint::Name("JPEG XL".to_owned()),
        e,
    ))
}

impl ImageDecoder for JxlDecoder {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        if self.gray {
            ColorType::La8
        } else {
            ColorType::Rgba8
        }
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.icc.clone())
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        let row = self.width as usize * self.color_type().bytes_per_pixel() as usize;
        let mut buffers = [JxlOutputBuffer::new(buf, self.height as usize, row)];
        process_until(
            &mut self.decoder,
            &mut self.reader,
            Some(&mut buffers),
            |e| matches!(e, Event::FrameComplete { .. }),
        )
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jxl::api::RenderingIntent;

    #[test]
    fn hdr_converts_only_where_jxl_rs_tone_maps_it() {
        let rgb = |white_point, primaries, transfer_function| {
            JxlColorProfile::Simple(JxlColorEncoding::RgbColorSpace {
                white_point,
                primaries,
                transfer_function,
                rendering_intent: RenderingIntent::Relative,
            })
        };
        let gray_pq = JxlColorProfile::Simple(JxlColorEncoding::GrayscaleColorSpace {
            white_point: JxlWhitePoint::D65,
            transfer_function: JxlTransferFunction::PQ,
            rendering_intent: RenderingIntent::Relative,
        });
        let (pq, hlg) = (JxlTransferFunction::PQ, JxlTransferFunction::HLG);
        assert!(!misread_hdr(&rgb(
            JxlWhitePoint::D65,
            JxlPrimaries::BT2100,
            pq.clone()
        )));
        assert!(!misread_hdr(&rgb(
            JxlWhitePoint::DCI,
            JxlPrimaries::P3,
            hlg
        )));
        assert!(!misread_hdr(&rgb(
            JxlWhitePoint::E,
            JxlPrimaries::P3,
            JxlTransferFunction::SRGB
        )));
        assert!(misread_hdr(&rgb(
            JxlWhitePoint::E,
            JxlPrimaries::BT2100,
            pq
        )));
        assert!(misread_hdr(&gray_pq));
    }
}
