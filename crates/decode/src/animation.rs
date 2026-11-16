//! Animations: their frames, fully composed, decoded on a thread of their own and kept within
//! a byte budget.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::Orientation;
use image::{AnimationDecoder, DynamicImage, Frames, ImageDecoder, ImageFormat, Limits};

use crate::{Image, to_srgb};

mod compose;

/// An animation's frames, decoded on a thread of their own as playback needs them. While
/// every frame fits the budget, all stay and loops cost nothing. Past it, a window of frames
/// around the one on screen stays, and a loop, or a step back past the window, decodes the
/// file again from the start. Dropping the animation stops its thread.
pub struct Animation {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    /// The frames kept, by index.
    frames: BTreeMap<usize, Arc<Image>>,
    /// The frame on screen.
    shown: usize,
    closed: bool,
}

impl Animation {
    /// Starts decoding the file's frames. `budget` caps the bytes of the frames kept, besides
    /// the one the decoder holds while it waits for room, and `max_bytes` each frame's decode,
    /// as [`crate::decode`] takes it. Once the file proves to hold two frames, `progress` gets
    /// the index of the first frame whose delay it has not had yet, those delays in seconds,
    /// and whether the last frame is among them: after each frame of the first pass, and
    /// whenever the frame on screen arrives after a wait, so the frontend can draw it. A file
    /// of one frame, or of frames too large for two to fit the budget or to compose within
    /// `max_bytes`, gets no call and stays a still.
    pub fn start(
        path: PathBuf,
        budget: u64,
        max_bytes: u64,
        progress: impl Fn(usize, &[f64], bool) + Send + 'static,
    ) -> Animation {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
        });
        let decoder = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("animation".to_owned())
            .spawn(move || play(&decoder, &path, budget, max_bytes, &progress))
            .expect("an animation thread");
        Animation { shared }
    }

    /// The frame at `index`, if it is decoded and kept.
    pub fn frame(&self, index: usize) -> Option<Arc<Image>> {
        self.shared
            .state
            .lock()
            .unwrap()
            .frames
            .get(&index)
            .cloned()
    }

    /// The frame now on screen, around which the window of kept frames stays. Call it for
    /// every frame the viewer moves to.
    pub fn show(&self, index: usize) {
        self.shared.state.lock().unwrap().shown = index;
        self.shared.wake.notify_all();
    }
}

impl Drop for Animation {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().closed = true;
        self.shared.wake.notify_all();
    }
}

enum Step {
    /// The frame is kept, and is the one on screen, which arrived after a wait.
    Kept {
        on_screen: bool,
    },
    Restart,
    Stop,
}

fn play(
    shared: &Shared,
    path: &Path,
    budget: u64,
    max_bytes: u64,
    progress: &dyn Fn(usize, &[f64], bool),
) {
    // the delay of every frame the first pass found; the count is final once that pass ends
    let mut delays = Vec::new();
    let mut complete = false;
    // the delays progress has had, as each call carries only the new ones
    let mut reported = 0;
    'pass: loop {
        // a viewer that moved on before the thread got here needs no decode at all
        if shared.state.lock().unwrap().closed {
            return;
        }
        let Some(mut opened) = open(path, max_bytes) else {
            return;
        };
        // two frames at least must fit, the one on screen and the next
        let window = usize::try_from(budget / opened.frame_bytes.max(1)).unwrap_or(usize::MAX);
        if window < 2 {
            return;
        }
        let mut index = 0;
        // a file that grew since the first pass shows no more frames than that pass found
        while !complete || index < delays.len() {
            // Decoding comes before the wait for room, so the end of a file whose frames all
            // fit shows up before any frame goes. A broken frame ends the animation, as
            // browsers end it.
            let Some((image, (numer, denom))) = next(&mut opened) else {
                break;
            };
            let on_screen = match step(shared, index, Some(Arc::new(image)), window) {
                Step::Kept { on_screen } => on_screen,
                Step::Restart => continue 'pass,
                Step::Stop => return,
            };
            // a first pass that started over, after a step back past the window, counts each
            // frame once
            let counted = index == delays.len();
            if counted {
                delays.push(f64::from(numer) / f64::from(denom.max(1)) / 1000.0);
            }
            if delays.len() >= 2 && (counted || on_screen) {
                progress(reported, &delays[reported..], complete);
                reported = delays.len();
            }
            index += 1;
        }
        if !complete {
            complete = true;
            if delays.len() < 2 {
                // a still after all, which the first frame shows already
                shared.state.lock().unwrap().frames.clear();
                return;
            }
            progress(reported, &delays[reported..], true);
            reported = delays.len();
        }
        // with every frame kept, loops need no more decoding
        if shared.state.lock().unwrap().frames.len() == delays.len() {
            return;
        }
        if let Step::Stop = step(shared, index, None, window) {
            return;
        }
    }
}

