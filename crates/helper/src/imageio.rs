//! HEIC and AVIF through ImageIO, in the viewer's own process: from the bytes of the file the
//! viewer opened, oriented as the file says, and drawn into straight-alpha RGBA in sRGB.

use std::ffi::c_void;
use std::fs::File;
use std::io::Read;

use nsxiv_ng_decode::{DecodeError, DecodeShare, Image};
use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect,
    CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, kCGColorSpaceSRGB,
};
use objc2_image_io::{
    CGImageSource, kCGImagePropertyOrientation, kCGImagePropertyPixelHeight,
    kCGImagePropertyPixelWidth, kCGImageSourceCreateThumbnailFromImageAlways,
    kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceShouldCacheImmediately,
    kCGImageSourceThumbnailMaxPixelSize,
};

/// Decodes a HEIC or AVIF file's primary image as straight-alpha RGBA in sRGB, refusing one
/// whose decode would pass `max_bytes`, at its full size, or with a `side` scaled down so its
/// longer side is that, where that is smaller, by ImageIO's own thumbnail call. With a
/// `share`, the decode waits, once the size is known, for room in its budget.
// shortcut: a HEIC sequence shows the image its source names primary; decode its frames as an
// animation once sequences are seen in use
pub(crate) fn decode(
    file: File,
    max_bytes: u64,
    side: u32,
    share: Option<&DecodeShare>,
) -> Result<Image, DecodeError> {
    // the file's bytes stand twice, read and copied for ImageIO, before any property is read:
    // a file that would pass the cap so is refused by its length, then by a read cut short, as
    // it may have grown
    let half = max_bytes / 2;
    let too_large = || DecodeError::TooLarge { size: None };
    let unread = |e: std::io::Error| DecodeError::Unreadable(e.to_string());
    if file.metadata().map_err(unread)?.len() > half {
        return Err(too_large());
    }
    let mut data = Vec::new();
    file.take(half.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(unread)?;
    if data.len() as u64 > half {
        return Err(too_large());
    }
    let bytes = CFData::from_bytes(&data);
    let file_bytes = data.len();
    drop(data);
    // SAFETY: no options, whose generics could be wrong
    let source = unsafe { CGImageSource::with_data(&bytes, None) }.ok_or_else(unreadable)?;
    // SAFETY: a source ImageIO made, read from this thread alone
    let index = unsafe {
        if source.count() == 0 {
            return Err(failed(&source));
        }
        source.primary_image_index()
    };
    // SAFETY: no options, whose generics could be wrong
    let properties =
        unsafe { source.properties_at_index(index, None) }.ok_or_else(|| failed(&source))?;
    // SAFETY: the keys of an image's properties are strings, and their values CF objects
    let properties = unsafe { properties.cast_unchecked::<CFString, CFType>() };
    let number = |key: &CFString| {
        let value = properties.get(key)?;
        value.downcast_ref::<CFNumber>()?.as_i64()
    };
    // SAFETY: ImageIO's own constant keys
    let (width, height, orientation) = unsafe {
        (
            number(kCGImagePropertyPixelWidth),
            number(kCGImagePropertyPixelHeight),
            number(kCGImagePropertyOrientation).unwrap_or(1),
        )
    };
    let (Some(width), Some(height)) = (
        width.and_then(|w| u32::try_from(w).ok()),
        height.and_then(|h| u32::try_from(h).ok()),
    ) else {
        return Err(failed(&source));
    };
    let plan = plan((width, height), orientation, side, file_bytes)
        .ok_or_else(|| DecodeError::Unreadable("the file gives its image no size".to_owned()))?;
    if plan.needed > max_bytes {
        return Err(DecodeError::TooLarge {
            size: Some(plan.full),
        });
    }
    if let Some(share) = share {
        share.take(plan.needed);
    }
    let image = thumbnail(&source, index, plan.side).ok_or_else(|| failed(&source))?;
    drop(source);
    // the image ImageIO gave, held to the size planned and paid for before its buffer is made
    let (width, height) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
    let (out_width, out_height) = plan.out;
    if width > out_width as usize || height > out_height as usize {
        return Err(DecodeError::TooLarge {
            size: Some(plan.full),
        });
    }
    draw(&image).ok_or_else(|| DecodeError::Unreadable("ImageIO drew nothing".to_owned()))
}

/// What a decode of an image `width × height`, turned as `orientation` says, gives for a
/// `side` of 0, the whole image, or with its longer side scaled down to `side`.
#[derive(Debug, PartialEq)]
struct Plan {
    /// The oriented size of the whole image.
    full: (u32, u32),
    /// The longer side asked of ImageIO.
    side: u32,
    /// The oriented size that comes out, rounded up, so ImageIO's rounding stays within it.
    out: (u32, u32),
    /// What the decode holds at its peak ([`peak`]).
    needed: u64,
}

// The plan for a decode, None for an image with no width or no height.
fn plan((width, height): (u32, u32), orientation: i64, side: u32, file: usize) -> Option<Plan> {
    if width == 0 || height == 0 {
        return None;
    }
    // orientations 5 to 8 turn the image a quarter
    let full = match orientation {
        5..=8 => (height, width),
        _ => (width, height),
    };
    let longer = full.0.max(full.1);
    let side = if side > 0 { side.min(longer) } else { longer };
    // ImageIO scales the longer side to `side`
    let scale = |n: u32| (u64::from(n) * u64::from(side)).div_ceil(u64::from(longer)) as u32;
    let out = (scale(full.0), scale(full.1));
    Some(Plan {
        full,
        side,
        out,
        needed: peak(full, out, file),
    })
}

// What a decode holds at its peak: ImageIO's decoded image beside the file it read, its copy as
// CFData, and the RGBA here. A thumbnail counts the full image too, as ImageIO decodes some
// streams whole in this process; those it hands VideoToolbox took far less (7 MiB for a 12 MP
// HEIC at 160 pixels). A decode that waits for room in its budget holds its file's bytes
// meanwhile, ImageIO's copy of them, outside the budget: no more than half the cap.
// shortcut: measured on a 12 and a 48 MP HEIC and AVIF on macOS 27, 12.9 and 12.5 bytes a
// pixel at the peak with the RGBA; measure again if ImageIO changes how it decodes them
fn peak(full: (u32, u32), out: (u32, u32), file: usize) -> u64 {
    let pixels = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
    (pixels(full).saturating_mul(FULL))
        .saturating_add(pixels(out).saturating_mul(4))
        .saturating_add((file as u64).saturating_mul(2))
}

// Bytes ImageIO holds for each pixel of the full image while it decodes, beside the RGBA.
const FULL: u64 = 9;

// The image at index `index`, oriented, its longer side `side`: ImageIO's thumbnail call, made
// from the full image, which applies the file's orientation in the same step.
fn thumbnail(source: &CGImageSource, index: usize, side: u32) -> Option<CFRetained<CGImage>> {
    let always = CFBoolean::new(true);
    let size = CFNumber::new_i64(i64::from(side));
    // SAFETY: ImageIO's own constant keys
    let keys: [&CFString; 4] = unsafe {
        [
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceShouldCacheImmediately,
            kCGImageSourceThumbnailMaxPixelSize,
        ]
    };
    let values: [&CFType; 4] = [always, always, always, &size];
    let options = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    // SAFETY: the options' keys are strings and their values CF objects, as ImageIO reads them
    unsafe { source.thumbnail_at_index(index, Some(options.as_opaque())) }
}

// The image drawn into 8-bit RGBA in sRGB, its alpha straight; ColorSync converts its color.
fn draw(image: &CGImage) -> Option<Image> {
    let (width, height) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let row = width.checked_mul(4)?;
    let mut rgba = vec![0u8; row.checked_mul(height)?];
    // SAFETY: CoreGraphics' own constant name
    let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: the buffer holds `height` rows of `row` bytes and outlives the context, which
    // is dropped before it
    let context: CFRetained<CGContext> = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast::<c_void>(),
            width,
            height,
            8,
            row,
            Some(&srgb),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    let all = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::draw_image(Some(&context), all, Some(image));
    drop(context);
    nsxiv_ng_decode::unpremultiply(&mut rgba);
    Some(Image {
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        rgba,
        may_animate: false,
    })
}

fn unreadable() -> DecodeError {
    DecodeError::Unreadable("ImageIO cannot read the file".to_owned())
}

// Why ImageIO gave nothing for the source.
fn failed(source: &CGImageSource) -> DecodeError {
    // SAFETY: a source ImageIO made
    let status = unsafe { source.status() };
    DecodeError::Unreadable(format!(
        "ImageIO cannot decode the file (status {})",
        status.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CFMutableData;
    use objc2_core_graphics::CGBitmapContextCreateImage;
    use objc2_image_io::CGImageDestination;
    use std::io::Seek;

    // A HEIC of 64 × 32 pixels drawn by `paint`, with orientation `orientation`.
    fn heic(orientation: i32, paint: impl Fn(&CGContext)) -> Vec<u8> {
        // SAFETY: CoreGraphics' own constant name
        let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
        // SAFETY: no buffer, so CoreGraphics allocates its own
        let context = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                64,
                32,
                8,
                0,
                Some(&srgb),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .unwrap();
        paint(&context);
        let image = CGBitmapContextCreateImage(Some(&context)).unwrap();
        let data = CFMutableData::new(None, 0).unwrap();
        let heic = CFString::from_static_str("public.heic");
        let orientation = CFNumber::new_i32(orientation);
        // SAFETY: ImageIO's own constant key, and a property dictionary of CF objects
        unsafe {
            let destination = CGImageDestination::with_data(&data, &heic, 1, None).unwrap();
            let properties = CFDictionary::<CFString, CFType>::from_slices(
                &[kCGImagePropertyOrientation],
                &[&orientation],
            );
            destination.add_image(&image, Some(properties.as_opaque()));
            assert!(destination.finalize());
        }
        data.to_vec()
    }

    // The HEIC decoded from a file of its own.
    fn decoded(name: &str, heic: Vec<u8>) -> Image {
        let path =
            std::env::temp_dir().join(format!("nsxiv-ng-{name}-{}.heic", std::process::id()));
        std::fs::write(&path, heic).unwrap();
        let image = decode(File::open(&path).unwrap(), 1 << 30, 0, None).unwrap();
        std::fs::remove_file(&path).unwrap();
        image
    }

    #[test]
    fn a_half_clear_color_comes_back_straight() {
        let image = decoded(
            "half-clear",
            heic(1, |context| {
                let all = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(64.0, 32.0));
                CGContext::set_rgb_fill_color(Some(context), 1.0, 0.0, 0.0, 0.5);
                CGContext::fill_rect(Some(context), all);
            }),
        );
        let middle = ((16 * image.width + 32) * 4) as usize;
        let pixel = &image.rgba[middle..middle + 4];
        // red at half alpha, not red premultiplied to half its value
        assert!(
            pixel[0] > 230 && (100..156).contains(&pixel[3]),
            "{pixel:?}"
        );
    }

    #[test]
    fn a_heic_turns_as_its_orientation_says() {
        // red on its left half and blue on its right, to be turned a quarter clockwise
        let image = decoded(
            "turned",
            heic(6, |context| {
                let half = |x: f64| CGRect::new(CGPoint::new(x, 0.0), CGSize::new(32.0, 32.0));
                CGContext::set_rgb_fill_color(Some(context), 1.0, 0.0, 0.0, 1.0);
                CGContext::fill_rect(Some(context), half(0.0));
                CGContext::set_rgb_fill_color(Some(context), 0.0, 0.0, 1.0, 1.0);
                CGContext::fill_rect(Some(context), half(32.0));
            }),
        );
        assert_eq!((image.width, image.height), (32, 64));
        let at = |x: u32, y: u32| {
            let i = ((y * image.width + x) * 4) as usize;
            [image.rgba[i], image.rgba[i + 1], image.rgba[i + 2]]
        };
        // the left half, red, now on top, and the blue below it
        let (top, bottom) = (at(16, 4), at(16, 59));
        assert!(top[0] > 200 && top[2] < 60, "{top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 60, "{bottom:?}");
    }

    #[test]
    fn a_plan_turns_scales_and_counts_the_image_and_refuses_one_of_no_size() {
        assert_eq!(plan((0, 200), 1, 0, 10), None);
        assert_eq!(plan((320, 0), 1, 64, 10), None);
        // both, which divided zero by zero
        assert_eq!(plan((0, 0), 1, 0, 10), None);
        assert_eq!(plan((0, 0), 6, 64, 10), None);
        // whole, and turned a quarter: the plan's sizes are the image's as shown
        let whole = plan((4000, 3000), 1, 0, 1000).unwrap();
        assert_eq!(
            (whole.full, whole.side, whole.out),
            ((4000, 3000), 4000, (4000, 3000))
        );
        assert_eq!(whole.needed, 12_000_000 * (FULL + 4) + 2000);
        let turned = plan((4000, 3000), 6, 0, 1000).unwrap();
        assert_eq!((turned.full, turned.out), ((3000, 4000), (3000, 4000)));
        // a side past the image gives the whole image, and one that does not divide it evenly
        // rounds up
        assert_eq!(plan((320, 200), 1, 1000, 0).unwrap().out, (320, 200));
        let scaled = plan((320, 200), 1, 65, 0).unwrap();
        assert_eq!((scaled.side, scaled.out), (65, (65, 41)));
        assert_eq!(scaled.needed, 64_000 * FULL + 65 * 41 * 4);
        assert_eq!(plan((200, 320), 8, 65, 0).unwrap().out, (65, 41));
    }

    #[test]
    fn a_file_whose_bytes_twice_pass_the_cap_is_refused_before_it_is_read() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fixtures/gradient.heic");
        let length = std::fs::metadata(&path).unwrap().len();
        let open = || File::open(&path).unwrap();
        // a second handle on the same open file, which shares its position
        let file = open();
        let mut kept = file.try_clone().unwrap();
        assert_eq!(
            decode(file, 2 * length - 1, 0, None).map(|i| i.width),
            Err(DecodeError::TooLarge { size: None })
        );
        assert_eq!(kept.stream_position().unwrap(), 0, "no byte of it was read");
        // twice its length passes, and the pixels then tell
        assert!(matches!(
            decode(open(), 2 * length, 0, None),
            Err(DecodeError::TooLarge {
                size: Some((320, 200))
            })
        ));
    }

    #[test]
    fn a_file_whose_length_says_nothing_is_cut_short_once_it_gives_more_than_half_the_cap() {
        // a pipe's length reads as zero; it gives one byte more than half a cap of 2048
        let (reader, mut writer) = std::io::pipe().unwrap();
        let feeder = std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut writer, &[0; 1025]);
        });
        let file = File::from(std::os::fd::OwnedFd::from(reader));
        assert_eq!(
            decode(file, 2048, 0, None).map(|i| i.width),
            Err(DecodeError::TooLarge { size: None })
        );
        feeder.join().unwrap();
    }
}
