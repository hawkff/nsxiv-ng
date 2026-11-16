//! What Vision recognizes in an edit's image: text with its words' boxes, barcodes' payloads,
//! faces, and the foreground's coverage.

use std::ffi::c_void;
use std::ptr::NonNull;

use nsxiv_ng_core::Rect;
use nsxiv_ng_edit::{RgbaImage, Word};
use nsxiv_ng_session::Recognizer;
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyClass;
use objc2_core_foundation::{CFRetained, CGRect};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    kCGColorSpaceSRGB,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_OneComponent8, kCVPixelFormatType_OneComponent32Float, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSDictionary, NSRange};
use objc2_vision::{
    VNDetectBarcodesRequest, VNDetectFaceRectanglesRequest,
    VNGenerateForegroundInstanceMaskRequest, VNImageRequestHandler, VNRecognizeTextRequest,
    VNRequest, VNRequestTextRecognitionLevel,
};

/// Vision, whose requests run on the thread that asks, each in an autorelease pool of its own,
/// so a thread that runs many holds nothing between them.
pub(crate) struct Vision;

impl Recognizer for Vision {
    fn text(&self, image: &RgbaImage) -> Option<(String, Vec<Word>)> {
        autoreleasepool(|_| {
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setAutomaticallyDetectsLanguage(true);
            if perform(image, &request).is_none() {
                // the accurate level fails where the system cannot run its model, where the
                // fast one still reads
                request.setRecognitionLevel(VNRequestTextRecognitionLevel::Fast);
                perform(image, &request)?;
            }
            let size = (f64::from(image.width), f64::from(image.height));
            let mut lines = Vec::new();
            let mut words = Vec::new();
            for observation in request.results()?.to_vec() {
                let Some(candidate) = observation.topCandidates(1).firstObject() else {
                    continue;
                };
                let line = candidate.string().to_string();
                for (text, range) in word_ranges(&line) {
                    // SAFETY: the range is of the candidate's own string, in its UTF-16 units
                    let Ok(found) = (unsafe { candidate.boundingBoxForRange_error(range) }) else {
                        continue;
                    };
                    // SAFETY: an observation's box, normalized, which every one has
                    let rect = to_pixels(unsafe { found.boundingBox() }, size);
                    let text = text.to_owned();
                    let line = lines.len();
                    words.push(Word { text, rect, line });
                }
                lines.push(line);
            }
            Some((lines.join("\n"), words))
        })
    }

    fn barcodes(&self, image: &RgbaImage) -> Vec<String> {
        autoreleasepool(|_| {
            // SAFETY: takes nothing, and gives a request of its default settings
            let request = unsafe { VNDetectBarcodesRequest::new() };
            perform(image, &request);
            // SAFETY: the request's barcode observations, once performed; None where it failed
            let Some(found) = (unsafe { request.results() }) else {
                return Vec::new();
            };
            // SAFETY: an observation's payload, None where it is not a string
            let payloads = found
                .to_vec()
                .into_iter()
                .filter_map(|o| unsafe { o.payloadStringValue() });
            payloads.map(|payload| payload.to_string()).collect()
        })
    }

    fn faces(&self, image: &RgbaImage) -> Vec<Rect> {
        detect_faces(image).unwrap_or_default()
    }

    fn foreground(&self, image: &RgbaImage) -> Option<Vec<u8>> {
        // macOS 14's request, which the system before it lacks
        AnyClass::get(c"VNGenerateForegroundInstanceMaskRequest")?;
        autoreleasepool(|_| {
            // SAFETY: takes nothing, and gives a request of its default settings; the class
            // is there, as asked above
            let request = unsafe { VNGenerateForegroundInstanceMaskRequest::new() };
            let handler = perform(image, &request)?;
            // SAFETY: the request's mask observations, once performed; None where it failed
            let observation = unsafe { request.results() }?.firstObject()?;
            // SAFETY: the instances the observation found, the background not among them
            let instances = unsafe { observation.allInstances() };
            if instances.count() == 0 {
                return None;
            }
            // SAFETY: the instances are the observation's own, and the handler is the one
            // that performed its request
            let mask = unsafe {
                observation.generateScaledMaskForImageForInstances_fromRequestHandler_error(
                    &instances, &handler,
                )
            };
            let mask = mask.ok()?;
            read_mask(&mask, image.width, image.height)
        })
    }
}

