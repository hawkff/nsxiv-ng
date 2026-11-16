//! The session's pixels as the image a layer shows.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{
    CFDictionary, CFNumber, CFRetained, CFString, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_io_surface::{
    IOSurfaceLockOptions, IOSurfaceRef, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow,
    kIOSurfaceColorSpace, kIOSurfaceHeight, kIOSurfacePixelFormat, kIOSurfaceWidth,
};

/// An image of `pixels`, rows `width` pixels wide as [`nsxiv_ng_render::Canvas`] holds them,
/// tagged sRGB, which owns the pixels from now on: a layer may read its image again at any
/// time, so the pixels must stay as they are while it lives, and go with it.
pub(crate) fn image(pixels: Vec<u32>, width: usize) -> Option<CFRetained<CGImage>> {
    if width == 0 || pixels.is_empty() || !pixels.len().is_multiple_of(width) {
        return None;
    }
    let height = pixels.len() / width;
    let bytes = pixels.len() * 4;
    let pixels = Box::new(pixels);
    let data = pixels.as_ptr().cast::<c_void>();
    let info = Box::into_raw(pixels).cast::<c_void>();
    // SAFETY: `data` is `bytes` long and lives in the box behind `info` until `release`
    // frees it, which the provider calls once when it goes
    let provider = unsafe { CGDataProvider::with_data(info, data, bytes, Some(release)) };
    let Some(provider) = provider else {
        // SAFETY: no provider took the box, so it is still this function's to free
        unsafe { drop(Box::from_raw(info.cast::<Vec<u32>>())) };
        return None;
    };
    let space = srgb()?;
    // premultiplied 0xAARRGGBB in the machine's byte order, which is little endian
    let order = CGImageByteOrderInfo::Order32Little.0;
    let format = CGBitmapInfo(CGImageAlphaInfo::PremultipliedFirst.0 | order);
    // SAFETY: the provider holds width x height pixels of 32 bits, rows without padding, and
    // the decode array may be null
    unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            width * 4,
            Some(&space),
            format,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

fn srgb() -> Option<CFRetained<CGColorSpace>> {
    // SAFETY: a constant string Core Graphics defines
    CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
}

// Frees the pixels an image owned, once its provider goes.
unsafe extern "C-unwind" fn release(info: *mut c_void, _: NonNull<c_void>, _: usize) {
    // SAFETY: `info` is the box `image` gave the provider, which calls this once
    unsafe { drop(Box::from_raw(info.cast::<Vec<u32>>())) };
}

/// How many surfaces frames take turns in at most: a stream of frames at 3024 × 1964 pixels
/// kept three in the window server's use at once, at 5120 × 2880 four.
pub(crate) const SURFACES: usize = 4;

/// The surfaces the window's frames are drawn into, in the window's size in pixels, and the
/// one a layer shows, which holds the last frame drawn.
pub(crate) struct Surfaces {
    list: Vec<CFRetained<IOSurfaceRef>>,
    size: (u32, u32),
    shown: Option<usize>,
}

impl Surfaces {
    pub(crate) fn new() -> Surfaces {
        Surfaces {
            list: Vec::new(),
            size: (0, 0),
            shown: None,
        }
    }

    /// A surface of `size` pixels to draw a frame into: the first the window server no
    /// longer uses, other than the one shown, or a new one where none is free and fewer than
    /// [`SURFACES`] are kept; none past that. A new size lets every surface go first.
    pub(crate) fn free(&mut self, size: (u32, u32)) -> Option<usize> {
        if size != self.size {
            self.clear();
            self.size = size;
        }
        let free =
            (0..self.list.len()).find(|&i| Some(i) != self.shown && !self.list[i].is_in_use());
        if free.is_some() || self.list.len() >= SURFACES {
            return free;
        }
        self.list.push(surface(size.0, size.1)?);
        Some(self.list.len() - 1)
    }

    pub(crate) fn get(&self, i: usize) -> &IOSurfaceRef {
        &self.list[i]
    }

    /// The surface shown, which holds the last frame drawn, if any does.
    pub(crate) fn shown(&self) -> Option<usize> {
        self.shown
    }

    /// The surface `i` is shown from now on, or, with none, no surface holds the last frame.
    pub(crate) fn show(&mut self, i: Option<usize>) {
        self.shown = i;
    }

    /// Lets every surface go.
    pub(crate) fn clear(&mut self) {
        self.list.clear();
        self.shown = None;
    }
}

// A surface of `width` × `height` premultiplied BGRA pixels, rows unpadded as a canvas holds
// them, tagged sRGB as the frame's image was.
fn surface(width: u32, height: u32) -> Option<CFRetained<IOSurfaceRef>> {
    let surface = bgra(width, height, width * 4)?;
    if !unpadded(&surface) {
        return None;
    }
    let space = srgb()?.property_list()?;
    // SAFETY: the color space key with a color space's property list, as IOSurface takes it
    unsafe { surface.set_value(kIOSurfaceColorSpace, &space) };
    Some(surface)
}

// A surface of `width` × `height` BGRA pixels, asked for with rows `row` bytes apart.
fn bgra(width: u32, height: u32, row: u32) -> Option<CFRetained<IOSurfaceRef>> {
    let number = |n: u32| CFNumber::new_i32(i32::try_from(n).ok()?).into();
    let (Some(w), Some(h), Some(row)) = (number(width), number(height), number(row)) else {
        return None;
    };
    let bytes = CFNumber::new_i32(4);
    // 'BGRA', the pixel format of the canvas's words in memory
    let format = CFNumber::new_i32(0x4247_5241);
    // SAFETY: constant strings IOSurface defines
    let keys: [&CFString; 5] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfaceBytesPerRow,
            kIOSurfacePixelFormat,
        ]
    };
    let values: [&CFNumber; 5] = [&w, &h, &bytes, &row, &format];
    let properties = CFDictionary::from_slices(&keys, &values);
    // SAFETY: a dictionary of IOSurface's keys with numbers
    unsafe { IOSurfaceRef::new(properties.as_opaque()) }
}

