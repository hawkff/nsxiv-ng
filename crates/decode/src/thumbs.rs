//! The freedesktop thumbnail cache, which file managers share: PNG files under
//! `$XDG_CACHE_HOME/thumbnails`, named by the MD5 of the file's URI, which their text chunks
//! repeat with the file's modification time.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::BufReader;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use image::RgbaImage;

use crate::{DecodeError, Image};

/// The cache's folders, by the side of the square their thumbnails fit in.
const FOLDERS: [(&str, u32); 4] = [
    ("normal", 128),
    ("large", 256),
    ("x-large", 512),
    ("xx-large", 1024),
];

/// The thumbnail cache, read always, and written unless the viewer is private (`-p`).
pub struct Thumbnails {
    root: Option<PathBuf>,
    write: bool,
    /// The entries ever forgotten, by name. The lock also makes forgetting one step against
    /// placing a new thumbnail, and is held for those file operations alone.
    forgotten: Mutex<HashMap<String, Forgotten>>,
}

#[derive(Clone, Copy, Default)]
struct Forgotten {
    /// How often the entry was forgotten, which never goes back, so a thumbnail begun before
    /// any of those times never places.
    count: u64,
    /// No thumbnail was placed since the last time. Its cache file may then be stale, where an
    /// edit kept the size and the modification time to the second, so it is not read.
    stale: bool,
}

// What names and checks a file's thumbnail.
struct Entry {
    uri: String,
    name: String,
    mtime: String,
    size: u64,
}

impl Thumbnails {
    /// The user's cache, in `$XDG_CACHE_HOME` or else `~/.cache`.
    pub fn new(private: bool) -> Self {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|cache| cache.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")));
        Self::at(cache.map(|cache| cache.join("thumbnails")), !private)
    }

    fn at(root: Option<PathBuf>, write: bool) -> Self {
        Self {
            root,
            write,
            forgotten: Mutex::default(),
        }
    }

    /// A thumbnail of the file at `path` that fits a square of the cache's smallest size that
    /// covers `side` pixels, never enlarged. Pass the largest cell the grid can draw, so zooming
    /// finds the same thumbnail. One in the cache serves while its URI and modification time
    /// match the file and it was not forgotten since it was written; otherwise `decode` decodes
    /// the file, given the side the thumbnail must cover, and the new thumbnail goes into the
    /// cache, unless the file was forgotten while it was made.
    pub fn get(
        &self,
        path: &Path,
        side: u32,
        decode: impl FnOnce(u32) -> Result<Image, DecodeError>,
    ) -> Result<Image, DecodeError> {
        let first = FOLDERS
            .iter()
            .position(|&(_, fits)| fits >= side)
            .unwrap_or(FOLDERS.len() - 1);
        let (folder, fits) = FOLDERS[first];
        let found = self.root.as_deref().zip(entry(path));
        let forgotten = |entry: &Entry| self.forgotten.lock().unwrap().get(&entry.name).copied();
        let began = found
            .as_ref()
            .and_then(|(_, entry)| forgotten(entry))
            .unwrap_or_default();
        if let Some((root, entry)) = &found
            && !began.stale
        {
            // a larger thumbnail, as another program may have written, serves too
            for &(larger, larger_fits) in &FOLDERS[first..] {
                if let Some(image) = read(&root.join(larger).join(&entry.name), entry, larger_fits)
                {
                    return Ok(fit(image, fits));
                }
            }
        }
        let image = fit(decode(fits)?, fits);
        // never thumbnails of the cache's own files, however the path reaches them
        let real = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some((root, entry)) = found
            && self.write
            && !real(path).starts_with(real(root))
        {
            // a thumbnail that cannot be written is decoded again next time
            let _ = write(&root.join(folder), &entry, &image, |partial, placed| {
                let mut forgotten = self.forgotten.lock().unwrap();
                let now = forgotten.get(&entry.name).copied().unwrap_or_default();
                // forgotten since this began, it may show the file as it was
                if now.count != began.count {
                    return Err(std::io::Error::other("forgotten while made"));
                }
                fs::rename(partial, placed)?;
                if let Some(forgotten) = forgotten.get_mut(&entry.name) {
                    forgotten.stale = false;
                }
                Ok(())
            });
        }
        Ok(image)
    }

