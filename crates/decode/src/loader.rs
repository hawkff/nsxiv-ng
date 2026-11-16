//! Decoding on worker threads: the file the viewer waits for first, then the files it may
//! show next, with a cache of decoded images that answers revisits while files stay unchanged.

use std::collections::VecDeque;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use crate::{DecodeError, Image, Pyramid, memory_needed};

/// The answer to a [`Loader::load`] request: the decoded image with its pyramid.
#[derive(Debug)]
pub struct Loaded {
    pub request: u64,
    pub pyramid: Result<Arc<Pyramid>, DecodeError>,
    /// The file's stamp, taken before its pixels were read, or None when it could not be read.
    pub stamp: Option<Stamp>,
}

type Decode = dyn Fn(&Path, u64) -> Result<Image, DecodeError> + Send + Sync;
type Deliver = dyn Fn(Loaded) + Send + Sync;
type Ahead = dyn Fn(&Path, &Arc<Pyramid>, Stamp) + Send + Sync;

/// Worker threads that decode files for the viewer: the file it waits for first, then those
/// [`Loader::prefetch`] lists, while a cache keeps decoded images for revisits.
pub struct Loader {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    threads: usize,
    max_image_bytes: u64,
    decode: Box<Decode>,
    deliver: Box<Deliver>,
    /// Told of each file decoded ahead, once [`Loader::tell_ahead`] set it.
    ahead: Mutex<Option<Arc<Ahead>>>,
}

#[derive(Default)]
struct State {
    /// The request the viewer waits for, until a worker takes it.
    current: Option<(u64, PathBuf)>,
    /// Files to decode ahead, the likeliest next first.
    ahead: VecDeque<PathBuf>,
    running: Vec<Running>,
    /// The number of the last job a worker took.
    jobs: u64,
    cache: Cache,
    closed: bool,
}

struct Running {
    job: u64,
    path: PathBuf,
    /// The request this decode answers, while the viewer still waits for it.
    answer: Option<u64>,
    /// The file was invalidated after this decode began, so its pixels may be old ones.
    stale: bool,
}

impl Loader {
    /// Starts `threads` workers, at least two, so one stays free for the file the viewer waits
    /// for. `max_image_bytes` caps each decode, as [`crate::decode`] takes it, and the image
    /// with the pyramid built after it ([`memory_needed`]); `cache_bytes` caps the images and
    /// pyramids kept for revisits, and decoding ahead skips files the cache cannot hold.
    /// Answers go to `deliver`, which runs on a worker thread and should return at once, as a
    /// channel send does.
    pub fn new(
        threads: usize,
        max_image_bytes: u64,
        cache_bytes: u64,
        deliver: impl Fn(Loaded) + Send + Sync + 'static,
    ) -> Loader {
        Loader::with_decoder(
            threads,
            max_image_bytes,
            cache_bytes,
            crate::decode,
            deliver,
        )
    }