// Whether `surface` holds 4-byte pixels in rows with no bytes between them, as a canvas does.
fn unpadded(surface: &IOSurfaceRef) -> bool {
    surface.bytes_per_element() == 4 && surface.bytes_per_row() == surface.width() * 4
}

/// Draws into `surface`'s pixels through `draw`, locked for it, and says whether it did:
/// not where its rows are padded or it cannot be locked, which leaves it as it was.
pub(crate) fn draw_into(surface: &IOSurfaceRef, draw: impl FnOnce(&mut [u32])) -> bool {
    if !unpadded(surface) {
        return false;
    }
    let len = surface.width() * surface.height();
    // SAFETY: locked for writing, with no seed asked for
    if unsafe { surface.lock(IOSurfaceLockOptions(0), std::ptr::null_mut()) } != 0 {
        return false;
    }
    // SAFETY: the surface's memory is `height` rows of `width` 4-byte pixels with nothing
    // between them, as checked above, so `len` words from its base; locked for writing until
    // the unlock below. No one else touches it meanwhile: the app draws only into the surface
    // `Surfaces::free` gave, which the window server no longer uses and which is not the one
    // shown, and `copy` reads from another surface than this
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(surface.base_address().as_ptr().cast(), len) };
    draw(pixels);
    // SAFETY: as locked above
    unsafe { surface.unlock(IOSurfaceLockOptions(0), std::ptr::null_mut()) };
    true
}

/// Copies the pixels of `from` into `to`, another surface of the same size, and says whether
/// it did: not where either cannot be locked, or their sizes or layouts differ, which leaves
/// `to` as it was.
pub(crate) fn copy(from: &IOSurfaceRef, to: &IOSurfaceRef) -> bool {
    if std::ptr::eq(from, to)
        || (from.width(), from.height()) != (to.width(), to.height())
        || !unpadded(from)
    {
        return false;
    }
    let read = IOSurfaceLockOptions::ReadOnly;
    // SAFETY: locked for reading, with no seed asked for
    if unsafe { from.lock(read, std::ptr::null_mut()) } != 0 {
        return false;
    }
    let len = from.width() * from.height();
    let copied = draw_into(to, |pixels| {
        // SAFETY: `from`'s memory is `len` words in unpadded rows, as checked above, locked
        // for reading until the unlock below; it is not `to`, checked above, so this slice
        // and the one written do not overlap
        let source =
            unsafe { std::slice::from_raw_parts(from.base_address().as_ptr().cast(), len) };
        pixels.copy_from_slice(source);
    });
    // SAFETY: as locked above
    unsafe { from.unlock(read, std::ptr::null_mut()) };
    copied
}

