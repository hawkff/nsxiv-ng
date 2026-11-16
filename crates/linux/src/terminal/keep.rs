//! The frames a kitty terminal keeps, to show again by placement alone, within a budget; in
//! sixel mode, the frames the viewer keeps itself, to write again without drawing them.

use std::path::PathBuf;

use nsxiv_ng_core::viewer::Viewer;
use nsxiv_ng_core::viewport::Viewport;

/// What a terminal keeps of the viewer's frames: within what herdr keeps a pane, 64 MiB, and
/// fewer than the ids tmux's placeholders tell apart. Sixel frames the viewer keeps take as
/// much of its own memory at most: 11 views of a photo at 3840x2160 or 40 at 1920x1080.
pub const BUDGET: usize = 56 << 20;
pub const LIMIT: usize = 128;

/// What decides the pixels of a frame of the image view, so a frame is shown again only where
/// it would be drawn the same.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameKey {
    pub path: PathBuf,
    pub frame: usize,
    pub rotation: u32,
    pub flips: (bool, bool),
    pub gamma: i32,
    pub contrast: i32,
    /// The image's place in the view, and its zoom.
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
    pub antialias: bool,
    pub alpha_layer: bool,
    /// The frame's pixels.
    pub width: u32,
    pub height: u32,
}

impl FrameKey {
    /// The key of the view's frame of `width × height` pixels, None in the grid, without an
    /// image, while the next file decodes, as the image drawn is still the last one, and for
    /// an animation, whose frame drawn may be an earlier one while the next decodes.
    pub fn of(viewer: &Viewer, width: u32, height: u32) -> Option<FrameKey> {
        let image = viewer.image()?;
        if viewer.thumbnail_mode() || viewer.awaited().is_some() || image.is_animated() {
            return None;
        }
        let mut key = FrameKey::ahead(
            viewer.current_entry()?.path.clone(),
            viewer.viewport(),
            (viewer.gamma(), viewer.contrast()),
            width,
            height,
        );
        key.frame = viewer.frame_index();
        key.rotation = viewer.rotation();
        key.flips = viewer.flips();
        Some(key)
    }

    /// The key a frame of `path` drawn ahead in `view` will have once the viewer shows the
    /// file: its first frame, unturned, with the gamma and contrast steps of now.
    pub fn ahead(
        path: PathBuf,
        view: &Viewport,
        (gamma, contrast): (i32, i32),
        width: u32,
        height: u32,
    ) -> FrameKey {
        let rect = view.image_rect();
        FrameKey {
            path,
            frame: 0,
            rotation: 0,
            flips: (false, false),
            gamma,
            contrast,
            x: rect.x,
            y: rect.y,
            zoom: view.zoom(),
            antialias: view.antialias,
            alpha_layer: view.alpha_layer,
            width,
            height,
        }
    }
}

/// A frame the terminal holds: its image id, and the bytes it stores for it.
#[derive(Clone, Debug, PartialEq)]
struct Held {
    key: FrameKey,
    id: u32,
    bytes: usize,
}

/// The frames the terminal holds, least recently shown first.
#[derive(Debug, Default)]
pub struct Kept {
    frames: Vec<Held>,
}

impl Kept {
    /// The id of the frame for `key`, now the most recently shown.
    pub fn get(&mut self, key: &FrameKey) -> Option<u32> {
        let at = self.frames.iter().position(|f| f.key == *key)?;
        let frame = self.frames.remove(at);
        let id = frame.id;
        self.frames.push(frame);
        Some(id)
    }

    pub fn contains(&self, key: &FrameKey) -> bool {
        self.frames.iter().any(|f| f.key == *key)
    }

    /// A frame the terminal now holds, the most recently shown.
    pub fn keep(&mut self, key: FrameKey, id: u32, bytes: usize) {
        self.frames.retain(|f| f.key != key && f.id != id);
        self.frames.push(Held { key, id, bytes });
    }