    /// [`Loader::new`] with the function that decodes, as one that sends some formats to a
    /// decoding helper. Its images get their pyramids on the worker too.
    pub fn with_decoder(
        threads: usize,
        max_image_bytes: u64,
        cache_bytes: u64,
        decode: impl Fn(&Path, u64) -> Result<Image, DecodeError> + Send + Sync + 'static,
        deliver: impl Fn(Loaded) + Send + Sync + 'static,
    ) -> Loader {
        let (decode, deliver): (Box<Decode>, Box<Deliver>) = (Box::new(decode), Box::new(deliver));
        let threads = threads.max(2);
        let cache = Cache {
            budget: cache_bytes,
            ..Cache::default()
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                cache,
                ..State::default()
            }),
            wake: Condvar::new(),
            threads,
            max_image_bytes,
            decode,
            deliver,
            ahead: Mutex::default(),
        });
        for _ in 0..threads {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("decode".to_owned())
                .spawn(move || work(&shared))
                .expect("a decode thread");
        }
        Loader { shared }
    }

    /// Decodes the file for the viewer's request and passes the answer to `deliver`. The
    /// viewer waits for one request at a time, so an earlier request stops getting an answer,
    /// though one already on its way may still arrive, and a decode under way still fills the
    /// cache. A decode of the file already under way answers the request.
    pub fn load(&self, request: u64, path: PathBuf) {
        let mut state = self.shared.state.lock().unwrap();
        for running in &mut state.running {
            running.answer = None;
        }
        state.cache.pinned = Some(path.clone());
        // joined here, under the lock, a decode cannot end unseen before a worker gets to it;
        // a decode that began before an invalidation may hold old pixels, so it answers nothing
        match state
            .running
            .iter_mut()
            .find(|r| r.path == path && !r.stale)
        {
            Some(running) => {
                running.answer = Some(request);
                state.current = None;
            }
            None => state.current = Some((request, path)),
        }
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Decodes these files into the cache, in this order, while a worker besides the one kept
    /// free is idle. The list replaces the one before.
    pub fn prefetch(&self, paths: Vec<PathBuf>) {
        self.shared.state.lock().unwrap().ahead = paths.into();
        self.shared.wake.notify_all();
    }

    /// Has `ahead` told, on a worker thread, of every decode that answers no request, as of a
    /// file [`Loader::prefetch`] lists or a load the viewer moved past, once its image and
    /// pyramid are in the cache, decoded and kept or found there: the file, them, and the stamp
    /// taken before its pixels were read. One the cache has no room for is not told. It must
    /// return at once, as a channel send does. A later call replaces the one before.
    pub fn tell_ahead(&self, ahead: impl Fn(&Path, &Arc<Pyramid>, Stamp) + Send + Sync + 'static) {
        *self.shared.ahead.lock().unwrap() = Some(Arc::new(ahead));
    }

    /// Forgets these files' decoded images, so the next load reads them again; a decode of one
    /// already under way neither fills the cache nor answers a later request.
    pub fn invalidate(&self, paths: &[PathBuf]) {
        let mut state = self.shared.state.lock().unwrap();
        for path in paths {
            state.cache.remove(path);
        }
        for running in &mut state.running {
            if paths.contains(&running.path) {
                running.stale = true;
            }
        }
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        // each worker finishes the decode it is in, then stops
        self.shared.state.lock().unwrap().closed = true;
        self.shared.wake.notify_all();
    }
}

fn work(shared: &Shared) {
    let mut state = shared.state.lock().unwrap();
    loop {
        if state.closed {
            return;
        }
        let (path, answer, cap) = if let Some((request, path)) = state.current.take() {
            (path, Some(request), shared.max_image_bytes)
        } else if state.running.len() + 1 < shared.threads
            && let Some(path) = state.ahead.pop_front()
        {
            if state.running.iter().any(|r| r.path == path && !r.stale) {
                continue;
            }
            // a file decoded ahead is for the cache, so one too large for it is not decoded
            (path, None, shared.max_image_bytes.min(state.cache.budget))
        } else {
            state = shared.wake.wait(state).unwrap();
            continue;
        };
        state.jobs += 1;
        let job = state.jobs;
        state.running.push(Running {
            job,
            path: path.clone(),
            answer,
            stale: false,
        });
        drop(state);
        let (stamp, decoded, pyramid) = shared.load(&path, cap);
        state = shared.state.lock().unwrap();
        let i = state
            .running
            .iter()
            .position(|r| r.job == job)
            .expect("a job stays listed until it ends");
        let running = state.running.swap_remove(i);
        // too large to decode ahead, but the viewer now waits for it: once more, under the full cap
        let retry = cap < shared.max_image_bytes
            && matches!(pyramid, Err(DecodeError::TooLarge { .. }))
            && state.current.is_none();
        if let Some(request) = running.answer.filter(|_| retry) {
            state.current = Some((request, path));
            shared.wake.notify_all();
            continue;
        }
        // in the cache now: found there, or decoded and kept
        let kept = match (&pyramid, stamp, running.stale) {
            (Ok(pyramid), Some(stamp), false) if decoded => {
                state.cache.insert(path.clone(), stamp, Arc::clone(pyramid))
            }
            (Ok(_), Some(_), false) => true,
            _ => false,
        };
        drop(state);
        if let (None, Ok(pyramid), Some(stamp), true) = (running.answer, &pyramid, stamp, kept) {
            // taken out of the lock, so a callback may set another one
            let ahead = shared.ahead.lock().unwrap().clone();
            if let Some(ahead) = ahead {
                // a callback that panics loses its news, not the worker
                let _ = std::panic::catch_unwind(AssertUnwindSafe(|| ahead(&path, pyramid, stamp)));
            }
        }
        if let Some(request) = running.answer {
            // a callback that panics loses its answer, not the worker
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                (shared.deliver)(Loaded {
                    request,
                    pyramid,
                    stamp,
                });
            }));
        }
        // a worker waiting for room to decode ahead may have it now
        shared.wake.notify_all();
        state = shared.state.lock().unwrap();
    }
}