/// Rows `top..top + rows` of `image`, which share its pixels.
pub(crate) fn rows(image: &CGImage, top: usize, rows: usize) -> Option<CFRetained<CGImage>> {
    let width = CGImage::width(Some(image));
    let rect = CGRect::new(
        CGPoint::new(0.0, top as f64),
        CGSize::new(width as f64, rows as f64),
    );
    CGImage::with_image_in_rect(Some(image), rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_graphics::{CGBitmapContextCreate, CGContext};

    // The image's pixels as RGBA bytes in sRGB, as a bitmap context draws them.
    fn rgba(image: &CGImage) -> Vec<u8> {
        let (width, height) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
        let mut bytes = vec![0u8; width * height * 4];
        let space = srgb().unwrap();
        // SAFETY: `bytes` holds width x height pixels of four bytes and outlives the context
        let context = unsafe {
            CGBitmapContextCreate(
                bytes.as_mut_ptr().cast(),
                width,
                height,
                8,
                width * 4,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .unwrap();
        let all = CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64));
        CGContext::draw_image(Some(&context), all, Some(image));
        drop(context);
        bytes
    }

    #[test]
    fn a_frame_is_drawn_into_a_surface_the_window_server_no_longer_uses() {
        let mut surfaces = Surfaces::new();
        let size = (64, 48);
        let first = surfaces.free(size).unwrap();
        surfaces.show(Some(first));
        // the one shown is not drawn into, whether in use or not
        let second = surfaces.free(size).unwrap();
        assert_ne!(second, first);
        // one in use is not drawn into: a new one comes, up to the bound
        surfaces.get(second).increment_use_count();
        let third = surfaces.free(size).unwrap();
        assert!(third != first && third != second);
        surfaces.get(third).increment_use_count();
        let fourth = surfaces.free(size).unwrap();
        surfaces.get(fourth).increment_use_count();
        assert_eq!(surfaces.list.len(), SURFACES);
        assert_eq!(surfaces.free(size), None, "past the bound, none");
        assert_eq!(surfaces.list.len(), SURFACES, "and none made");
        // one the window server gave back is drawn into again
        surfaces.get(third).decrement_use_count();
        assert_eq!(surfaces.free(size), Some(third));
        // a surface holds the canvas's words as drawn, and a copy carries them over
        let words: Vec<u32> = (0..64 * 48).collect();
        assert!(draw_into(surfaces.get(third), |pixels| pixels.copy_from_slice(&words)));
        assert!(copy(surfaces.get(third), surfaces.get(first)));
        let mut read = Vec::new();
        assert!(draw_into(surfaces.get(first), |pixels| read.extend_from_slice(pixels)));
        assert_eq!(read, words);
        // a new size lets them all go
        assert_eq!(surfaces.free((32, 32)), Some(0));
        assert_eq!((surfaces.list.len(), surfaces.shown()), (1, None));
    }

    #[test]
    fn a_surface_neither_copy_nor_draw_can_use_says_so_and_stays_as_it_was() {
        let (one, other) = (surface(16, 8).unwrap(), surface(16, 8).unwrap());
        let words: Vec<u32> = (0..16 * 8).collect();
        assert!(draw_into(&one, |pixels| pixels.copy_from_slice(&words)));
        assert!(draw_into(&other, |pixels| pixels.fill(7)));
        let contents = |surface: &IOSurfaceRef| {
            let mut read = Vec::new();
            assert!(draw_into(surface, |pixels| read.extend_from_slice(pixels)));
            read
        };
        // onto itself, or onto another size, nothing is copied
        assert!(!copy(&one, &one), "onto itself");
        assert_eq!(contents(&one), words);
        assert!(!copy(&surface(8, 8).unwrap(), &other), "another size");
        assert_eq!(contents(&other), [7; 16 * 8]);
        // rows with bytes between them are no canvas's: never drawn into, nor copied from
        let padded = bgra(16, 8, 16 * 4 + 64).unwrap();
        assert_ne!(padded.bytes_per_row(), 16 * 4);
        let mut drawn = false;
        assert!(!draw_into(&padded, |_| drawn = true), "padded rows");
        assert!(!drawn);
        assert!(!copy(&padded, &other), "from padded rows");
        assert_eq!(contents(&other), [7; 16 * 8]);
    }

    #[test]
    fn the_canvas_words_are_the_colors_the_image_shows() {
        // two rows: opaque 0x123456 and half-transparent premultiplied red, then white and black
        let pixels = vec![0xff12_3456, 0x8080_0000, 0xffff_ffff, 0xff00_0000];
        let image = image(pixels, 2).unwrap();
        assert_eq!(
            (CGImage::width(Some(&image)), CGImage::height(Some(&image))),
            (2, 2)
        );
        assert_eq!(
            rgba(&image),
            [
                0x12, 0x34, 0x56, 0xff, 0x80, 0, 0, 0x80, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0xff
            ]
        );
        // the second row alone, the bar's rows of a frame
        let bar = rows(&image, 1, 1).unwrap();
        assert_eq!(rgba(&bar), [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0xff]);
        assert!(super::image(vec![0; 3], 2).is_none(), "not whole rows");
        assert!(super::image(Vec::new(), 2).is_none());
    }
}