// Keeps decoded frame `index` once the window has room for it, or, at the end of the file
// with no frame, waits. Either way a new pass starts once the frame on screen lies behind
// the decoder and is gone.
fn step(shared: &Shared, index: usize, frame: Option<Arc<Image>>, window: usize) -> Step {
    let mut state = shared.state.lock().unwrap();
    loop {
        if state.closed {
            return Step::Stop;
        }
        let shown = state.shown;
        if shown < index && !state.frames.contains_key(&shown) {
            return Step::Restart;
        }
        if let Some(frame) = &frame
            && let Some(on_screen) = place(&mut state.frames, index, frame, shown, window)
        {
            return Step::Kept { on_screen };
        }
        state = shared.wake.wait(state).unwrap();
    }
}

// Where decoded frame `index` goes, with frame `shown` on screen: Some once it is kept, or
// passed over on the way to the window, true when it is the frame on screen; None while it
// must wait for room.
fn place(
    frames: &mut BTreeMap<usize, Arc<Image>>,
    index: usize,
    frame: &Arc<Image>,
    shown: usize,
    window: usize,
) -> Option<bool> {
    // a new pass decodes again the frames still kept from the last one
    if frames.contains_key(&index) {
        return Some(false);
    }
    // a quarter of the window stays behind the frame on screen, for C-p
    let behind = window / 4;
    let keep = shown.saturating_sub(behind)..=shown.saturating_add(window - behind - 1);
    if frames.len() >= window && index <= *keep.end() {
        // full: what the window does not hold goes, and a frame the window has passed is
        // decoded only on the way to it
        frames.retain(|i, _| keep.contains(i));
        if index < *keep.start() {
            return Some(false);
        }
    }
    // past the window ahead, the decoder waits for the frame on screen to move
    if frames.len() >= window {
        return None;
    }
    frames.insert(index, Arc::clone(frame));
    Some(index == shown)
}

struct Opened {
    frames: Frames<'static>,
    orientation: Orientation,
    icc: Option<Vec<u8>>,
    /// What one frame takes as RGBA, turned or not.
    frame_bytes: u64,
}