impl Shared {
    // The file's stamp, whether its image was decoded now rather than found in the cache, and
    // its image and pyramid: the cached ones while the file is unchanged, or else a new decode.
    // The stamp comes first, before any pixel is read, so a later change always shows; it is
    // missing when the file cannot be read.
    fn load(
        &self,
        path: &Path,
        cap: u64,
    ) -> (Option<Stamp>, bool, Result<Arc<Pyramid>, DecodeError>) {
        let stamp = match std::fs::metadata(path) {
            Ok(meta) => stamp(&meta),
            Err(e) => return (None, false, Err(DecodeError::Unreadable(e.to_string()))),
        };
        let cached = self.state.lock().unwrap().cache.get(path, stamp);
        if let Some(pyramid) = cached {
            return (Some(stamp), false, Ok(pyramid));
        }
        // a decoder that panics fails its file, not the worker, which still owes an answer
        let pyramid = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let image = (self.decode)(path, cap)?;
            let pixels = u128::from(image.width) * u128::from(image.height);
            if image.rgba.len() as u128 != pixels * 4 {
                return Err(DecodeError::Unreadable(
                    "the decoder gave a wrong number of pixels".to_owned(),
                ));
            }
            // the decoder may be a helper's, whose cap counts no levels
            if memory_needed(image.width, image.height) > cap {
                return Err(DecodeError::TooLarge {
                    size: Some((image.width, image.height)),
                });
            }
            Ok(Arc::new(Pyramid::new(image)))
        }))
        .unwrap_or_else(|_| Err(DecodeError::Unreadable("the decoder crashed".to_owned())));
        (Some(stamp), true, pyramid)
    }
}

/// What tells a changed file from an unchanged one: two stamps of a file compare equal while
/// it is unchanged. They hold its device, inode and size, and its modification and change
/// times, which every write moves; a file saved by renaming another over it has a new inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp((u64, u64, u64, i64, i64, i64, i64));

