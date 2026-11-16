//! The grid's thumbnails: made on worker threads through the freedesktop cache, visible cells
//! first, and kept in memory within a budget.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nsxiv_ng_decode::{DecodeBudget, Image, Thumbnails};
use nsxiv_ng_helper::Helpers;

/// The RGBA budget for cached thumbnails and rendered cells: 400 MiB, past which those out of
/// view go. Thumbnails fit the cache's folders, 256 pixels a side at scale 1 and 512 at scale
/// 2, so they may run to 1 MB each. The thumbnails as drawn in their cells count within it
/// too, and go first.
pub(crate) const BUDGET: usize = 400 << 20;

/// How long a worker waits for room in the decode budget while no decode ends, as one stuck on
/// a file that stopped answering may never end.
const STALL: Duration = Duration::from_secs(10);

/// A thumbnail a worker made, or None for a file that gives none, for the generation of its
/// file's thumbnails it was asked for.
pub(crate) struct Made {
    path: PathBuf,
    generation: Generation,
    image: Option<Image>,
}

/// What a thumbnail was asked for under: the side, and how often its file was invalidated.
type Generation = (u32, u64);

struct Job {
    path: PathBuf,
    generation: Generation,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

pub(crate) struct Thumbs {
    shared: Arc<Shared>,
    cache: Arc<Thumbnails>,
    /// Each thumbnail, with the number the store gave it when it came, which a cell drawn
    /// from it is kept under.
    store: HashMap<PathBuf, (u64, Image)>,
    /// The number the next thumbnail stored gets.
    next: u64,
    /// Each thumbnail as drawn in its cell, which a frame copies rather than draws it again.
    cells: HashMap<PathBuf, Cell>,
    /// The bytes of the thumbnails and the cells kept, and how many they may take.
    bytes: usize,
    budget: usize,
    /// Files that give no thumbnail, which are not asked for again until invalidated.
    failed: HashSet<PathBuf>,
    /// Files queued or being made.
    pending: HashSet<PathBuf>,
    invalidated: HashMap<PathBuf, u64>,
    /// The side in pixels thumbnails are made to cover.
    side: u32,
}

impl Thumbs {
    /// Starts `workers` threads that make thumbnails through the cache, decoding through the
    /// helpers within `max_bytes`, and pass each to `deliver`, which must return at once. The
    /// decodes in this process together hold no more than `max_bytes`, as one image may, until
    /// their pictures are scaled down: a worker waits for the others' to end where its own
    /// would pass that, as long as some end.
    /// Thumbnails out of view go once those kept take more than `budget` bytes.
    pub(crate) fn new(
        private: bool,
        budget: usize,
        workers: usize,
        helpers: Arc<Helpers>,
        max_bytes: u64,
        deliver: impl Fn(Made) + Send + Sync + 'static,
    ) -> Thumbs {
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            wake: Condvar::new(),
        });
        let cache = Arc::new(Thumbnails::new(private));
        let deliver = Arc::new(deliver);
        let decodes = Arc::new(DecodeBudget::new(max_bytes, STALL));
        for _ in 0..workers {
            let (shared, cache) = (Arc::clone(&shared), Arc::clone(&cache));
            let (helpers, deliver) = (Arc::clone(&helpers), Arc::clone(&deliver));
            let decodes = Arc::clone(&decodes);
            std::thread::Builder::new()
                .name("thumbnail".to_owned())
                .spawn(move || {
                    while let Some(Job { path, generation }) = next(&shared) {
                        // the decode's share of the budget stays until the whole picture is
                        // scaled down and freed, when get returns
                        let share = decodes.share();
                        // a panic in reading, scaling or writing a thumbnail fails its file,
                        // not the worker, which still owes an answer
                        let image = std::panic::catch_unwind(AssertUnwindSafe(|| {
                            cache.get(&path, generation.0, |fits| {
                                helpers.thumbnail(&path, fits, max_bytes, Some(&share))
                            })
                        }))
                        .ok()
                        .and_then(Result::ok);
                        drop(share);
                        deliver(Made {
                            path,
                            generation,
                            image,
                        });
                    }
                })
                .expect("a thumbnail thread");
        }
        Thumbs {
            shared,
            cache,
            store: HashMap::new(),
            next: 0,
            cells: HashMap::new(),
            bytes: 0,
            budget,
            failed: HashSet::new(),
            pending: HashSet::new(),
            invalidated: HashMap::new(),
            side: 0,
        }
    }

    pub(crate) fn get(&self, path: &Path) -> Option<&Image> {
        self.store.get(path).map(|(_, image)| image)
    }

    /// The pixels of `path`'s thumbnail as drawn at `size` over `background`, where they are
    /// kept for the thumbnail it has now.
    pub(crate) fn cell(&self, path: &Path, size: (u32, u32), background: u32) -> Option<&[u32]> {
        let (number, _) = self.store.get(path)?;
        let cell = self.cells.get(path)?;
        let same = cell.thumbnail == *number && cell.size == size && cell.background == background;
        same.then_some(&cell.pixels[..])
    }

    /// Whether a cell of `size` for `path`'s thumbnail would be kept: the budget has room for
    /// it, once the cells of the files out of view go where it has not. The files in view are
    /// worked out only where that is so, and once for all the cells of a frame.
    // shortcut: a frame's cells are each asked against the store as it was before any of them
    // is kept, so near the budget a capture may be thrown away or a cell kept a frame late;
    // count the frame's planned bytes as it goes if that waste shows
    pub(crate) fn room<'a, F: FnMut() -> HashSet<&'a Path>>(
        &self,
        path: &Path,
        size: (u32, u32),
        in_view: &mut InView<'a, F>,
    ) -> bool {
        if !self.store.contains_key(path) {
            return false;
        }
        let need = size.0 as usize * size.1 as usize * 4;
        let used = self.bytes - self.cells.get(path).map_or(0, Cell::bytes);
        if used + need <= self.budget {
            return true;
        }
        // no cell of another file that could go
        if self.cells.keys().all(|kept| kept == path) {
            return false;
        }
        let in_view = in_view.get();
        let out: usize = (self.cells.iter())
            .filter(|(kept, _)| *kept != path && !in_view.contains(kept.as_path()))
            .map(|(_, cell)| cell.bytes())
            .sum();
        used - out + need <= self.budget
    }

    /// Keeps `pixels`, `path`'s thumbnail as drawn at `size` over `background`, in place of
    /// any it had, where the budget has room once the cells of the files out of view go: a
    /// cell never takes a thumbnail's room.
    pub(crate) fn keep<'a, F: FnMut() -> HashSet<&'a Path>>(
        &mut self,
        path: &Path,
        size: (u32, u32),
        background: u32,
        pixels: Vec<u32>,
        in_view: &mut InView<'a, F>,
    ) {
        let Some((number, _)) = self.store.get(path) else {
            return;
        };
        let cell = Cell {
            thumbnail: *number,
            size,
            background,
            pixels,
        };
        self.drop_cell(path);
        if self.bytes + cell.bytes() > self.budget && !self.cells.is_empty() {
            let in_view = in_view.get();
            self.drop_cells(|path| !in_view.contains(path));
        }
        if self.bytes + cell.bytes() <= self.budget {
            self.bytes += cell.bytes();
            self.cells.insert(path.to_owned(), cell);
        }
    }

    /// The bytes the cells kept hold, for tests.
    #[cfg(test)]
    pub(crate) fn cell_bytes(&self) -> usize {
        self.cells.values().map(Cell::bytes).sum()
    }

    // Drops `path`'s cell.
    fn drop_cell(&mut self, path: &Path) {
        if let Some(old) = self.cells.remove(path) {
            self.bytes -= old.bytes();
        }
    }

    // Drops the cells of the files `goes` picks.
    fn drop_cells(&mut self, goes: impl Fn(&Path) -> bool) {
        let bytes = &mut self.bytes;
        self.cells.retain(|path, cell| {
            let keep = !goes(path);
            if !keep {
                *bytes -= cell.bytes();
            }
            keep
        });
    }

    /// Asks for the thumbnails of `wanted` that are missing, in this order, `side` pixels a
    /// side. Work asked for before and not yet begun goes. A new side drops every thumbnail.
    pub(crate) fn want(&mut self, wanted: &[&Path], side: u32) {
        if side != self.side {
            self.side = side;
            self.store.clear();
            self.cells.clear();
            self.bytes = 0;
            self.failed.clear();
            // what is being made for the old side is not taken when it comes
            self.pending.clear();
        }
        let mut queue = self.shared.queue.lock().unwrap();
        for job in queue.jobs.drain(..) {
            self.pending.remove(&job.path);
        }
        for &path in wanted {
            if !self.store.contains_key(path)
                && !self.failed.contains(path)
                && self.pending.insert(path.to_owned())
            {
                queue.jobs.push_back(Job {
                    path: path.to_owned(),
                    generation: self.generation(path),
                });
            }
        }
        let queued = !queue.jobs.is_empty();
        drop(queue);
        if queued {
            self.shared.wake.notify_all();
        }
    }

    /// Takes a thumbnail a worker made, unless its file was invalidated or the side changed
    /// since it was asked for. Past the budget, every thumbnail out of the view that `visible`
    /// gives goes, the new one too when it is out of view, so the cells in view always keep
    /// theirs. True when the store took it.
    pub(crate) fn made<'a>(
        &mut self,
        made: Made,
        visible: impl FnOnce() -> HashSet<&'a Path>,
    ) -> bool {
        if made.generation != self.generation(&made.path) {
            return false;
        }
        self.pending.remove(&made.path);
        let Some(image) = made.image else {
            self.failed.insert(made.path);
            return false;
        };
        self.bytes += image.rgba.len();
        self.next += 1;
        if let Some((_, old)) = self.store.insert(made.path.clone(), (self.next, image)) {
            self.bytes -= old.rgba.len();
        }
        // the cell drawn from the thumbnail before goes with it
        self.drop_cell(&made.path);
        // shortcut: past the budget, the cells go first, then every thumbnail out of view; an
        // LRU would keep more when a huge grid is scrolled back and forth
        if self.bytes > self.budget {
            self.drop_cells(|_| true);
        }
        if self.bytes > self.budget {
            let visible = visible();
            let bytes = &mut self.bytes;
            self.store.retain(|path, (_, image)| {
                let keep = visible.contains(path.as_path());
                if !keep {
                    *bytes -= image.rgba.len();
                }
                keep
            });
        }
        true
    }

    /// Drops these files' thumbnails, in memory and in the cache, so they are made again;
    /// one being made now is not taken.
    pub(crate) fn forget(&mut self, paths: &[PathBuf]) {
        for path in paths {
            self.cache.forget(path);
            if let Some((_, old)) = self.store.remove(path) {
                self.bytes -= old.rgba.len();
            }
            self.drop_cell(path);
            self.failed.remove(path);
            self.pending.remove(path);
            *self.invalidated.entry(path.clone()).or_default() += 1;
        }
    }

    /// The files asked for and not yet in, and the side asked for, for tests.
    #[cfg(test)]
    pub(crate) fn asked(&self) -> (usize, u32) {
        (self.pending.len(), self.side)
    }

    /// A thumbnail of a pixel for `path`, as a worker would make it now, for tests.
    #[cfg(test)]
    pub(crate) fn made_now(&self, path: &Path) -> Made {
        Made {
            path: path.to_owned(),
            generation: self.generation(path),
            image: Some(Image {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
                may_animate: false,
            }),
        }
    }

    fn generation(&self, path: &Path) -> Generation {
        (self.side, self.invalidated.get(path).copied().unwrap_or(0))
    }
}