    /// Deletes the file's thumbnails at every size, so the next [`get`](Self::get) decodes it
    /// again, even when the file is gone. A private viewer deletes nothing, and decodes the
    /// file each time from then on, as it writes no thumbnail that would be new.
    pub fn forget(&self, path: &Path) {
        let Some(uri) = uri(path) else {
            return;
        };
        let name = name(&uri);
        let mut forgotten = self.forgotten.lock().unwrap();
        let entry = forgotten.entry(name.clone()).or_default();
        entry.count += 1;
        entry.stale = true;
        if let (Some(root), true) = (&self.root, self.write) {
            for (folder, _) in FOLDERS {
                let _ = fs::remove_file(root.join(folder).join(&name));
            }
        }
    }
}

// The thumbnail's file name: the MD5 of the URI.
fn name(uri: &str) -> String {
    format!("{:x}.png", md5::compute(uri.as_bytes()))
}

// Opens a regular file to read; anything else is refused, a FIFO without waiting for a writer.
fn open_regular(path: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

// A file the user cannot read gets no thumbnail, not even one cached before it turned
// unreadable, as the spec asks.
fn entry(path: &Path) -> Option<Entry> {
    let metadata = open_regular(path)?.metadata().ok()?;
    let uri = uri(path)?;
    Some(Entry {
        name: name(&uri),
        uri,
        mtime: metadata.mtime().to_string(),
        size: metadata.len(),
    })
}

// The file's URI as GLib writes it, so the thumbnail's name is the one file managers use: the
// absolute path with `.` and `..` resolved by name, not through symlinks, and every byte but a
// letter, a digit and one of !$&'()*+,-./:=@_~ escaped.
fn uri(path: &Path) -> Option<String> {
    let absolute = std::path::absolute(path).ok()?;
    let mut parts = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    let mut uri = String::from("file://");
    for part in parts {
        uri.push('/');
        for &byte in part.as_bytes() {
            if byte.is_ascii_alphanumeric() || b"!$&'()*+,-.:=@_~".contains(&byte) {
                uri.push(char::from(byte));
            } else {
                let _ = write!(uri, "%{byte:02X}");
            }
        }
    }
    Some(uri)
}

// The thumbnail in `file` when its text chunks name this file as it is now, and it fits the
// folder's square.
fn read(file: &Path, entry: &Entry, fits: u32) -> Option<Image> {
    // another program may have planted anything here
    let file = open_regular(file)?;
    // a thumbnail that fits its folder needs far less; anything else is no thumbnail
    let limits = png::Limits { bytes: 16 << 20 };
    let mut decoder = png::Decoder::new_with_limits(BufReader::new(file), limits);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    if reader.info().width.max(reader.info().height) > fits {
        return None;
    }
    let mut pixels = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut pixels).ok()?;
    // text chunks may follow the pixels; a fault past them leaves only those before
    let _ = reader.finish();
    let info = reader.info();
    // compressed text inflates only as far as a URI can run
    let text = |key: &str| -> Option<String> {
        const LIMIT: usize = 64 << 10;
        if let Some(chunk) = info
            .uncompressed_latin1_text
            .iter()
            .find(|c| c.keyword == key)
        {
            return Some(chunk.text.clone());
        }
        if let Some(chunk) = info
            .compressed_latin1_text
            .iter()
            .find(|c| c.keyword == key)
        {
            let mut chunk = chunk.clone();
            chunk.decompress_text_with_limit(LIMIT).ok()?;
            return chunk.get_text().ok();
        }
        let mut chunk = info.utf8_text.iter().find(|c| c.keyword == key)?.clone();
        chunk.decompress_text_with_limit(LIMIT).ok()?;
        chunk.get_text().ok()
    };
    // the size, where the thumbnail has it, tells apart files changed within one second
    if text("Thumb::URI")? != entry.uri
        || text("Thumb::MTime")? != entry.mtime
        || text("Thumb::Size").is_some_and(|size| size != entry.size.to_string())
    {
        return None;
    }
    pixels.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
        png::ColorType::Rgba => pixels,
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[l, a]| [l, l, l, a])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&l| [l, l, l, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some(Image {
        width: frame.width,
        height: frame.height,
        rgba,
        may_animate: false,
    })
}