    /// The frames past the budget or the limit, least recently shown first, all but the
    /// `spared` ones, as the one shown: their ids, for the terminal to delete.
    pub fn trim(&mut self, spared: &[u32]) -> Vec<u32> {
        let mut total: usize = self.frames.iter().map(|f| f.bytes).sum();
        let mut dropped = Vec::new();
        let mut at = 0;
        while (total > BUDGET || self.frames.len() > LIMIT) && at < self.frames.len() {
            if spared.contains(&self.frames[at].id) {
                at += 1;
                continue;
            }
            let frame = self.frames.remove(at);
            total -= frame.bytes;
            dropped.push(frame.id);
        }
        dropped
    }

    /// What must stay in the terminal beside a frame sent ahead, and the ids of the kept frames
    /// among it: the frame on screen, of `shown` bytes, kept or not, and the frames of the
    /// neighbors `before` it, in the order the viewer goes, which one sent ahead must not push
    /// out, or the two would push each other out for as long as no key comes. Of a neighbor,
    /// that is the frame of the view it will have, by its planned key, or every frame of a file
    /// whose view cannot be planned.
    pub fn reserved(
        &self,
        shown: usize,
        before: &[(PathBuf, Option<FrameKey>)],
    ) -> (usize, Vec<u32>) {
        let mut reserved = (shown, Vec::new());
        for (path, planned) in before {
            let meant = |f: &&Held| match planned {
                Some(key) => f.key == *key,
                None => f.key.path == *path,
            };
            for frame in self.frames.iter().filter(meant) {
                reserved.0 += frame.bytes;
                reserved.1.push(frame.id);
            }
        }
        reserved
    }

    /// The bytes the terminal stores for frame `id`.
    pub fn bytes(&self, id: u32) -> Option<usize> {
        self.frames.iter().find(|f| f.id == id).map(|f| f.bytes)
    }

    /// The frames of these files: their ids.
    pub fn forget_files(&mut self, paths: &[PathBuf]) -> Vec<u32> {
        self.drop_where(|f| paths.contains(&f.key.path))
    }

    /// A frame the terminal no longer holds.
    pub fn forget(&mut self, id: u32) {
        self.drop_where(|f| f.id == id);
    }

    /// Every frame: their ids.
    pub fn clear(&mut self) -> Vec<u32> {
        self.drop_where(|_| true)
    }