fn stamp(meta: &Metadata) -> Stamp {
    Stamp((
        meta.dev(),
        meta.ino(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    ))
}

/// Decoded images with their pyramids within a byte budget, least recently used first.
#[derive(Default)]
struct Cache {
    budget: u64,
    used: u64,
    // shortcut: linear scans, quick for the few hundred images a budget holds; a map would
    // suit budgets that hold many thousands of tiny images
    entries: Vec<Entry>,
    /// The file the viewer asked for last, which making room never drops.
    pinned: Option<PathBuf>,
}

struct Entry {
    path: PathBuf,
    stamp: Stamp,
    pyramid: Arc<Pyramid>,
}

fn size(pyramid: &Pyramid) -> u64 {
    pyramid.levels.iter().map(|l| l.rgba.len() as u64).sum()
}

impl Cache {
    /// The file's image, while the stamp says the file is unchanged.
    /// A miss drops nothing, since a worker may ask with a stamp older than the entry's; a
    /// changed file's old pixels go once its new ones replace them.
    fn get(&mut self, path: &Path, stamp: Stamp) -> Option<Arc<Pyramid>> {
        let i = self
            .entries
            .iter()
            .position(|e| e.path == path && e.stamp == stamp)?;
        let entry = self.entries.remove(i);
        let pyramid = Arc::clone(&entry.pyramid);
        self.entries.push(entry);
        Some(pyramid)
    }

    /// Keeps the image, making room by dropping the least recently used, never the pinned
    /// file's. An image that cannot fit beside that one stays out, and nothing goes for it.
    fn insert(&mut self, path: PathBuf, stamp: Stamp, pyramid: Arc<Pyramid>) -> bool {
        self.remove(&path);
        let bytes = size(&pyramid);
        let pinned = self
            .entries
            .iter()
            .find(|e| self.pinned.as_ref() == Some(&e.path))
            .map_or(0, |e| size(&e.pyramid));
        if bytes + pinned > self.budget {
            return false;
        }
        while self.used + bytes > self.budget {
            let i = self
                .entries
                .iter()
                .position(|e| self.pinned.as_ref() != Some(&e.path))
                .expect("room for the image was checked");
            let gone = self.entries.remove(i);
            self.used -= size(&gone.pyramid);
        }
        self.used += bytes;
        self.entries.push(Entry {
            path,
            stamp,
            pyramid,
        });
        true
    }

    fn remove(&mut self, path: &Path) {
        if let Some(i) = self.entries.iter().position(|e| e.path == path) {
            let gone = self.entries.remove(i);
            self.used -= size(&gone.pyramid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const PLENTY: u64 = 1 << 30;

    // a decode that started: its file, its cap, and the sender that lets it finish
    type Start = (PathBuf, u64, mpsc::Sender<()>);

    // empty files in a fresh directory, there for their stamps
    fn files<const N: usize>(test: &str, names: [&str; N]) -> (PathBuf, [PathBuf; N]) {
        let dir =
            std::env::temp_dir().join(format!("nsxiv-ng-loader-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = names.map(|name| {
            let path = dir.join(name);
            std::fs::write(&path, b"").unwrap();
            path
        });
        (dir, paths)
    }

    // A loader whose decoder the test drives: each decode reports its start, then waits to be
    // let go. Its image is as wide as the count of decodes started, and too large where four
    // bytes a pixel pass the cap.
    fn gated_loader(
        threads: usize,
        cache_bytes: u64,
    ) -> (Loader, mpsc::Receiver<Start>, mpsc::Receiver<Loaded>) {
        let (started, starts) = mpsc::channel();
        let count = AtomicU32::new(0);
        let decode = move |path: &Path, cap: u64| {
            let width = count.fetch_add(1, Ordering::SeqCst) + 1;
            let (release, wait) = mpsc::channel();
            started.send((path.to_owned(), cap, release)).unwrap();
            wait.recv().unwrap();
            if u64::from(width) * 4 > cap {
                return Err(DecodeError::TooLarge {
                    size: Some((width, 1)),
                });
            }
            Ok(Image {
                width,
                height: 1,
                rgba: vec![0; width as usize * 4],
                may_animate: false,
            })
        };
        let (sender, answers) = mpsc::channel();
        let deliver = move |loaded: Loaded| {
            let _ = sender.send(loaded);
        };
        let loader = Loader::with_decoder(
            threads,
            PLENTY,
            cache_bytes,
            Box::new(decode),
            Box::new(deliver),
        );
        (loader, starts, answers)
    }

    fn next_start(starts: &mpsc::Receiver<Start>) -> Start {
        starts
            .recv_timeout(Duration::from_secs(10))
            .expect("a decode starts")
    }

    // the next answer's request and its image's width
    fn next_answer(answers: &mpsc::Receiver<Loaded>) -> (u64, u32) {
        let loaded = answers
            .recv_timeout(Duration::from_secs(10))
            .expect("an answer");
        let pyramid = loaded.pyramid.expect("an image");
        (loaded.request, pyramid.levels[0].width)
    }

    fn pyramid(width: u32, height: u32) -> Arc<Pyramid> {
        Arc::new(Pyramid::new(Image {
            width,
            height,
            rgba: vec![0; (width * height) as usize * 4],
            may_animate: false,
        }))
    }

    #[test]
    fn the_cache_counts_an_image_with_its_levels() {
        let stamp = Stamp((0, 0, 0, 0, 0, 0, 0));
        let mut cache = Cache {
            budget: PLENTY,
            ..Cache::default()
        };
        // 2x2 and 1x1; 1x3, 1x2 and 1x1
        cache.insert("square".into(), stamp, pyramid(2, 2));
        assert_eq!(cache.used, 5 * 4);
        cache.insert("line".into(), stamp, pyramid(1, 3));
        assert_eq!(cache.used, 11 * 4);
        cache.remove(Path::new("square"));
        assert_eq!(cache.used, 6 * 4);
        // a budget for the line's own pixels, but not its levels
        let mut small = Cache {
            budget: 3 * 4,
            ..Cache::default()
        };
        small.insert("line".into(), stamp, pyramid(1, 3));
        assert!(small.entries.is_empty());
    }

    #[test]
    fn the_cap_counts_the_levels_of_any_decoder_s_image() {
        let (dir, [line]) = files("levels", ["line"]);
        // a decoder that counts no levels, as a helper's: a 1x8 line is 32 bytes, and 60 with
        // its levels
        let decode = |_: &Path, _: u64| -> Result<Image, DecodeError> {
            Ok(Image {
                width: 1,
                height: 8,
                rgba: [7, 8, 9, 255].repeat(8),
                may_animate: false,
            })
        };
        for (cap, levels) in [(59, None), (60, Some(4))] {
            let (sender, answers) = mpsc::channel();
            let deliver = move |loaded: Loaded| {
                let _ = sender.send(loaded);
            };
            let loader = Loader::with_decoder(2, cap, PLENTY, decode, deliver);
            loader.load(1, line.clone());
            let loaded = answers
                .recv_timeout(Duration::from_secs(10))
                .expect("an answer");
            match (loaded.pyramid, levels) {
                (Ok(pyramid), Some(levels)) => {
                    assert_eq!(pyramid.levels.len(), levels);
                    assert!(pyramid.opaque);
                    assert_eq!(pyramid.levels[3].rgba, [7, 8, 9, 255]);
                }
                (Err(e), None) => assert_eq!(e, DecodeError::TooLarge { size: Some((1, 8)) }),
                (answer, _) => panic!("cap {cap}: {answer:?}"),
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_decoder_that_gives_too_few_pixels_fails_its_file_and_the_workers_go_on() {
        let (dir, [short, good]) = files("short", ["short", "good"]);
        // 2x2 needs 16 bytes; building levels from 4 would run off their end
        let decode = |path: &Path, _: u64| -> Result<Image, DecodeError> {
            let width = if path.ends_with("short") { 2 } else { 1 };
            Ok(Image {
                width,
                height: width,
                rgba: vec![0; 4],
                may_animate: false,
            })
        };
        let (sender, answers) = mpsc::channel();
        let deliver = move |loaded: Loaded| {
            let _ = sender.send(loaded);
        };
        let loader = Loader::with_decoder(2, PLENTY, PLENTY, decode, deliver);
        // two workers: were the first two requests to end theirs, none would answer the last
        for request in [1, 2] {
            loader.load(request, short.clone());
            let loaded = answers
                .recv_timeout(Duration::from_secs(10))
                .expect("an answer");
            assert_eq!(loaded.request, request);
            assert_eq!(
                loaded.pyramid.unwrap_err(),
                DecodeError::Unreadable("the decoder gave a wrong number of pixels".to_owned())
            );
        }
        loader.load(3, good);
        assert_eq!(next_answer(&answers), (3, 1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_cache_drops_the_least_recently_used_but_never_the_pinned_file() {
        let image = |width: u32| pyramid(width, 1);
        let stamp = Stamp((0, 0, 0, 0, 0, 0, 0));
        let names = |cache: &Cache| {
            cache
                .entries
                .iter()
                .map(|e| e.path.display().to_string())
                .collect::<Vec<_>>()
        };
        let mut cache = Cache {
            budget: 12,
            pinned: Some("a".into()),
            ..Cache::default()
        };
        for name in ["a", "b", "c"] {
            cache.insert(name.into(), stamp, image(1));
        }
        assert!(cache.get(Path::new("b"), stamp).is_some());
        cache.insert("d".into(), stamp, image(1));
        assert_eq!(
            names(&cache),
            ["a", "b", "d"],
            "c, used least recently, made room, and the pinned a stayed"
        );
        cache.insert("e".into(), stamp, image(3));
        assert_eq!(
            names(&cache),
            ["a", "b", "d"],
            "an image that cannot fit beside the pinned one drops nothing"
        );
        let changed = Stamp((1, 0, 0, 0, 0, 0, 0));
        assert!(
            cache.get(Path::new("b"), changed).is_none(),
            "a changed file misses"
        );
        assert_eq!(names(&cache), ["a", "b", "d"], "and the miss drops nothing");
        cache.insert("b".into(), changed, image(1));
        assert_eq!(
            (names(&cache), cache.used),
            (["a", "d", "b"].map(String::from).to_vec(), 12),
            "the new pixels replace the old"
        );
    }

    #[test]
    fn revisits_come_from_the_cache_until_the_file_changes() {
        let (dir, [a, b]) = files("revisits", ["a.png", "b.png"]);
        let png = |path: &Path, width: u32| {
            image::RgbaImage::from_fn(width, 10, |x, _| image::Rgba([x as u8, 0, 0, 255]))
                .save(path)
                .unwrap();
        };
        png(&a, 30);
        png(&b, 40);
        let (sender, answers) = mpsc::channel();
        let loader = Loader::new(2, PLENTY, PLENTY, move |loaded| {
            let _ = sender.send(loaded);
        });
        let get = |request, path: &Path| {
            loader.load(request, path.to_owned());
            let loaded = answers
                .recv_timeout(Duration::from_secs(10))
                .expect("an answer");
            assert_eq!(loaded.request, request);
            loaded.pyramid
        };
        let first = get(1, &a).unwrap();
        let sizes: Vec<_> = first.levels.iter().map(|l| (l.width, l.height)).collect();
        assert_eq!(
            (sizes, first.opaque),
            (
                vec![(30, 10), (15, 5), (8, 3), (4, 2), (2, 1), (1, 1)],
                true
            ),
            "a decode comes with its levels"
        );
        assert_eq!(get(2, &b).unwrap().levels[0].width, 40);
        assert!(
            Arc::ptr_eq(&first, &get(3, &a).unwrap()),
            "a revisit takes an unchanged file's decoded image from the cache"
        );
        png(&a, 50);
        let changed = get(4, &a).unwrap();
        assert_eq!(
            changed.levels[0].width, 50,
            "a revisit reads a file that changed"
        );
        loader.invalidate(std::slice::from_ref(&a));
        assert!(
            !Arc::ptr_eq(&changed, &get(5, &a).unwrap()),
            "an invalidated file is read again"
        );
        assert!(matches!(
            get(6, &dir.join("gone.png")),
            Err(DecodeError::Unreadable(_))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_decoded_ahead_are_told_once_in_with_their_stamps_and_requests_are_not() {
        let (dir, [a, b]) = files("ahead", ["a.png", "b.png"]);
        let missing = dir.join("missing.png");
        for (path, width) in [(&a, 30), (&b, 40)] {
            image::RgbaImage::new(width, 10).save(path).unwrap();
        }
        let (sender, answers) = mpsc::channel();
        let loader = Arc::new(Loader::new(2, PLENTY, PLENTY, move |loaded| {
            let _ = sender.send(loaded);
        }));
        let (told, news) = mpsc::channel();
        let again = Arc::downgrade(&loader);
        loader.tell_ahead(move |path, pyramid, stamp| {
            let _ = told.send((path.to_owned(), pyramid.levels[0].width, stamp));
            // a callback may set the next one, as the lock is not held while it runs
            let told = told.clone();
            if let Some(loader) = again.upgrade() {
                loader.tell_ahead(move |path, pyramid, stamp| {
                    let _ = told.send((path.to_owned(), pyramid.levels[0].width, stamp));
                });
            }
        });
        let next = || news.recv_timeout(Duration::from_secs(10)).expect("news");
        loader.prefetch(vec![missing.clone(), a.clone()]);
        let (path, width, decoded) = next();
        assert_eq!(
            (path, width),
            (a.clone(), 30),
            "decoded, and a file that fails is not told"
        );
        loader.prefetch(vec![a.clone()]);
        let (path, _, cached) = next();
        assert_eq!(
            (path, cached),
            (a.clone(), decoded),
            "found in the cache, same stamp"
        );
        loader.load(1, a.clone());
        let loaded = answers.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            loaded.stamp,
            Some(decoded),
            "an answer carries the stamp too"
        );
        loader.load(2, b);
        let loaded = answers.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(loaded.pyramid.unwrap().levels[0].width, 40);
        assert!(loaded.stamp.is_some_and(|stamp| stamp != decoded));
        assert!(
            news.recv_timeout(Duration::from_millis(200)).is_err(),
            "a request is answered, not told"
        );
        loader.load(3, missing);
        let loaded = answers.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(loaded.stamp, None, "a file that cannot be read has none");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_decoded_ahead_that_the_cache_has_no_room_for_is_not_told() {
        let (dir, [shown, big, tiny]) = files("no-room", ["shown.png", "big.png", "tiny.png"]);
        for (path, side) in [(&shown, 20), (&big, 16), (&tiny, 1)] {
            image::RgbaImage::new(side, side).save(path).unwrap();
        }
        // room for either of the first two alone, and for the tiny one beside the shown one
        let budget = memory_needed(20, 20) + memory_needed(16, 16) - 1;
        let (sender, answers) = mpsc::channel();
        let loader = Loader::new(2, PLENTY, budget, move |loaded| {
            let _ = sender.send(loaded);
        });
        let (told, news) = mpsc::channel();
        loader.tell_ahead(move |path, _, _| {
            let _ = told.send(path.to_owned());
        });
        loader.load(1, shown);
        answers.recv_timeout(Duration::from_secs(10)).unwrap();
        // with two workers, one kept free, files ahead decode one at a time, in order
        loader.prefetch(vec![big, tiny.clone()]);
        assert_eq!(
            news.recv_timeout(Duration::from_secs(10)).unwrap(),
            tiny,
            "the big one, decoded first, found no room beside the shown one"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_request_joins_the_decode_of_its_file_under_way() {
        let (dir, [p, q]) = files("joins", ["p", "q"]);
        // no cache, and both workers busy when the viewer comes back to p: p's decode under way
        // must answer, as nothing would keep its pixels for a second look
        let (loader, starts, answers) = gated_loader(2, 0);
        loader.load(1, p.clone());
        let (_, _, first) = next_start(&starts);
        loader.load(2, q);
        let (_, _, second) = next_start(&starts);
        loader.load(3, p.clone());
        first.send(()).unwrap();
        assert_eq!(next_answer(&answers), (3, 1), "no second decode of p");
        second.send(()).unwrap();

        // nothing fits a cache of 0 bytes, so decoding ahead refuses every file
        let (loader, starts, answers) = gated_loader(2, 0);
        loader.prefetch(vec![p.clone()]);
        let (_, cap, first) = next_start(&starts);
        assert_eq!(cap, 0, "a file decoded ahead must fit the cache");
        loader.load(4, p.clone());
        first.send(()).unwrap();
        let (path, cap, second) = next_start(&starts);
        assert_eq!(
            (path, cap),
            (p, PLENTY),
            "the viewer's file, refused ahead, decodes again under the full cap"
        );
        second.send(()).unwrap();
        assert_eq!(next_answer(&answers), (4, 2));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_decoder_that_panics_fails_its_file_and_the_workers_go_on() {
        let (dir, [bad, good]) = files("panics", ["bad", "good"]);
        let decode = |path: &Path, _: u64| -> Result<Image, DecodeError> {
            assert!(!path.ends_with("bad"), "a decoder bug");
            Ok(Image {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
                may_animate: false,
            })
        };
        let (sender, answers) = mpsc::channel();
        let deliver = move |loaded: Loaded| {
            let _ = sender.send(loaded);
        };
        let loader = Loader::with_decoder(2, PLENTY, PLENTY, Box::new(decode), Box::new(deliver));
        // two workers: were a panic to end one, two panics would leave none for the last request
        for request in [1, 2] {
            loader.load(request, bad.clone());
            let loaded = answers
                .recv_timeout(Duration::from_secs(10))
                .expect("an answer");
            assert_eq!(loaded.request, request);
            assert!(matches!(loaded.pyramid, Err(DecodeError::Unreadable(_))));
        }
        loader.load(3, good);
        assert_eq!(next_answer(&answers), (3, 1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_invalidated_decode_is_neither_joined_nor_cached() {
        let (dir, [p, q]) = files("invalidated", ["p", "q"]);
        let (loader, starts, answers) = gated_loader(2, PLENTY);
        loader.prefetch(vec![p.clone()]);
        let (_, _, stale) = next_start(&starts);
        loader.invalidate(std::slice::from_ref(&p));
        loader.load(1, p.clone());
        let (path, _, fresh) = next_start(&starts);
        assert_eq!(path, p, "the request decodes the file again");
        fresh.send(()).unwrap();
        assert_eq!(next_answer(&answers), (1, 2));
        stale.send(()).unwrap();
        // with two workers, one kept free, a file ahead starts only once no decode runs: here,
        // once the stale one has ended
        loader.prefetch(vec![q]);
        let (_, _, last) = next_start(&starts);
        loader.load(2, p);
        assert_eq!(
            next_answer(&answers),
            (2, 2),
            "the stale pixels never replaced the new ones in the cache"
        );
        last.send(()).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_latest_request_gets_an_answer() {
        let (dir, [p, q, r]) = files("latest", ["p", "q", "r"]);
        let (loader, starts, answers) = gated_loader(2, PLENTY);
        loader.load(1, p.clone());
        let (_, _, first) = next_start(&starts);
        loader.load(2, q);
        let (_, _, second) = next_start(&starts);
        second.send(()).unwrap();
        assert_eq!(next_answer(&answers), (2, 2));
        first.send(()).unwrap();
        // r starts once p's decode has ended, and whatever p's worker delivered comes before the
        // next answer: that worker either took r or is the one left for the next request
        loader.prefetch(vec![r]);
        let (_, _, last) = next_start(&starts);
        loader.load(3, p);
        assert_eq!(
            next_answer(&answers),
            (3, 1),
            "a request the viewer moved past gets no answer, though its decode filled the cache"
        );
        last.send(()).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
