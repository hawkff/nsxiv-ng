//! What only a system can recognize in an image, behind one trait a frontend implements:
//! text with its words' boxes, barcodes, faces and the foreground. Its calls block; the
//! edit asks for one through [`crate::Response::Recognize`] and takes the answer through
//! [`crate::Edit::recognized`], whoever ran it on whichever thread.

use nsxiv_ng_core::Rect;

use crate::{RgbaImage, Word};

/// What to recognize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recognize {
    /// Text, to copy (`o`) or to find personal data in (`P`).
    Text,
    /// Barcodes' payloads, to copy (`Q`).
    Barcodes,
    /// Faces, to censor (`F`).
    Faces,
    /// The foreground's coverage, to remove the background (`B`).
    Foreground,
}

/// A recognizer's answer. Boxes are in the image's pixels, origin top left.
#[derive(Clone, Debug, PartialEq)]
pub enum Recognition {
    /// The lines joined by `\n`, and each word with its box.
    Text(String, Vec<Word>),
    Barcodes(Vec<String>),
    /// Each face's box, grown by a tenth of its size on each side, so a censor covers the
    /// whole face.
    Faces(Vec<Rect>),
    /// One byte of coverage a pixel, rows top to bottom; None where nothing stands out or the
    /// system cannot tell.
    Foreground(Option<Vec<u8>>),
    /// The system recognizes nothing of the kind.
    Unavailable,
}

/// A system's recognition. Each call blocks until it has an answer.
pub trait Recognizer: Send + Sync {
    /// The text's lines joined by `\n`, and each word with its box and line; None where it
    /// failed. The words come line by line, each line's in reading order.
    fn text(&self, image: &RgbaImage) -> Option<(String, Vec<Word>)>;
    fn barcodes(&self, image: &RgbaImage) -> Vec<String>;
    /// Face boxes, grown by a tenth of their size on each side.
    fn faces(&self, image: &RgbaImage) -> Vec<Rect>;
    /// The foreground's coverage, one byte a pixel; None where the system cannot tell.
    fn foreground(&self, image: &RgbaImage) -> Option<Vec<u8>>;
}

/// Asks `recognizer` for `what` in `image`, blocking; Unavailable without one.
pub fn recognize(
    recognizer: Option<&dyn Recognizer>,
    what: Recognize,
    image: &RgbaImage,
) -> Recognition {
    let Some(recognizer) = recognizer else {
        return Recognition::Unavailable;
    };
    match what {
        Recognize::Text => match recognizer.text(image) {
            Some((text, words)) => Recognition::Text(text, words),
            None => Recognition::Text(String::new(), Vec::new()),
        },
        Recognize::Barcodes => Recognition::Barcodes(recognizer.barcodes(image)),
        Recognize::Faces => Recognition::Faces(recognizer.faces(image)),
        Recognize::Foreground => Recognition::Foreground(recognizer.foreground(image)),
    }
}