    pub fn holds(&self, id: u32) -> bool {
        self.frames.iter().any(|f| f.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn ids(&self) -> Vec<u32> {
        self.frames.iter().map(|f| f.id).collect()
    }

    fn drop_where(&mut self, stale: impl Fn(&Held) -> bool) -> Vec<u32> {
        let ids = self
            .frames
            .iter()
            .filter(|f| stale(f))
            .map(|f| f.id)
            .collect();
        self.frames.retain(|f| !stale(f));
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, x: f64) -> FrameKey {
        FrameKey {
            path: PathBuf::from(name),
            frame: 0,
            rotation: 0,
            flips: (false, false),
            gamma: 0,
            contrast: 0,
            x,
            y: 0.0,
            zoom: 1.0,
            antialias: true,
            alpha_layer: false,
            width: 1000,
            height: 580,
        }
    }

    #[test]
    fn frames_past_the_budget_go_least_recently_shown_first_but_never_one_spared() {
        let mut kept = Kept::default();
        let frame = 24 << 20; // two fit in the budget, three do not
        kept.keep(key("a", 0.0), 1, frame);
        kept.keep(key("b", 0.0), 2, frame);
        assert!(kept.trim(&[2]).is_empty());
        kept.keep(key("c", 0.0), 3, frame);
        assert_eq!(kept.trim(&[3]), [1], "the least recently shown");
        // showing b again makes c the oldest, and the one shown stays though it is the oldest
        assert_eq!(kept.get(&key("b", 0.0)), Some(2));
        kept.keep(key("a", 0.0), 4, frame);
        assert_eq!(kept.trim(&[4]), [3]);
        // a frame just sent ahead is spared beside the one shown
        kept.keep(key("d", 0.0), 5, frame);
        assert_eq!(kept.trim(&[2, 5]), [4]);
        let mut huge = Kept::default();
        huge.keep(key("a", 0.0), 1, BUDGET + 1);
        assert!(
            huge.trim(&[1]).is_empty(),
            "the frame shown stays past the budget"
        );
        assert_eq!(kept.get(&key("c", 0.0)), None);
        assert_eq!(
            kept.get(&key("b", 1.0)),
            None,
            "another view of the same file"
        );
    }

    #[test]
    fn no_more_frames_than_the_limit_stay() {
        let mut kept = Kept::default();
        for id in 1..=LIMIT as u32 + 2 {
            kept.keep(key("a", f64::from(id)), id, 1);
        }
        assert_eq!(kept.trim(&[1]), [2, 3], "the oldest but the one shown");
    }

    #[test]
    fn the_files_named_take_their_frames_with_them() {
        let mut kept = Kept::default();
        kept.keep(key("a", 0.0), 1, 1);
        kept.keep(key("a", 5.0), 2, 1);
        kept.keep(key("b", 0.0), 3, 1);
        assert_eq!(kept.forget_files(&[PathBuf::from("a")]), [1, 2]);
        assert!(kept.forget_files(&[PathBuf::from("c")]).is_empty());
        assert!(kept.holds(3));
        kept.forget(3);
        assert!(kept.is_empty());
    }

    // the neighbor 3, planned in view x
    fn three(x: f64) -> [(PathBuf, Option<FrameKey>); 1] {
        [(PathBuf::from("3"), Some(key("3", x)))]
    }

    #[test]
    fn a_frame_ahead_leaves_room_for_the_one_shown_and_the_neighbor_before_it() {
        let frame = 24 << 20;
        let mut kept = Kept::default();
        kept.keep(key("2", 0.0), 2, frame);
        kept.keep(key("3", 0.0), 3, frame);
        // going forward from 2, shown: 3 first, with no neighbor before it, then 1 behind it
        let shown = kept.bytes(2).unwrap();
        assert_eq!(kept.reserved(shown, &[]), (frame, vec![]));
        // 1 must leave room for 3 too, so of the two only 3 fits beside the one shown
        let behind = kept.reserved(shown, &three(0.0));
        assert_eq!(behind, (2 * frame, vec![3]));
        assert!(behind.0 + frame > BUDGET);
        assert!(kept.reserved(shown, &[]).0 + frame <= BUDGET);
        // a file whose view cannot be planned keeps every frame of it
        let unplanned = [(PathBuf::from("3"), None)];
        assert_eq!(kept.reserved(shown, &unplanned), (2 * frame, vec![3]));
        // a frame on screen the terminal does not keep, as an animation's, counts all the same
        let mut unkept = Kept::default();
        unkept.keep(key("3", 0.0), 3, frame);
        let reserved = unkept.reserved(frame, &three(0.0));
        assert!(reserved.0 + frame > BUDGET, "{reserved:?}");
    }

    #[test]
    fn a_frame_sent_ahead_pushes_out_an_older_frame_not_the_next_files() {
        let frame = 18 << 20;
        let mut kept = Kept::default();
        // shown 2, an older frame of another file, the frame of 3 next, and 1 behind sent ahead
        kept.keep(key("2", 0.0), 2, frame);
        kept.keep(key("9", 0.0), 9, 4 << 20);
        kept.keep(key("3", 0.0), 3, frame);
        let (reserved, spared) = kept.reserved(frame, &three(0.0));
        assert!(reserved + frame <= BUDGET);
        kept.keep(key("1", 0.0), 1, frame);
        // were 3 the oldest unspared, it would go for 1 behind it
        kept.get(&key("9", 0.0));
        assert_eq!(kept.trim(&[&[2, 1][..], &spared].concat()), [9]);
        assert!(kept.holds(3));
    }

    #[test]
    fn of_two_views_of_the_next_file_the_one_it_will_have_stays() {
        let frame = 18 << 20;
        let mut kept = Kept::default();
        // shown 2; of 3 next, an older view, before a change of zoom, then the one planned
        kept.keep(key("2", 0.0), 2, frame);
        kept.keep(key("3", 0.0), 30, 4 << 20);
        kept.keep(key("3", 5.0), 35, frame);
        let (reserved, spared) = kept.reserved(frame, &three(5.0));
        assert_eq!((reserved, spared.clone()), (2 * frame, vec![35]));
        kept.keep(key("1", 0.0), 1, frame);
        assert_eq!(
            kept.trim(&[&[2, 1][..], &spared].concat()),
            [30],
            "the old view goes, not the one planned"
        );
        assert!(kept.holds(35));
    }
}