/// The faces' boxes, grown; None where the request was not performed or gave no results.
fn detect_faces(image: &RgbaImage) -> Option<Vec<Rect>> {
    autoreleasepool(|_| {
        // SAFETY: takes nothing, and gives a request of its default settings
        let request = unsafe { VNDetectFaceRectanglesRequest::new() };
        perform(image, &request)?;
        // SAFETY: the request's face observations, once performed; None where it failed
        let found = unsafe { request.results() }?;
        let size = (f64::from(image.width), f64::from(image.height));
        // SAFETY: an observation's box, normalized, which every one has
        let boxes = found
            .to_vec()
            .into_iter()
            .map(|o| unsafe { o.boundingBox() });
        Some(boxes.map(|b| grown(to_pixels(b, size))).collect())
    })
}

/// Performs `request` on `image`, and gives the handler, which a mask is scaled through; None
/// where Vision could not perform it, which leaves the request without results.
fn perform(image: &RgbaImage, request: &VNRequest) -> Option<Retained<VNImageRequestHandler>> {
    let image = cg_image(image)?;
    // SAFETY: a handler of an image, with no options
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            &image,
            &NSDictionary::new(),
        )
    };
    let requests = NSArray::from_slice(&[request]);
    handler.performRequests_error(&requests).ok()?;
    Some(handler)
}

/// An image of `image`'s straight RGBA in sRGB, over a copy of its pixels that the image owns,
/// as Vision may keep an image past the request.
fn cg_image(image: &RgbaImage) -> Option<CFRetained<CGImage>> {
    let (width, height) = (image.width as usize, image.height as usize);
    // a size whose bytes overflow is no image's
    let length = (width.checked_mul(height)).and_then(|pixels| pixels.checked_mul(4));
    if width == 0 || height == 0 || length != Some(image.rgba.len()) {
        return None;
    }
    let pixels = Box::new(image.rgba.clone());
    let (data, bytes) = (pixels.as_ptr().cast::<c_void>(), pixels.len());
    let info = Box::into_raw(pixels).cast::<c_void>();
    // SAFETY: `data` is `bytes` long and lives in the box behind `info` until `release`
    // frees it, which the provider calls once when it goes
    let provider = unsafe { CGDataProvider::with_data(info, data, bytes, Some(release)) };
    let Some(provider) = provider else {
        // SAFETY: no provider took the box, so it is still this function's to free
        unsafe { drop(Box::from_raw(info.cast::<Vec<u8>>())) };
        return None;
    };
    // SAFETY: a constant string Core Graphics defines
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: the provider holds width x height pixels of four bytes, R, G, B and A, rows
    // without padding, and the decode array may be null
    unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            width * 4,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::Last.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

// Frees the pixels an image owned, once its provider goes.
unsafe extern "C-unwind" fn release(info: *mut c_void, _: NonNull<c_void>, _: usize) {
    // SAFETY: `info` is the box `cg_image` gave the provider, which calls this once
    unsafe { drop(Box::from_raw(info.cast::<Vec<u8>>())) };
}

/// The coverage of a mask the size of the image, one byte a pixel; None where it is of
/// another size or format, or cannot be read.
fn read_mask(mask: &CVPixelBuffer, width: u32, height: u32) -> Option<Vec<u8>> {
    let (width, height) = (width as usize, height as usize);
    if CVPixelBufferGetWidth(mask) != width || CVPixelBufferGetHeight(mask) != height {
        return None;
    }
    let flags = CVPixelBufferLockFlags::ReadOnly;
    // SAFETY: locks the buffer's memory for reading, which the unlock below ends
    if unsafe { CVPixelBufferLockBaseAddress(mask, flags) } != kCVReturnSuccess {
        return None;
    }
    let base = CVPixelBufferGetBaseAddress(mask);
    let stride = CVPixelBufferGetBytesPerRow(mask);
    let format = CVPixelBufferGetPixelFormatType(mask);
    let read = (!base.is_null()).then(|| {
        // SAFETY: a locked buffer's base address holds its rows, bytes_per_row apart, until
        // it is unlocked, which comes after the last read of them
        let bytes = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), stride * height) };
        coverage(format, width, height, stride, bytes)
    });
    // SAFETY: unlocks what was locked above, with the same flags
    unsafe { CVPixelBufferUnlockBaseAddress(mask, flags) };
    read.flatten()
}