// Writes the thumbnail under a name of its own, then has `place` rename it into place, so no
// reader sees half a file.
fn write(
    folder: &Path,
    entry: &Entry,
    image: &Image,
    place: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)?;
    let partial = folder.join(format!(
        ".{}.{}.{}",
        entry.name,
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)?;
    let written = (|| -> Result<(), png::EncodingError> {
        let mut encoder = png::Encoder::new(file, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        for (key, text) in [
            ("Thumb::URI", entry.uri.clone()),
            ("Thumb::MTime", entry.mtime.clone()),
            ("Thumb::Size", entry.size.to_string()),
            ("Software", "nsxiv-ng".to_owned()),
        ] {
            encoder.add_text_chunk(key.to_owned(), text)?;
        }
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&image.rgba)?;
        writer.finish()
    })();
    let placed = written
        .map_err(std::io::Error::other)
        .and_then(|()| place(&partial, &folder.join(&entry.name)));
    if placed.is_err() {
        let _ = fs::remove_file(&partial);
    }
    placed
}

// Scales the image down to fit a square of `side`, averaging premultiplied pixels so that
// transparent ones lend no color to the edges they border.
fn fit(image: Image, side: u32) -> Image {
    let (width, height) = (image.width, image.height);
    let longest = width.max(height);
    if longest <= side {
        return image;
    }
    let scaled = |n: u32| (u64::from(n) * u64::from(side) / u64::from(longest)).max(1) as u32;
    let (new_width, new_height) = (scaled(width), scaled(height));
    let mut rgba = image.rgba;
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
    let full = RgbaImage::from_raw(width, height, rgba).expect("an Image holds its pixels");
    let mut rgba = image::imageops::thumbnail(&full, new_width, new_height).into_raw();
    crate::unpremultiply(&mut rgba);
    Image {
        width: new_width,
        height: new_height,
        rgba,
        may_animate: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nsxiv-ng-thumbs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn red(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            rgba: [255, 0, 0, 255].repeat((width * height) as usize),
            may_animate: false,
        }
    }

    #[test]
    fn names_follow_the_spec_and_glib() {
        // the spec's own example
        let entry = format!("{:x}", md5::compute("file:///home/jens/photos/me.png"));
        assert_eq!(entry, "c6ee772d9e49320e97ec29a7eb5b1697");
        assert_eq!(
            uri(Path::new("/home/a b/./x/../ü#!.png")).unwrap(),
            "file:///home/a%20b/%C3%BC%23!.png"
        );
    }

    #[test]
    fn a_thumbnail_serves_until_its_file_changes() {
        let dir = scratch("cache");
        let file = dir.join("photo.png");
        fs::write(&file, b"pixels").unwrap();
        let cache = Thumbnails::at(Some(dir.join("thumbnails")), true);
        // a 160 cell takes the large folder's 256
        let first = cache.get(&file, 160, |side| {
            assert_eq!(side, 256);
            Ok(red(600, 300))
        });
        assert_eq!(first.map(|i| (i.width, i.height)), Ok((256, 128)));
        let written = dir
            .join("thumbnails/large")
            .join(entry(&file).unwrap().name);
        assert!(written.is_file());
        // from the cache, as it is, and scaled down for a smaller cell's folder
        let cached = cache.get(&file, 256, |_| panic!("decoded again")).unwrap();
        assert_eq!(
            (cached.width, cached.height, &cached.rgba[..4]),
            (256, 128, &[255, 0, 0, 255][..])
        );
        let smaller = cache.get(&file, 100, |_| panic!("decoded again"));
        assert_eq!(smaller.map(|i| (i.width, i.height)), Ok((128, 64)));
        // new content within the same second shows in the size
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, b"other pixels").unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let mut decoded = false;
        let _ = cache.get(&file, 160, |_| {
            decoded = true;
            Ok(red(600, 300))
        });
        assert!(decoded);
        // a new modification time decodes again
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let mut decoded = false;
        let _ = cache.get(&file, 160, |_| {
            decoded = true;
            Ok(red(10, 10))
        });
        assert!(decoded);
        // a file gone keeps no thumbnails
        fs::remove_file(&file).unwrap();
        cache.forget(&file);
        assert!(!written.exists());
        // a private viewer writes nothing
        let private = Thumbnails::at(Some(dir.join("private")), false);
        fs::write(&file, b"pixels").unwrap();
        let _ = private.get(&file, 160, |_| Ok(red(10, 10)));
        assert!(!dir.join("private").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn blue(width: u32, height: u32) -> Image {
        Image {
            rgba: [0, 0, 255, 255].repeat((width * height) as usize),
            ..red(width, height)
        }
    }

    #[test]
    fn a_forgotten_file_is_decoded_again_though_its_entry_still_matches() {
        let dir = scratch("forgotten");
        let file = dir.join("photo.png");
        fs::write(&file, b"pixels").unwrap();
        let root = dir.join("thumbnails");
        let shared = Thumbnails::at(Some(root.clone()), true);
        let _ = shared.get(&file, 160, |_| Ok(red(10, 10)));
        // an edit that keeps the size and the modification time to the second
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, b"PIXELS").unwrap();
        let file_handle = File::options().write(true).open(&file).unwrap();
        file_handle.set_modified(modified).unwrap();
        let private = Thumbnails::at(Some(root), false);
        let stale = private.get(&file, 160, |_| Ok(blue(10, 10))).unwrap();
        assert_eq!(&stale.rgba[..4], [255, 0, 0, 255], "the cache cannot tell");
        private.forget(&file);
        for _ in 0..2 {
            let mut decoded = false;
            let new = private
                .get(&file, 160, |_| {
                    decoded = true;
                    Ok(blue(10, 10))
                })
                .unwrap();
            assert!(
                decoded && new.rgba[..4] == [0, 0, 255, 255],
                "decoded each time"
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_thumbnail_made_while_its_file_is_forgotten_is_not_written() {
        let dir = scratch("forgotten-while-made");
        let file = dir.join("photo.png");
        fs::write(&file, b"pixels").unwrap();
        let cache = Thumbnails::at(Some(dir.join("thumbnails")), true);
        // an invalidation during the decode, which would deadlock were a lock held over it
        let made = cache.get(&file, 160, |_| {
            cache.forget(&file);
            Ok(red(10, 10))
        });
        assert!(made.is_ok());
        let large = dir.join("thumbnails/large");
        let left: Vec<_> = fs::read_dir(&large).unwrap().collect();
        assert!(left.is_empty(), "no entry and no partial file: {left:?}");
        // the next one, begun after the forget, is written, and then serves
        let _ = cache.get(&file, 160, |_| Ok(red(10, 10)));
        assert!(large.join(entry(&file).unwrap().name).is_file());
        let cached = cache.get(&file, 160, |_| panic!("decoded again"));
        assert!(cached.is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_thumbnail_begun_before_a_forget_never_places_whatever_came_between() {
        let dir = scratch("begun-before");
        let file = dir.join("photo.png");
        fs::write(&file, b"pixels").unwrap();
        let cache = Thumbnails::at(Some(dir.join("thumbnails")), true);
        // the outer one begins, its file is forgotten, and a whole newer one places first
        let _ = cache.get(&file, 160, |_| {
            cache.forget(&file);
            let inner = cache.get(&file, 160, |_| Ok(blue(10, 10)));
            assert!(inner.is_ok());
            Ok(red(10, 10))
        });
        let cached = cache.get(&file, 160, |_| panic!("decoded again")).unwrap();
        assert_eq!(&cached.rgba[..4], [0, 0, 255, 255], "the newer pixels stay");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compressed_text_counts_and_inflates_only_so_far() {
        let dir = scratch("compressed");
        let file = dir.join("photo.png");
        fs::write(&file, b"pixels").unwrap();
        let entry = entry(&file).unwrap();
        let cached = dir.join("thumbnails/large").join(&entry.name);
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        let plant = |uri: String| {
            let mut encoder = png::Encoder::new(File::create(&cached).unwrap(), 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder
                .add_ztxt_chunk("Thumb::URI".to_owned(), uri)
                .unwrap();
            let mtime = entry.mtime.clone();
            encoder
                .add_ztxt_chunk("Thumb::MTime".to_owned(), mtime)
                .unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 255, 0, 255]).unwrap();
        };
        let cache = Thumbnails::at(Some(dir.join("thumbnails")), false);
        plant(entry.uri.clone());
        let green = cache.get(&file, 160, |_| panic!("decoded again")).unwrap();
        assert_eq!(green.rgba, [0, 255, 0, 255]);
        // a megabyte of URI is no URI
        plant("a".repeat(1 << 20));
        let red = cache.get(&file, 160, |_| Ok(red(1, 1))).unwrap();
        assert_eq!(red.rgba, [255, 0, 0, 255]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fifos_neither_hang_nor_serve() {
        let dir = scratch("fifos");
        let fifo = |path: &Path| {
            let made = std::process::Command::new("mkfifo").arg(path).status();
            assert!(made.unwrap().success());
        };
        let cache = Thumbnails::at(Some(dir.join("thumbnails")), true);
        // as the file, and as its thumbnail
        let pipe = dir.join("pipe.png");
        fifo(&pipe);
        assert!(cache.get(&pipe, 160, |_| Ok(red(1, 1))).is_ok());
        let file = dir.join("photo.png");
        fs::write(&file, b"pixels").unwrap();
        let planted = dir
            .join("thumbnails/large")
            .join(entry(&file).unwrap().name);
        fs::create_dir_all(planted.parent().unwrap()).unwrap();
        fifo(&planted);
        assert!(cache.get(&file, 160, |_| Ok(red(1, 1))).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn thumbnails_match_the_reference_thumbnails() {
        // the reference thumbnails, from Imlib2 with anti-aliasing: the photo halved, and the
        // GIF's first frame at its own size; the largest channel error and the count of pixels
        // off by more than 2 that each may have, as measured. The photo's come from its JPEG
        // decoder: Imlib2 turns libjpeg-turbo's fancy chroma upsampling off and repeats each
        // chroma sample over 2x2 pixels, where zune-jpeg filters, as libjpeg-turbo does by
        // default. On libjpeg-turbo's pixels as Imlib2 decodes them, a 2x2 average is within 1
        // of Imlib2's thumbnail everywhere.
        let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
        let cases = [
            ("photo.jpg", "photo-thumbnail.png", 9, 11_424),
            ("moving.gif", "moving-thumbnail.png", 0, 0),
        ];
        for (fixture, golden, largest, most_off) in cases {
            let image = crate::decode(&testdata.join("fixtures").join(fixture), 1 << 30).unwrap();
            let thumbnail = fit(image, 320);
            let golden = image::open(testdata.join("golden").join(golden))
                .unwrap()
                .into_rgb8();
            assert_eq!((thumbnail.width, thumbnail.height), golden.dimensions());
            let errors: Vec<u8> = (thumbnail.rgba.as_chunks::<4>().0.iter())
                .zip(golden.as_raw().as_chunks::<3>().0)
                .map(|(ours, theirs)| (0..3).map(|c| ours[c].abs_diff(theirs[c])).max().unwrap())
                .collect();
            let worst = *errors.iter().max().unwrap();
            let off = errors.iter().filter(|&&e| e > 2).count();
            let share = off as f64 / errors.len() as f64 * 100.0;
            eprintln!(
                "{fixture}: largest error {worst}, {off} pixels ({share:.3}%) off by more than 2"
            );
            assert!(
                worst <= largest && off <= most_off,
                "{fixture}: {worst}, {off}"
            );
        }
    }

    #[test]
    fn fit_halves_the_photo_as_its_pyramid_does() {
        // 640 x 480 fit to 320 and the pyramid's level 1 both average 2 x 2 blocks of the same
        // opaque pixels, and both round the same way, so they agree exactly
        let photo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/photo.jpg");
        let image = crate::decode(&photo, 1 << 30).unwrap();
        let thumbnail = fit(image.clone(), 320);
        let pyramid = crate::Pyramid::new(image);
        let level = &pyramid.levels[1];
        assert_eq!(
            (level.width, level.height),
            (thumbnail.width, thumbnail.height)
        );
        let worst = (thumbnail.rgba.iter().zip(&level.rgba))
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        eprintln!("fit against level 1: largest difference {worst}");
        assert_eq!(worst, 0);
    }

    #[test]
    fn scaling_lends_no_color_from_transparent_pixels() {
        let image = Image {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 0, 0, 0],
            may_animate: false,
        };
        let pixel = fit(image, 1).rgba;
        assert!(
            pixel[0] >= 250 && (126..=129).contains(&pixel[3]),
            "{pixel:?}"
        );
    }
}
