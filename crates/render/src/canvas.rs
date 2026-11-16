//! The frame a frontend shows, which the drawing modules fill.

/// A frame's pixels, lent by the frontend that shows them, as a Wayland buffer or a window's
/// image: rows top to bottom with no padding between them, one `u32` a pixel, holding
/// premultiplied `0xAARRGGBB`. On a little-endian machine those are the bytes of Wayland's
/// ARGB8888, and of an X11 image of 32 bits a pixel whose red, green and blue masks are
/// `0xff0000`, `0xff00` and `0xff`, so a frame is drawn where it is shown. A frontend whose
/// pixels lie otherwise converts.
pub struct Canvas<'a> {
    width: u32,
    height: u32,
    pixels: &'a mut [u32],
}

impl<'a> Canvas<'a> {
    /// A canvas over `pixels`, or None unless they are `width × height` of them.
    pub fn new(pixels: &'a mut [u32], width: u32, height: u32) -> Option<Self> {
        (pixels.len() as u64 == u64::from(width) * u64::from(height)).then_some(Canvas {
            width,
            height,
            pixels,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u32] {
        self.pixels
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.pixels
    }

    /// Sets the pixels of the rectangle that lie on the canvas, replacing what was there.
    pub fn fill(&mut self, x: i32, y: i32, width: i32, height: i32, color: u32) {
        let clip = |start: i32, length: i32, limit: u32| {
            let from = i64::from(start).clamp(0, i64::from(limit));
            let to = (i64::from(start) + i64::from(length)).clamp(from, i64::from(limit));
            from as usize..to as usize
        };
        let (columns, rows) = (clip(x, width, self.width), clip(y, height, self.height));
        let stride = self.width as usize;
        for row in rows {
            self.pixels[row * stride..][columns.clone()].fill(color);
        }
    }
}

/// An opaque color as a canvas pixel.
pub const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    0xff00_0000 | (red as u32) << 16 | (green as u32) << 8 | blue as u32
}

/// `source` drawn over `under`, both premultiplied.
pub fn over(source: u32, under: u32) -> u32 {
    let keep = 255 - (source >> 24);
    let mut pixel = 0;
    for shift in [0, 8, 16, 24] {
        // the part of the pixel under that shows through, divided by 255 and rounded
        let kept = ((under >> shift) & 0xff) * keep + 128;
        let kept = (kept + (kept >> 8)) >> 8;
        pixel |= (((source >> shift) & 0xff) + kept).min(255) << shift;
    }
    pixel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_canvas_needs_exactly_its_pixels() {
        let mut pixels = [0; 6];
        assert!(Canvas::new(&mut pixels, 3, 2).is_some());
        assert!(Canvas::new(&mut pixels, 3, 3).is_none());
        assert!(Canvas::new(&mut pixels, 2, 2).is_none());
    }

    #[test]
    fn fill_keeps_to_the_canvas() {
        let mut pixels = [0; 12];
        let mut canvas = Canvas::new(&mut pixels, 4, 3).unwrap();
        // off the left and the top, and past the right edge
        canvas.fill(-2, -1, 4, 3, 1);
        canvas.fill(3, 1, 9, 1, 2);
        // nothing: no size, and off the canvas
        canvas.fill(1, 1, 0, 5, 3);
        canvas.fill(1, 1, -3, 1, 3);
        canvas.fill(4, 0, 2, 2, 3);
        canvas.fill(0, 3, 2, 2, 3);
        assert_eq!(pixels, [1, 1, 0, 0, 1, 1, 0, 2, 0, 0, 0, 0]);
    }

    #[test]
    fn over_blends_premultiplied_pixels() {
        let (white, red) = (rgb(255, 255, 255), rgb(255, 0, 0));
        assert_eq!(red, 0xffff_0000);
        // an opaque pixel covers, and a clear one shows what is under
        assert_eq!(over(red, white), red);
        assert_eq!(over(0, white), white);
        // half a red, premultiplied, over white and over nothing
        assert_eq!(over(0x8080_0000, white), 0xffff_7f7f);
        assert_eq!(over(0x8080_0000, 0), 0x8080_0000);
        // 11 × 58 / 255 is 2.5, which rounds up
        assert_eq!(over(0xc500_0000, 0xff0b_0b0b), 0xff03_0303);
        // both pixels half clear
        assert_eq!(over(0x8080_0000, 0x8000_8000), 0xc080_4000);
    }
}