// The file's frames, fully composed, or None for a file that holds no animation or cannot be
// read. A GIF may hold a single frame, which the first pass finds out.
fn open(path: &Path, max_bytes: u64) -> Option<Opened> {
    // one handle for the metadata and the frames, so a file swapped in between cannot slip by
    let mut file = File::open(path).ok()?;
    let mut reader = BufReader::new(file.try_clone().ok()?);
    let format = image::guess_format(reader.fill_buf().ok()?).ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_bytes);
    // the bytes of each sample an APNG's frames decode to, 2 at 16 bits
    let (size, orientation, icc, gif, sample_bytes) = match format {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(reader).ok()?;
            decoder.set_limits(limits).ok()?;
            let (size, orientation, icc) = metadata(&mut decoder);
            (size, orientation, icc, Some(decoder), 1)
        }
        ImageFormat::Png => {
            let mut decoder = PngDecoder::with_limits(reader, limits).ok()?;
            if !decoder.is_apng().ok()? {
                return None;
            }
            let (size, orientation, icc) = metadata(&mut decoder);
            let color = decoder.color_type();
            let sample_bytes = color.bytes_per_pixel() / color.channel_count();
            (size, orientation, icc, None, u64::from(sample_bytes))
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(reader).ok()?;
            if !decoder.has_animation() {
                return None;
            }
            let (size, orientation, icc) = metadata(&mut decoder);
            (size, orientation, icc, None, 1)
        }
        _ => return None,
    };
    // saturating, as a PNG may declare 2^31 x 2^31, whose RGBA overflows a u64
    let frame_bytes = (u64::from(size.0) * u64::from(size.1)).saturating_mul(4);
    // What is alive at once at the peak of composing a frame and turning it, in canvases,
    // besides the frames kept, which the budget counts, and the color profile, of which the
    // decoder holds a copy beside ours. A quarter turn copies the frame once more.
    // - GIF: decoding stays within the image crate's limit, set to the cap; then the canvas
    //   kept for disposal, the composed copy, and the turned one.
    // - APNG: the canvas; what previous disposal saves, up to a canvas; the frames as the png
    //   crate decodes them, a canvas, two at 16 bits; the frame in 8 bits, or after it the
    //   composed copy; and the turned one.
    // - WebP: the canvas, and while a frame decodes, about three frames' worth: its RGBA, an
    //   alpha plane decoded as a whole lossless image, and the planes of a lossy bitstream;
    //   its compressed data beside them, which compose::webp counts frame by frame. Then the
    //   canvas, the composed copy and the turned one, fewer.
    let turned = u64::from(crate::turns(orientation));
    let canvases = match format {
        ImageFormat::Gif => 2 + turned,
        ImageFormat::Png => 3 + sample_bytes + turned,
        _ => 4,
    };
    let profile = icc.as_ref().map_or(0, |icc| icc.len() as u64);
    if frame_bytes
        .saturating_mul(canvases)
        .saturating_add(profile.saturating_mul(2))
        > max_bytes
    {
        return None;
    }
    // the image crate composes GIF frames, and those of APNG and WebP are composed here, read
    // from the start of the same file
    let frames = match gif {
        Some(decoder) => decoder.into_frames(),
        None => {
            file.rewind().ok()?;
            let reader = BufReader::new(file);
            match format {
                ImageFormat::Png => compose::apng(reader, max_bytes).ok()?,
                _ => compose::webp(reader, size, max_bytes).ok()?,
            }
        }
    };
    Some(Opened {
        frames,
        orientation,
        icc,
        frame_bytes,
    })
}

// metadata a decoder cannot read leaves the frames as stored, as in decode
fn metadata(decoder: &mut impl ImageDecoder) -> ((u32, u32), Orientation, Option<Vec<u8>>) {
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    (
        decoder.dimensions(),
        orientation,
        decoder.icc_profile().ok().flatten(),
    )
}

// The next frame, turned and in sRGB, with its delay as a ratio in milliseconds, or None at
// the end of the file or a frame that fails. A decoder that panics fails its frame, so the
// frames before it still play.
fn next(opened: &mut Opened) -> Option<(Image, (u32, u32))> {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let frame = opened.frames.next()?.ok()?;
        let delay = frame.delay().numer_denom_ms();
        Some((
            finish(frame, opened.orientation, opened.icc.as_deref()),
            delay,
        ))
    }))
    .ok()
    .flatten()
}

