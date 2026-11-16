//! The edit mode: annotations drawn over the image as shown, kept apart from any system.
//! Geometry is in the image's pixels, origin top left, y down. What only a system can do (a
//! text field, the pasteboard, recognizing text, faces and barcodes) a frontend does when the
//! edit asks for it.
#![forbid(unsafe_code)]

mod annotation;
mod censor;
mod draw;
mod edit;
#[cfg(test)]
mod golden;
mod pii;
mod recognize;
#[cfg(test)]
mod testing;

use std::path::{Path, PathBuf};

use nsxiv_ng_render::text::tiny_skia::Pixmap;

pub use annotation::{Annotation, CensorMode, EMOJIS, FillMode, PALETTE, Rgb, Tool};
pub use edit::{Edit, EditError, Response};
pub use pii::{Word, find_pii};
pub use recognize::{Recognition, Recognize, Recognizer, recognize};

/// An image as straight RGBA, rows top to bottom: what an edit flattens to, and what a
/// recognizer reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl RgbaImage {
    pub(crate) fn from_pixmap(pixmap: Pixmap) -> RgbaImage {
        RgbaImage {
            width: pixmap.width(),
            height: pixmap.height(),
            rgba: pixmap.take_demultiplied(),
        }
    }
}

/// Where an edit of `source` is saved: `<stem>-edit.png` beside it, else the first of
/// `<stem>-edit-1.png`, `<stem>-edit-2.png` and on that nothing holds now, a dangling link
/// included; the stem as it is, whatever its bytes. A name free now may be taken before the
/// file is written: whoever saves creates the file new, and asks again where that fails.
pub fn save_path(source: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default();
    let folder = source.parent().unwrap_or(Path::new(""));
    let named = |suffix: &str| {
        let mut name = stem.to_os_string();
        name.push(suffix);
        folder.join(name)
    };
    let taken = |path: &Path| path.symlink_metadata().is_ok();
    let mut path = named("-edit.png");
    let mut n = 0;
    while taken(&path) {
        n += 1;
        path = named(&format!("-edit-{n}.png"));
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own under the system's temporary one, empty.
    fn folder(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nsxiv-ng-edit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn an_edit_is_saved_beside_its_source_under_a_name_not_taken() {
        let dir = folder("save");
        let source = dir.join("photo.jpg");
        std::fs::write(&source, b"").unwrap();
        assert_eq!(save_path(&source), dir.join("photo-edit.png"));
        // one edit there already
        std::fs::write(dir.join("photo-edit.png"), b"").unwrap();
        assert_eq!(save_path(&source), dir.join("photo-edit-1.png"));
        // three, the third a link to nothing
        std::fs::write(dir.join("photo-edit-1.png"), b"").unwrap();
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("photo-edit-2.png")).unwrap();
        assert_eq!(save_path(&source), dir.join("photo-edit-3.png"));
        // only the last extension goes
        assert_eq!(
            save_path(&dir.join("scan.tar.png")),
            dir.join("scan.tar-edit.png")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Linux keeps a name's bytes as they are, which need not be UTF-8.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_edit_keeps_its_sources_stem_whatever_its_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let source = Path::new("/pics").join(OsStr::from_bytes(b"caf\xe9.jpg"));
        let saved = save_path(&source);
        assert_eq!(saved.file_name().unwrap().as_bytes(), b"caf\xe9-edit.png");
    }
}