impl Drop for Thumbs {
    fn drop(&mut self) {
        // each worker finishes the thumbnail it is making, then stops
        self.shared.queue.lock().unwrap().closed = true;
        self.shared.wake.notify_all();
    }
}

/// The files in view, worked out from `build` the first time they are asked for.
pub(crate) struct InView<'a, F> {
    files: Option<HashSet<&'a Path>>,
    build: F,
}

impl<'a, F: FnMut() -> HashSet<&'a Path>> InView<'a, F> {
    pub(crate) fn new(build: F) -> Self {
        InView { files: None, build }
    }

    fn get(&mut self) -> &HashSet<&'a Path> {
        self.files.get_or_insert_with(&mut self.build)
    }
}

/// A thumbnail as drawn in its cell: the thumbnail's number, the size drawn, the background
/// under it, and the pixels.
struct Cell {
    thumbnail: u64,
    size: (u32, u32),
    background: u32,
    pixels: Vec<u32>,
}

impl Cell {
    fn bytes(&self) -> usize {
        self.pixels.len() * 4
    }
}

// The next job, waiting for one; None once the thumbnails are dropped.
fn next(shared: &Shared) -> Option<Job> {
    let mut queue = shared.queue.lock().unwrap();
    loop {
        if queue.closed {
            return None;
        }
        if let Some(job) = queue.jobs.pop_front() {
            return Some(job);
        }
        queue = shared.wake.wait(queue).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn image() -> Image {
        Image {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            may_animate: false,
        }
    }

    // thumbnails with no workers, so the queue stays as asked, keeping `budget` bytes
    fn idle(budget: usize) -> Thumbs {
        let helpers = Helpers::new("nsxiv-ng".into(), 1, 1 << 20, Duration::from_secs(1));
        Thumbs::new(true, budget, 0, Arc::new(helpers), 1 << 20, |_| {})
    }

    fn queued(thumbs: &Thumbs) -> Vec<PathBuf> {
        let queue = thumbs.shared.queue.lock().unwrap();
        queue.jobs.iter().map(|job| job.path.clone()).collect()
    }

    fn made(thumbs: &Thumbs, path: &str) -> Made {
        Made {
            path: path.into(),
            generation: thumbs.generation(Path::new(path)),
            image: Some(image()),
        }
    }

    #[test]
    fn work_for_cells_out_of_view_goes() {
        let mut thumbs = idle(BUDGET);
        let paths: Vec<PathBuf> = (0..4).map(|i| PathBuf::from(format!("{i}"))).collect();
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        thumbs.want(&refs[..2], 160);
        assert_eq!(queued(&thumbs), &paths[..2]);
        thumbs.want(&refs[2..], 160);
        assert_eq!(
            queued(&thumbs),
            &paths[2..],
            "a scroll drops what was not begun"
        );
        assert!(thumbs.made(made(&thumbs, "2"), HashSet::new));
        thumbs.want(&refs, 160);
        assert_eq!(
            queued(&thumbs),
            [paths[0].clone(), paths[1].clone(), paths[3].clone()],
            "in the order asked, without the thumbnail kept"
        );
    }

    #[test]
    fn an_invalidated_thumbnail_is_made_again_and_the_old_one_never_lands() {
        let mut thumbs = idle(BUDGET);
        thumbs.want(&[Path::new("a")], 160);
        let old = made(&thumbs, "a");
        thumbs.forget(&["a".into()]);
        assert!(
            !thumbs.made(old, HashSet::new),
            "made before the invalidation"
        );
        thumbs.want(&[Path::new("a")], 160);
        assert_eq!(queued(&thumbs), [PathBuf::from("a")]);
        assert!(thumbs.made(made(&thumbs, "a"), HashSet::new));
        assert!(thumbs.get(Path::new("a")).is_some());
        thumbs.want(&[Path::new("a")], 320);
        assert!(
            thumbs.get(Path::new("a")).is_none() && queued(&thumbs).len() == 1,
            "a new side makes every thumbnail again"
        );
    }

    // A worker's turn: every job queued made, counted by file, with `view` in view.
    fn work(thumbs: &mut Thumbs, view: &[&Path], count: &mut HashMap<PathBuf, usize>) {
        let jobs: Vec<Job> = thumbs.shared.queue.lock().unwrap().jobs.drain(..).collect();
        for Job { path, generation } in jobs {
            *count.entry(path.clone()).or_default() += 1;
            let done = Made {
                path,
                generation,
                image: Some(image()),
            };
            let visible = || view.iter().copied().collect();
            assert!(thumbs.made(done, visible), "every one asked for lands");
        }
    }

    #[test]
    fn every_cell_in_view_gets_its_thumbnail_past_the_budget() {
        // ten thumbnails' bytes, and fifteen cells in view
        let mut thumbs = idle(10 * 4);
        let paths: Vec<PathBuf> = (0..30).map(|i| PathBuf::from(i.to_string())).collect();
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let (first, second) = refs.split_at(15);
        let mut count = HashMap::new();
        for view in [first, second, first] {
            thumbs.want(view, 160);
            work(&mut thumbs, view, &mut count);
            assert!(view.iter().all(|path| thumbs.get(path).is_some()));
            thumbs.want(view, 160);
            assert_eq!(
                queued(&thumbs),
                [] as [PathBuf; 0],
                "none is asked for again"
            );
        }
        assert!(
            first.iter().all(|path| count[*path] == 2) && second.iter().all(|p| count[*p] == 1),
            "once each time a cell comes into view without its thumbnail: {count:?}"
        );
    }

    #[test]
    fn thumbnails_out_of_view_stay_within_the_budget() {
        let mut thumbs = idle(10 * 4);
        let view = [Path::new("0"), Path::new("1")];
        for i in 0..100 {
            thumbs.made(made(&thumbs, &i.to_string()), || view.into_iter().collect());
            let size = |(_, (_, image)): (&PathBuf, &(u64, Image))| image.rgba.len();
            let out: usize = thumbs
                .store
                .iter()
                .filter(|(path, _)| !view.contains(&path.as_path()))
                .map(size)
                .sum();
            assert!(out <= thumbs.budget, "{out} bytes out of view");
            assert_eq!(thumbs.bytes, thumbs.store.iter().map(size).sum::<usize>());
        }
        assert!(
            view.iter().all(|path| thumbs.get(path).is_some()),
            "the cells in view stay"
        );
    }

    #[test]
    fn under_pressure_the_files_in_view_are_worked_out_once_and_only_where_cells_could_go() {
        // a thumbnail's bytes and room for a cell of a pixel
        let mut thumbs = idle(8);
        assert!(thumbs.made(made(&thumbs, "a"), HashSet::new));
        assert!(thumbs.made(made(&thumbs, "b"), HashSet::new));
        let (a, b) = (Path::new("a"), Path::new("b"));
        let builds = std::cell::Cell::new(0);
        let mut in_view = InView::new(|| {
            builds.set(builds.get() + 1);
            [a, b].into_iter().collect()
        });
        // the budget is full of thumbnails and no cell could go: no room, and no set built
        assert!(!thumbs.room(a, (1, 1), &mut in_view));
        thumbs.keep(a, (1, 1), 0, vec![0], &mut in_view);
        assert_eq!((builds.get(), thumbs.cell_bytes()), (0, 0));
        // a cell kept with room, then two that would need it to go, though it is in view
        let mut thumbs = idle(12);
        assert!(thumbs.made(made(&thumbs, "a"), HashSet::new));
        assert!(thumbs.made(made(&thumbs, "b"), HashSet::new));
        thumbs.keep(a, (1, 1), 0, vec![0], &mut in_view);
        assert_eq!(builds.get(), 0, "a cell with room asks for nothing");
        assert!(!thumbs.room(b, (1, 1), &mut in_view));
        assert!(!thumbs.room(b, (2, 1), &mut in_view));
        thumbs.keep(b, (1, 1), 0, vec![0], &mut in_view);
        assert_eq!(builds.get(), 1, "the files in view once for the frame");
        assert!(thumbs.cell(a, (1, 1), 0).is_some() && thumbs.cell(b, (1, 1), 0).is_none());
    }

    #[test]
    fn a_cell_is_kept_for_its_thumbnail_its_size_and_its_background() {
        let mut thumbs = idle(BUDGET);
        assert!(thumbs.made(made(&thumbs, "a"), HashSet::new));
        let a = Path::new("a");
        thumbs.keep(a, (2, 1), 7, vec![1, 2], &mut InView::new(HashSet::new));
        assert_eq!(thumbs.cell(a, (2, 1), 7), Some(&[1, 2][..]));
        assert_eq!(thumbs.cell(a, (1, 2), 7), None, "another size");
        assert_eq!(thumbs.cell(a, (2, 1), 8), None, "another background");
        assert_eq!(thumbs.cell(Path::new("b"), (2, 1), 7), None, "another file");
        assert!(thumbs.made(made(&thumbs, "a"), HashSet::new));
        assert_eq!(thumbs.cell(a, (2, 1), 7), None, "a thumbnail made anew");
        thumbs.keep(a, (2, 1), 7, vec![1, 2], &mut InView::new(HashSet::new));
        thumbs.forget(&[a.to_owned()]);
        assert_eq!(thumbs.cell(a, (2, 1), 7), None, "a thumbnail forgotten");
        assert_eq!(thumbs.bytes, 0);
    }

    // The store's bytes as counted from what it holds: the thumbnails and the cells.
    fn counted(thumbs: &Thumbs) -> usize {
        let images: usize = thumbs
            .store
            .values()
            .map(|(_, image)| image.rgba.len())
            .sum();
        images + thumbs.cell_bytes()
    }

    #[test]
    fn the_cells_count_within_the_budget_and_go_before_a_thumbnail() {
        // ten thumbnails' bytes: four of a pixel each, and room for cells
        let mut thumbs = idle(10 * 4);
        for name in ["a", "b", "c"] {
            assert!(thumbs.made(made(&thumbs, name), HashSet::new));
        }
        let (a, b) = (Path::new("a"), Path::new("b"));
        thumbs.keep(a, (4, 1), 0, vec![0; 4], &mut InView::new(HashSet::new));
        assert_eq!((thumbs.bytes, counted(&thumbs)), (28, 28));
        // b's cell takes the room of a's, which is out of view, not a thumbnail's
        thumbs.keep(
            b,
            (4, 1),
            0,
            vec![0; 4],
            &mut InView::new(|| [b].into_iter().collect()),
        );
        assert!(thumbs.cell(a, (4, 1), 0).is_none() && thumbs.cell(b, (4, 1), 0).is_some());
        assert_eq!((thumbs.bytes, counted(&thumbs)), (28, 28));
        // a cell with no room once those out of view went is not kept
        thumbs.keep(
            a,
            (8, 1),
            0,
            vec![0; 8],
            &mut InView::new(|| [a, b].into_iter().collect()),
        );
        assert!(thumbs.cell(a, (8, 1), 0).is_none());
        assert_eq!(thumbs.bytes, 28);
        // a thumbnail past the budget drops the cells before any thumbnail
        for i in 0..6 {
            assert!(thumbs.made(made(&thumbs, &i.to_string()), HashSet::new));
        }
        assert_eq!(thumbs.cell_bytes(), 0);
        assert_eq!(thumbs.store.len(), 9, "every thumbnail stays");
        assert_eq!((thumbs.bytes, counted(&thumbs)), (36, 36));
        assert!(thumbs.bytes <= thumbs.budget);
    }
}