// a frame turned and in sRGB, as decode turned the file's first frame
fn finish(frame: image::Frame, orientation: Orientation, icc: Option<&[u8]>) -> Image {
    let mut image = DynamicImage::ImageRgba8(frame.into_buffer());
    image.apply_orientation(orientation);
    let mut rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    if let Some(icc) = icc {
        to_srgb(&mut rgba, width as usize, icc);
    }
    Image {
        width,
        height,
        rgba: rgba.into_raw(),
        may_animate: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba, RgbaImage};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const PLENTY: u64 = 1 << 30;

    type Calls = mpsc::Receiver<(usize, Vec<f64>, bool)>;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nsxiv-ng-animation-{}-{name}", std::process::id()))
    }

    // a GIF of solid 4x4 frames, each its red level and its delay in milliseconds
    fn gif(name: &str, frames: &[(u8, u32)]) -> PathBuf {
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            encoder
                .encode_frames(frames.iter().map(|&(red, ms)| {
                    let pixels = RgbaImage::from_pixel(4, 4, Rgba([red, 0, 0, 255]));
                    Frame::from_parts(pixels, 0, 0, Delay::from_numer_denom_ms(ms, 1))
                }))
                .unwrap();
        }
        let path = temp(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn start(path: &Path, budget: u64, max_bytes: u64) -> (Animation, Calls) {
        let (sender, calls) = mpsc::channel();
        let progress = move |first, delays: &[f64], done| {
            let _ = sender.send((first, delays.to_vec(), done));
        };
        let animation = Animation::start(path.to_owned(), budget, max_bytes, progress);
        (animation, calls)
    }

    // every progress call until the thread ends and drops its callback
    fn calls_until_the_end(calls: &Calls) -> Vec<(usize, Vec<f64>, bool)> {
        let mut all = Vec::new();
        loop {
            match calls.recv_timeout(Duration::from_secs(10)) {
                Ok(call) => all.push(call),
                Err(mpsc::RecvTimeoutError::Disconnected) => return all,
                Err(mpsc::RecvTimeoutError::Timeout) => panic!("the animation thread never ended"),
            }
        }
    }

    // the frame, once it is decoded
    fn decoded(animation: &Animation, index: usize) -> Arc<Image> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(frame) = animation.frame(index) {
                return frame;
            }
            assert!(Instant::now() < deadline, "frame {index} never decoded");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // the frame's red level, once the frame is decoded
    fn red(animation: &Animation, index: usize) -> u8 {
        decoded(animation, index).rgba[0]
    }

    #[test]
    fn frames_compose_as_their_disposal_and_blend_say() {
        const R: [u8; 4] = [255, 0, 0, 255];
        const G: [u8; 4] = [0, 255, 0, 255];
        const B: [u8; 4] = [0, 0, 255, 255];
        const W: [u8; 4] = [255; 4];
        const CLEAR: [u8; 4] = [0; 4];
        // white at half alpha over red
        const PINK: [u8; 4] = [255, 128, 128, 255];
        // Each fixture's frames, 4 x 1, as baseline/fixtures.sh places them. Every frame after the
        // first shows the disposal of the one before it.
        let cases = [
            // red; green over its left half, which background disposal clears to transparent,
            // as browsers and ImageMagick do, not to the file's background color, green here, as
            // ffmpeg and Pillow do; blue and a transparent pixel from x 1, which previous
            // disposal takes away again; white at x 3
            (
                "disposal.gif",
                [
                    [R, R, R, R],
                    [G, G, R, R],
                    [CLEAR, B, R, R],
                    [CLEAR, CLEAR, R, W],
                ],
            ),
            // the same in APNG, with green and a transparent pixel blended over, blue and a
            // transparent pixel replacing what lies under them, and half white over at x 3:
            // previous disposal puts back what its frame covered, cleared by the one before
            (
                "disposal.apng",
                [
                    [R, R, R, R],
                    [G, R, R, R],
                    [CLEAR, B, CLEAR, R],
                    [CLEAR, CLEAR, R, PINK],
                ],
            ),
            // WebP: green and a transparent pixel over; blue and a transparent pixel replacing,
            // at x 2, cleared by background disposal; then a whole frame over, half white at x 1.
            // The disposal clears its own frame's area only, and to transparent, as libwebp does,
            // for the file's background color is only a hint.
            (
                "disposal.webp",
                [
                    [R, R, R, R],
                    [G, R, R, R],
                    [G, R, B, CLEAR],
                    [G, PINK, CLEAR, CLEAR],
                ],
            ),
        ];
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures");
        let mut wrong = Vec::new();
        for (name, frames) in cases {
            let (animation, _calls) = start(&fixtures.join(name), PLENTY, PLENTY);
            for (index, expected) in frames.iter().enumerate() {
                let frame = decoded(&animation, index);
                let found = frame.rgba.as_chunks::<4>().0;
                // blends may round either way
                let close = (found.iter().flatten())
                    .zip(expected.iter().flatten())
                    .all(|(a, b)| a.abs_diff(*b) <= 1);
                if !close || found.len() != 4 {
                    wrong.push(format!("{name} frame {index}: {found:?}, not {expected:?}"));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn an_apng_too_large_to_count_stays_a_still() {
        // 2^31 x 2^31 in 8-bit gray, whose RGBA canvas is 2^64 bytes, one past what a u64
        // holds, under a cap that holds a row of it, so the png crate's own checks pass
        let chunk = |kind: &[u8], data: &[u8]| {
            let mut crc = flate2::Crc::new();
            crc.update(kind);
            crc.update(data);
            let size = (data.len() as u32).to_be_bytes();
            [&size[..], kind, data, &crc.sum().to_be_bytes()].concat()
        };
        let side = (1u32 << 31).to_be_bytes();
        let header = [&side[..], &side, &[8, 0, 0, 0, 0]].concat();
        let png = [
            &b"\x89PNG\r\n\x1a\n"[..],
            &chunk(b"IHDR", &header),
            &chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]),
            &chunk(b"IDAT", &[0x78, 0x9c]),
            &chunk(b"IEND", b""),
        ]
        .concat();
        let path = temp("huge.apng");
        std::fs::write(&path, png).unwrap();
        assert!(open(&path, 1 << 40).is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_decoder_that_panics_ends_the_animation_at_its_frame() {
        let frames = (0..3).map(|i| {
            assert!(i == 0, "a decoder bug");
            Ok(Frame::new(RgbaImage::new(1, 1)))
        });
        let mut opened = Opened {
            frames: Frames::new(Box::new(frames)),
            orientation: Orientation::NoTransforms,
            icc: None,
            frame_bytes: 4,
        };
        assert!(next(&mut opened).is_some());
        assert!(next(&mut opened).is_none(), "the panic fails the frame");
    }

    #[test]
    fn an_animation_that_fits_keeps_every_frame() {
        let path = gif("fits.gif", &[(250, 10), (130, 20), (10, 30)]);
        // room for exactly the three 4x4 frames
        let (animation, calls) = start(&path, 3 * 64, PLENTY);
        assert_eq!(
            calls_until_the_end(&calls),
            [
                (0, vec![0.01, 0.02], false),
                (2, vec![0.03], false),
                (3, vec![], true)
            ],
            "each call brings the new delays, and the thread ends once every frame is kept"
        );
        let reds: Vec<u8> = (0..3).map(|i| red(&animation, i)).collect();
        assert!(
            reds[0] > 200 && (100..160).contains(&reds[1]) && reds[2] < 40,
            "{reds:?}"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn past_the_budget_a_window_follows_the_frame_on_screen() {
        let levels: Vec<(u8, u32)> = (0..8).map(|i| (i * 30, 100)).collect();
        let path = gif("window.gif", &levels);
        // room for three 4x4 frames
        let (animation, calls) = start(&path, 3 * 64, PLENTY);
        let show = |i: usize| {
            animation.show(i);
            let level = red(&animation, i);
            assert!(level.abs_diff(i as u8 * 30) <= 8, "frame {i}: {level}");
            let kept = animation.shared.state.lock().unwrap().frames.len();
            assert!(kept <= 3, "{kept} frames kept");
        };
        (0..5).for_each(show);
        // a step back past the window, before the first pass is through, starts it over
        (0..8).for_each(show);
        // the viewer loops only once the count is known
        let counted = loop {
            let (first, delays, done) = calls.recv_timeout(Duration::from_secs(10)).unwrap();
            if done {
                break first + delays.len();
            }
        };
        assert_eq!(counted, 8, "frames decoded twice count once");
        // a loop decodes the file again from the start, and the frame on screen comes back
        // with a call, as nothing else would redraw it in a paused animation
        animation.show(0);
        let call = calls.recv_timeout(Duration::from_secs(10)).unwrap();
        let red = animation.frame(0).map(|frame| frame.rgba[0]);
        assert!(
            call == (8, vec![], true) && red.is_some_and(|red| red <= 8),
            "{call:?} {red:?}"
        );
        drop(animation);
        assert!(
            calls_until_the_end(&calls).is_empty(),
            "dropping the animation ends the thread"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_window_waits_for_the_frame_on_screen() {
        let frame = Arc::new(Image {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            may_animate: false,
        });
        // frames 0 to 3 kept while everything fit, around frame 0 as the window has it: 0 to 2
        let mut frames: BTreeMap<_, _> = (0..4).map(|i| (i, Arc::clone(&frame))).collect();
        let kept =
            |frames: &BTreeMap<usize, Arc<Image>>| frames.keys().copied().collect::<Vec<_>>();
        assert_eq!(
            place(&mut frames, 4, &frame, 0, 4),
            None,
            "a paused animation decodes no further than its window"
        );
        assert_eq!(kept(&frames), [0, 1, 2, 3], "and drops nothing for it");
        assert_eq!(place(&mut frames, 4, &frame, 2, 4), Some(false));
        assert_eq!(
            kept(&frames),
            [1, 2, 3, 4],
            "the frame on screen moving makes room"
        );
        assert_eq!(
            place(&mut frames, 5, &frame, 5, 4),
            Some(true),
            "the frame on screen arriving says so"
        );
        // a full window after a jump far ahead
        let mut full: BTreeMap<_, _> = (4..7).map(|i| (i, Arc::clone(&frame))).collect();
        assert_eq!(
            place(&mut full, 0, &frame, 9, 3),
            Some(false),
            "a frame the window has passed is decoded on the way only"
        );
        assert!(full.is_empty(), "and the frames the window left go");
    }

    #[test]
    fn a_still_is_no_animation() {
        let still = gif("still.gif", &[(200, 100)]);
        let png = temp("still.png");
        RgbaImage::new(4, 4).save(&png).unwrap();
        let animated = gif("large.gif", &[(200, 100), (100, 100)]);
        // a still GIF, a PNG, frames too large for two to fit the budget, and frames too large
        // for the canvas a GIF keeps and the composed copy to fit the cap; and the disposal
        // fixtures, 4 x 1, a byte short of the four canvases composing an APNG or a WebP holds
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures");
        let (apng, webp) = (
            fixtures.join("disposal.apng"),
            fixtures.join("disposal.webp"),
        );
        let cases = [
            (&still, PLENTY, PLENTY),
            (&png, PLENTY, PLENTY),
            (&animated, 64, PLENTY),
            (&animated, PLENTY, 2 * 64 - 1),
            (&apng, PLENTY, 4 * 16 - 1),
            (&webp, PLENTY, 4 * 16 - 1),
        ];
        for (path, budget, max_bytes) in cases {
            let (animation, calls) = start(path, budget, max_bytes);
            assert!(
                calls_until_the_end(&calls).is_empty(),
                "{} animates",
                path.display()
            );
            assert!(animation.frame(0).is_none(), "and keeps no frame");
        }
        for path in [still, png, animated] {
            std::fs::remove_file(path).unwrap();
        }
    }
}
