//! Frames of the current file's neighbors, made on a thread of the terminal's own while the
//! viewer waits for a key, so moving to one is a placement, or in sixel mode a write.

use std::path::PathBuf;

use nsxiv_ng_core::Size;
use nsxiv_ng_render::Canvas;
use nsxiv_ng_session::{Frame, Still};

use super::keep::FrameKey;
use super::kitty::{self, Crop};
use super::{frame, write_pixel_file};

/// A neighbor's frame to make: the still, the key and the place it will have once the viewer
/// shows the file, the scale it is drawn at, where its pixel file goes, if the terminal reads
/// them, where it goes as sixels the cell's pixels, the bytes it may take in the terminal,
/// and the layout it was made for.
pub struct Job {
    pub still: Still,
    pub key: FrameKey,
    pub crop: Option<Crop>,
    pub scale: f64,
    pub file: Option<PathBuf>,
    pub sixel: Option<(u32, u32)>,
    pub limit: usize,
    pub generation: u64,
}

/// What goes to the terminal of a frame made ahead.
pub enum Payload {
    File(PathBuf),
    Png(Vec<u8>),
    Sixel(String),
}

/// A frame made ahead: its key, its pixels, whether it has alpha, the bytes the
/// terminal stores for it, and what goes to the terminal, None when it would take more than
/// its job allowed or could not be written. Sixels come back whatever their bytes, as whether
/// they are kept is decided once they are back, by the room there is then.
pub struct Made {
    pub key: FrameKey,
    pub width: u32,
    pub height: u32,
    pub alpha: bool,
    pub bytes: usize,
    pub payload: Option<Payload>,
    pub generation: u64,
}

/// The key, place and scale of `still`'s frame, as the viewer's frame of the file will have
/// them once it is there: the canvas named in the still, the gamma and contrast `steps` of now,
/// cells of `cell` pixels, and `columns × rows` above the bar; through tmux and as sixels, no
/// crop.
pub fn plan(
    still: &Still,
    path: PathBuf,
    steps: (i32, i32),
    cell: (f64, f64),
    (columns, rows): (usize, usize),
    (tmux, sixel): (bool, bool),
) -> (FrameKey, Option<Crop>, f64) {
    let view = still.view();
    let canvas: Size = view.view_size();
    let (width, height, scale) = frame::frame_pixels(view, true, !sixel, sixel, cell.1, canvas);
    let key = FrameKey::ahead(path, view, steps, width, height);
    let crop = match tmux || sixel {
        true => None,
        false => kitty::crop(view, canvas, cell, scale, columns, rows),
    };
    (key, crop, scale)
}

/// The job's frame, drawn on the thread that calls it; one that panics comes back as a frame
/// that could not be made, its pixel file gone, so the next one goes ahead.
pub fn run(job: Job) -> Made {
    let (key, generation, file) = (job.key.clone(), job.generation, job.file.clone());
    caught(key, generation, file, || make(job))
}

// What `make` gives, or a frame that could not be made where it panics.
fn caught(
    key: FrameKey,
    generation: u64,
    file: Option<PathBuf>,
    make: impl FnOnce() -> Made,
) -> Made {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(make)).unwrap_or_else(|_| {
        if let Some(file) = file {
            let _ = std::fs::remove_file(file);
        }
        Made {
            key,
            width: 0,
            height: 0,
            alpha: false,
            bytes: 0,
            payload: None,
            generation,
        }
    })
}

/// What becomes of a frame made ahead that came back: what to send, or else whether its view
/// is to be skipped from now on, as one that could not be made or no longer fits. Its pixel
/// file goes where it is not sent. One that went stale meanwhile, for another `generation` or a
/// frame the terminal holds already, is dropped.
pub fn verdict(
    made: Made,
    generation: u64,
    kept: bool,
    fits: bool,
) -> Result<(FrameKey, Payload), Option<FrameKey>> {
    let fresh = made.generation == generation && !kept;
    match made.payload {
        Some(payload) if fresh && fits => Ok((made.key, payload)),
        payload => {
            if let Some(Payload::File(file)) = payload {
                let _ = std::fs::remove_file(file);
            }
            Err(fresh.then_some(made.key))
        }
    }
}

// The job's frame; the still goes as soon as it is drawn, as it holds the image.
fn make(job: Job) -> Made {
    let (width, height) = job
        .crop
        .map_or((job.key.width, job.key.height), |c| (c.width, c.height));
    let mut pixels = vec![0; width as usize * height as usize];
    let frame = Frame {
        clear: true,
        whole: job.crop.is_some(),
        kept: false,
    };
    let opaque = Canvas::new(&mut pixels, width, height)
        .is_some_and(|mut canvas| job.still.draw(&mut canvas, job.scale, frame));
    drop(job.still);
    if let Some(cell) = job.sixel {
        // as the loop's frame of the view: the picture's own pixels, from their cell
        let cut = frame::sixel_cut(&pixels, (width, height), true, cell.1);
        let sixels = frame::placed_sixels(&pixels, width, &cut, cell);
        let bytes = sixels.as_ref().map_or(0, String::len);
        return Made {
            key: job.key,
            width,
            height,
            alpha: !opaque,
            bytes,
            payload: sixels.map(Payload::Sixel),
            generation: job.generation,
        };
    }
    let alpha = !opaque;
    let channels = if job.file.is_some() && opaque { 3 } else { 4 };
    let bytes = channels * pixels.len();
    let payload = match job.file {
        _ if bytes > job.limit => None,
        Some(file) => write_pixel_file(&file, &pixels, alpha).then_some(Payload::File(file)),
        None => frame::png(&pixels, width, height, alpha).map(Payload::Png),
    };
    Made {
        key: job.key,
        width,
        height,
        alpha,
        bytes,
        payload,
        generation: job.generation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsxiv_ng_core::viewport::Viewport;

    fn key() -> FrameKey {
        FrameKey::ahead(
            PathBuf::from("/b.png"),
            &Viewport::default(),
            (0, 0),
            800,
            464,
        )
    }

    #[test]
    fn a_frame_that_panics_comes_back_as_one_that_could_not_be_made_its_file_gone() {
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("nsxiv-ng-ahead-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("ahead-1");
        let begun = file.clone();
        let made = caught(key(), 7, Some(file.clone()), move || {
            std::fs::write(&begun, b"half a frame").unwrap();
            panic!("a decoder's bug");
        });
        assert!(!file.exists(), "the pixel file begun goes");
        assert!(made.payload.is_none());
        // fresh, it is skipped from now on, so the next one goes ahead
        assert_eq!(verdict(made, 7, false, true).err(), Some(Some(key())));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_frame_made_ahead_goes_when_fresh_and_fitting_and_is_skipped_or_dropped_otherwise() {
        let made = |generation| Made {
            key: key(),
            width: 1,
            height: 1,
            alpha: false,
            bytes: 4,
            payload: Some(Payload::Png(vec![1])),
            generation,
        };
        assert!(verdict(made(3), 3, false, true).is_ok());
        assert_eq!(
            verdict(made(3), 3, false, false).err(),
            Some(Some(key())),
            "no room"
        );
        assert_eq!(
            verdict(made(2), 3, false, true).err(),
            Some(None),
            "another layout"
        );
        assert_eq!(
            verdict(made(3), 3, true, true).err(),
            Some(None),
            "kept meanwhile"
        );
    }
}