/// One byte a pixel of a mask `width` x `height` of one component in `format`, rows `stride`
/// bytes apart in `bytes`: 8 bits as they are, a 32-bit float from 0 to 1 scaled to 255.
/// None for another format, or rows the bytes cannot hold.
fn coverage(
    format: u32,
    width: usize,
    height: usize,
    stride: usize,
    bytes: &[u8],
) -> Option<Vec<u8>> {
    let size = if format == kCVPixelFormatType_OneComponent8 {
        1
    } else if format == kCVPixelFormatType_OneComponent32Float {
        4
    } else {
        return None;
    };
    let row = width * size;
    if height == 0 || stride < row || bytes.len() < stride * (height - 1) + row {
        return None;
    }
    let mut out = Vec::with_capacity(width * height);
    for line in bytes.chunks(stride).take(height) {
        let line = &line[..row];
        match size {
            1 => out.extend_from_slice(line),
            _ => out.extend(line.as_chunks::<4>().0.iter().map(|&v| {
                let v = f32::from_ne_bytes(v);
                (v.clamp(0.0, 1.0) * 255.0).round() as u8
            })),
        }
    }
    Some(out)
}

/// A box Vision gives, normalized from the bottom left, in the pixels of an image of `size`,
/// from the top left.
fn to_pixels(normalized: CGRect, (width, height): (f64, f64)) -> Rect {
    Rect {
        x: normalized.origin.x * width,
        y: (1.0 - normalized.origin.y - normalized.size.height) * height,
        width: normalized.size.width * width,
        height: normalized.size.height * height,
    }
}

/// `rect` grown by a tenth of its size on each side, so a censor covers the whole face.
fn grown(rect: Rect) -> Rect {
    let (dx, dy) = (rect.width / 10.0, rect.height / 10.0);
    Rect {
        x: rect.x - dx,
        y: rect.y - dy,
        width: rect.width + 2.0 * dx,
        height: rect.height + 2.0 * dy,
    }
}

/// The words of `line`, as a regular expression's `\S+` finds them: runs of anything but white
/// space, each with its range in UTF-16 units, as Vision's string counts them.
fn word_ranges(line: &str) -> Vec<(&str, NSRange)> {
    let mut words = Vec::new();
    // where the word now running starts, in bytes and in UTF-16 units
    let mut start = None;
    let mut utf16 = 0;
    for (byte, c) in line.char_indices() {
        if c.is_whitespace() {
            if let Some((from, at)) = start.take() {
                words.push((&line[from..byte], NSRange::new(at, utf16 - at)));
            }
        } else if start.is_none() {
            start = Some((byte, utf16));
        }
        utf16 += c.len_utf16();
    }
    if let Some((from, at)) = start {
        words.push((&line[from..], NSRange::new(at, utf16 - at)));
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    use nsxiv_ng_core::Size;
    use nsxiv_ng_core::config::Config;
    use nsxiv_ng_core::file_list::FileEntry;
    use nsxiv_ng_core::options::Options;
    use nsxiv_ng_core::viewer::Keystroke;
    use nsxiv_ng_render::Canvas;
    use nsxiv_ng_session::{Bar, EditSetup, Request, Session};
    use objc2_core_foundation::{CGPoint, CGSize};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fixtures")
            .join(name)
    }

    fn image(name: &str) -> RgbaImage {
        let image = nsxiv_ng_decode::decode(&fixture(name), 1 << 30).unwrap();
        RgbaImage {
            width: image.width,
            height: image.height,
            rgba: image.rgba,
        }
    }

    #[test]
    fn a_lines_words_are_its_runs_without_white_space_counted_in_utf16() {
        let range = |words: &[(&str, NSRange)]| -> Vec<_> {
            (words.iter())
                .map(|(word, r)| (word.to_string(), r.location, r.length))
                .collect()
        };
        // the euro sign is one unit, the emoji two
        let words = word_ranges(" a\u{a0}b€c  😀x\tend ");
        assert_eq!(
            range(&words),
            [
                ("a".to_owned(), 1, 1),
                ("b€c".to_owned(), 3, 3),
                ("😀x".to_owned(), 8, 3),
                ("end".to_owned(), 12, 3),
            ]
        );
        assert!(word_ranges(" \t").is_empty());
    }

    #[test]
    fn a_box_from_the_bottom_left_comes_out_in_pixels_from_the_top_left() {
        let normalized = CGRect::new(CGPoint::new(0.25, 0.5), CGSize::new(0.5, 0.25));
        let rect = to_pixels(normalized, (400.0, 200.0));
        assert_eq!(
            rect,
            Rect {
                x: 100.0,
                y: 50.0,
                width: 200.0,
                height: 50.0
            }
        );
        // a face grows a tenth of its size on each side
        assert_eq!(
            grown(rect),
            Rect {
                x: 80.0,
                y: 45.0,
                width: 240.0,
                height: 60.0
            }
        );
    }

    #[test]
    fn a_mask_is_read_one_byte_a_pixel_past_the_padding_of_its_rows() {
        // two rows of two pixels, each row padded to 3 bytes, the last row unpadded
        let bytes = [10, 20, 99, 30, 40];
        assert_eq!(
            coverage(kCVPixelFormatType_OneComponent8, 2, 2, 3, &bytes),
            Some(vec![10, 20, 30, 40])
        );
        // floats from 0 to 1, and past them, clamped, a row padded to three
        let floats: Vec<u8> = [0.0f32, 0.5, 9.0, 2.0, -1.0]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        assert_eq!(
            coverage(kCVPixelFormatType_OneComponent32Float, 2, 2, 12, &floats),
            Some(vec![0, 128, 255, 0])
        );
        // too few bytes for the rows, or another format
        assert_eq!(
            coverage(kCVPixelFormatType_OneComponent8, 2, 2, 3, &bytes[..4]),
            None
        );
        assert_eq!(coverage(0x4c303068, 2, 2, 4, &[0; 8]), None);
    }

    #[test]
    fn text_comes_with_each_words_box() {
        let (text, words) = Vision.text(&image("text.png")).unwrap();
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            ["Write to jane.doe@example.com", "before the end of May"],
            "{text:?}"
        );
        // the address, printed from x 24 on the baseline at 76, 36 pixels high
        let email = words.iter().find(|w| w.text == "jane.doe@example.com");
        let rect = email.unwrap().rect;
        assert!(rect.x > 120.0 && rect.max_x() < 650.0, "{rect:?}");
        assert!(rect.y > 30.0 && rect.max_y() < 95.0, "{rect:?}");
        assert_eq!(words.len(), 8, "{words:?}");
        // each word knows its line
        let lines: Vec<usize> = words.iter().map(|w| w.line).collect();
        assert_eq!(lines, [0, 0, 0, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn a_qr_codes_payload_is_read() {
        assert_eq!(
            Vision.barcodes(&image("qr.png")),
            ["https://github.com/nsxiv/nsxiv"]
        );
    }

    #[test]
    fn an_image_whose_size_overflows_its_bytes_makes_no_image() {
        let image = RgbaImage {
            width: u32::MAX,
            height: u32::MAX,
            rgba: Vec::new(),
        };
        assert!(cg_image(&image).is_none());
    }

    #[test]
    fn faces_are_none_where_the_image_has_none() {
        // no image of a face with a license that allows it is to hand: the request runs, and
        // its results are there, empty
        assert_eq!(detect_faces(&image("text.png")), Some(Vec::new()));
    }

    #[test]
    fn the_foreground_is_the_subject_and_a_plain_image_has_none() {
        let subject = image("subject.png");
        let Some(mask) = Vision.foreground(&subject) else {
            // macOS 13, without the request
            assert!(AnyClass::get(c"VNGenerateForegroundInstanceMaskRequest").is_none());
            return;
        };
        assert_eq!(mask.len(), 400 * 300);
        let at = |x: usize, y: usize| mask[y * 400 + x];
        // the disk's center, at 200, 150, and the ground's corners
        assert!(at(200, 150) > 200, "{}", at(200, 150));
        assert!(
            at(5, 5) < 50 && at(395, 295) < 50,
            "{} {}",
            at(5, 5),
            at(395, 295)
        );
        let plain = RgbaImage {
            width: 64,
            height: 48,
            rgba: vec![216; 64 * 48 * 4],
        };
        assert_eq!(Vision.foreground(&plain), None);
    }

    /// A session over `file`, in a window of the image's size with the edit's status line
    /// below it, so the image's pixels are the canvas's, which edits with Vision.
    fn open(file: &str, width: u32, height: u32) -> (Session, Receiver<()>) {
        let (sender, wakes) = mpsc::channel();
        let options = Options {
            no_bar: true,
            ..Options::default()
        };
        let mut session = Session::new(
            options,
            vec![FileEntry::new(fixture(file))],
            &Config::default(),
            PathBuf::from("nsxiv-ng"),
            Vec::new(),
            Bar::Drawn,
            move || {
                let _ = sender.send(());
            },
        );
        session.can_edit(EditSetup {
            families: crate::fonts::FAMILIES.map(str::to_owned).to_vec(),
            field: true,
            recognizer: Some(Arc::new(Vision)),
        });
        let window = Size {
            width: f64::from(width),
            height: f64::from(height),
        };
        session.resize(window, 1.0);
        session.start();
        while session.viewer().image().is_none() || session.title_height() == 0.0 {
            wakes.recv_timeout(Duration::from_secs(60)).expect("a wake");
            session.answers();
        }
        // the window grown by the edit's status line, which shows below the canvas in an edit,
        // so the canvas stays the image's size
        let window = Size {
            height: f64::from(height) + session.title_height(),
            ..window
        };
        session.resize(window, 1.0);
        session.key(&Keystroke::new("x"));
        assert!(session.editing());
        (session, wakes)
    }

    /// The requests of what `name` asks, until one holds `until`, within a minute.
    fn asked(
        session: &mut Session,
        wakes: &Receiver<()>,
        name: &str,
        until: impl Fn(&Request) -> bool,
    ) -> Vec<Request> {
        let mut requests = session.key(&Keystroke::new(name)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !requests.iter().any(&until) && Instant::now() < deadline {
            if wakes.recv_timeout(Duration::from_secs(1)).is_ok() {
                requests.extend(session.answers());
            }
        }
        requests
    }

    /// The canvas's pixels, `width` x `height`, of the frame with the edit's status line below.
    fn frame(session: &mut Session, width: u32, height: u32) -> Vec<u32> {
        let rows = session.bar().round() as u32;
        let mut pixels = vec![0; (width * (height + rows)) as usize];
        let mut canvas = Canvas::new(&mut pixels, width, height + rows).unwrap();
        session.draw(&mut canvas, 1.0, Default::default());
        pixels.truncate((width * height) as usize);
        pixels
    }

    fn redraw_or_beep(r: &Request) -> bool {
        matches!(r, Request::Redraw { bar_only: false } | Request::Beep)
    }

    #[test]
    fn an_edit_copies_text_and_a_payload_censors_an_address_and_cuts_the_background() {
        // o copies the text, and P censors the address, in its line alone
        let (mut session, wakes) = open("text.png", 720, 200);
        let copied = asked(&mut session, &wakes, "o", |r| {
            matches!(r, Request::CopyText(_) | Request::Beep)
        });
        assert!(
            copied
                .iter()
                .any(|r| matches!(r, Request::CopyText(t) if t.contains("jane.doe@example.com\n"))),
            "{copied:?}"
        );
        let before = frame(&mut session, 720, 200);
        let censored = asked(&mut session, &wakes, "P", redraw_or_beep);
        assert!(!censored.contains(&Request::Beep), "{censored:?}");
        let after = frame(&mut session, 720, 200);
        let changed = |rows: std::ops::Range<usize>| {
            rows.flat_map(|y| (0..720).map(move |x| y * 720 + x))
                .any(|i| before[i] != after[i])
        };
        assert!(changed(40..90), "the address");
        assert!(!changed(110..200), "the second line");
        // Q copies a QR code's payload
        let (mut session, wakes) = open("qr.png", 232, 232);
        let payload = Request::CopyText("https://github.com/nsxiv/nsxiv".to_owned());
        let copied = asked(&mut session, &wakes, "Q", |r| {
            *r == payload || *r == Request::Beep
        });
        assert!(copied.contains(&payload), "{copied:?}");
        // B cuts the ground from around the disk, where the system has the request
        if AnyClass::get(c"VNGenerateForegroundInstanceMaskRequest").is_none() {
            return;
        }
        let (mut session, wakes) = open("subject.png", 400, 300);
        let before = frame(&mut session, 400, 300);
        let cut = asked(&mut session, &wakes, "B", redraw_or_beep);
        assert!(!cut.contains(&Request::Beep), "{cut:?}");
        let after = frame(&mut session, 400, 300);
        let at = |x: usize, y: usize| y * 400 + x;
        assert_ne!(before[at(5, 5)], after[at(5, 5)], "the ground");
        assert_eq!(before[at(200, 150)], after[at(200, 150)], "the disk");
    }

    #[test]
    fn p_censors_a_phone_number_in_groups_and_not_the_words_around_it() {
        let (_, words) = Vision.text(&image("phone.png")).unwrap();
        let rect = |text: &str| {
            let word = words.iter().find(|w| w.text == text);
            word.unwrap_or_else(|| panic!("{text} in {words:?}")).rect
        };
        let (call, plus, last, today) = (rect("Call"), rect("+1"), rect("4567"), rect("today"));
        let (mut session, wakes) = open("phone.png", 720, 100);
        let before = frame(&mut session, 720, 100);
        let censored = asked(&mut session, &wakes, "P", redraw_or_beep);
        assert!(!censored.contains(&Request::Beep), "{censored:?}");
        let after = frame(&mut session, 720, 100);
        // within a word's box, 4 pixels in from its edges
        let changed = |r: Rect| {
            let (left, right) = ((r.x + 4.0) as usize, (r.max_x() - 4.0).min(720.0) as usize);
            let (top, bottom) = ((r.y + 4.0) as usize, (r.max_y() - 4.0).min(100.0) as usize);
            (top..bottom).any(|y| (left..right).any(|x| before[y * 720 + x] != after[y * 720 + x]))
        };
        assert!(changed(plus) && changed(last), "the number");
        assert!(!changed(call) && !changed(today), "the words around it");
    }

    /// `from` scaled `times` by nearest neighbour into `to` at `x`, `y`.
    fn paste(to: &mut RgbaImage, from: &RgbaImage, times: u32, (x, y): (u32, u32)) {
        for row in 0..from.height * times {
            for column in 0..from.width * times {
                let source = ((row / times * from.width + column / times) * 4) as usize;
                let target = (((y + row) * to.width + x + column) * 4) as usize;
                to.rgba[target..target + 4].copy_from_slice(&from.rgba[source..source + 4]);
            }
        }
    }

    /// The process's resident memory in MB, as ps reads it.
    fn resident() -> u64 {
        let pid = std::process::id().to_string();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .unwrap()
            / 1024
    }

    // Each request on a 12 MP image, 4000 x 3000: the disk on its ground, the text and the QR
    // code pasted over it, scaled up; the time and the resident memory before and after.
    #[test]
    #[ignore = "a measurement, run alone in release"]
    fn measure_each_request_on_12_mp() {
        let mut big = RgbaImage {
            width: 4000,
            height: 3000,
            rgba: vec![0; 4000 * 3000 * 4],
        };
        paste(&mut big, &image("subject.png"), 10, (0, 0));
        paste(&mut big, &image("text.png"), 2, (100, 100));
        paste(&mut big, &image("qr.png"), 3, (3200, 100));
        let requests: [(&str, &dyn Fn() -> String); 4] = [
            ("text", &|| {
                format!("{:?}", Vision.text(&big).map(|(t, w)| (t.len(), w.len())))
            }),
            ("barcodes", &|| format!("{:?}", Vision.barcodes(&big))),
            ("faces", &|| format!("{:?}", Vision.faces(&big).len())),
            ("foreground", &|| {
                format!("{:?}", Vision.foreground(&big).map(|m| m.len()))
            }),
        ];
        for round in 0..4 {
            for (name, run) in &requests {
                let before = resident();
                let start = Instant::now();
                let answer = run();
                let took = start.elapsed();
                let after = resident();
                println!(
                    "round {round} {name}: {} ms, resident {before} -> {after} MB, {answer}",
                    took.as_millis()
                );
            }
        }
        // the mask's pixel format, as Vision gives it
        autoreleasepool(|_| {
            // SAFETY: as in `foreground`
            let request = unsafe { VNGenerateForegroundInstanceMaskRequest::new() };
            let handler = perform(&big, &request).unwrap();
            // SAFETY: as in `foreground`
            let observation = unsafe { request.results() }.unwrap().firstObject().unwrap();
            // SAFETY: as in `foreground`
            let instances = unsafe { observation.allInstances() };
            // SAFETY: as in `foreground`
            let mask = unsafe {
                observation.generateScaledMaskForImageForInstances_fromRequestHandler_error(
                    &instances, &handler,
                )
            }
            .unwrap();
            let format = CVPixelBufferGetPixelFormatType(&mask).to_be_bytes();
            println!(
                "mask format {:?}, {} bytes a row for {} pixels",
                String::from_utf8_lossy(&format),
                CVPixelBufferGetBytesPerRow(&mask),
                CVPixelBufferGetWidth(&mask)
            );
        });
    }
}
