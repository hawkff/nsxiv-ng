//! The files the command line names: a file as given, and the images inside a
//! directory, sorted the way a file manager lists them.

use std::cmp::Ordering;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub marked: bool,
}

impl FileEntry {
    pub fn new(path: PathBuf) -> Self {
        FileEntry {
            path,
            marked: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileList {
    pub files: Vec<FileEntry>,
    /// Arguments that name nothing, as given, for the caller to warn about.
    pub missing: Vec<PathBuf>,
}

impl FileList {
    /// Paths become absolute, with `.` and `..` resolved by name. A named path comes as given,
    /// whatever its extension or file type: the user chose it, and `<(curl ...)` names a pipe.
    /// A directory contributes only its regular files whose extension is in `extensions`, all
    /// levels down when `recursive`, sorted by [`natural_cmp`]. Symlinks to images count;
    /// symlinked directories are not entered, and hidden files and directories stay out, on
    /// macOS also those hidden by a flag, as Finder hides them. A FIFO in a scanned directory
    /// stays out too, since opening it would block.
    pub fn build(paths: &[PathBuf], recursive: bool, extensions: &[&str]) -> FileList {
        let mut list = FileList::default();
        for given in paths {
            let path = standardize(given);
            match fs::metadata(&path) {
                Err(_) => list.missing.push(given.clone()),
                Ok(meta) if meta.is_dir() => {
                    let mut found = scan(&path, recursive, extensions);
                    // names that only differ in bytes that are not UTF-8 fall back to the bytes
                    found.sort_by(|a, b| {
                        natural_cmp(&a.to_string_lossy(), &b.to_string_lossy())
                            .then_with(|| a.cmp(b))
                    });
                    list.files.extend(found.into_iter().map(FileEntry::new));
                }
                Ok(_) => list.files.push(FileEntry::new(path)),
            }
        }
        list
    }
}

/// Absolute, with `.` dropped and `..` taking off the component before it, without asking
/// the file system, as Foundation's standardized URLs do.
fn standardize(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

// a worklist, not recursion: each directory closes before the next opens, so a deep tree
// holds one descriptor, not one per level
fn scan(root: &Path, recursive: bool, extensions: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name().as_encoded_bytes().starts_with(b".") || hidden_by_flag(&entry) {
                continue;
            }
            let path = entry.path();
            // the entry's own type: a symlink to a directory is no directory here
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if recursive {
                    dirs.push(path);
                }
            } else if has_extension(&path, extensions)
                && fs::metadata(&path).is_ok_and(|m| m.is_file())
            {
                found.push(path);
            }
        }
    }
    found
}

// Whether macOS hides the entry itself, a link not followed, by its UF_HIDDEN flag, as Finder
// leaves it out.
#[cfg(target_os = "macos")]
fn hidden_by_flag(entry: &fs::DirEntry) -> bool {
    use std::os::macos::fs::MetadataExt;
    const UF_HIDDEN: u32 = 0x8000;
    entry
        .metadata()
        .is_ok_and(|meta| meta.st_flags() & UF_HIDDEN != 0)
}

#[cfg(not(target_os = "macos"))]
fn hidden_by_flag(_: &fs::DirEntry) -> bool {
    false
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Orders names the way a file manager does: letters without regard to case, and runs of
/// digits by their value, so img2 comes before img10. Names equal by that measure fall back
/// to their bytes, so the order is total.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    // byte offsets into a and b; digits are ASCII, so a digit run ends on a char boundary
    let (mut i, mut j) = (0, 0);
    loop {
        let order = match (a[i..].chars().next(), b[j..].chars().next()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let (m_end, n_end) = (i + digit_run(&a[i..]), j + digit_run(&b[j..]));
                let m = a[i..m_end].trim_start_matches('0');
                let n = b[j..n_end].trim_start_matches('0');
                (i, j) = (m_end, n_end);
                m.len().cmp(&n.len()).then_with(|| m.cmp(n))
            }
            (Some(c), Some(d)) => {
                (i, j) = (i + c.len_utf8(), j + d.len_utf8());
                c.to_lowercase().cmp(d.to_lowercase())
            }
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

fn digit_run(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_sort_like_a_file_manager() {
        let mut names = [
            "img10.png",
            "img2.png",
            "Img1.png",
            "img02.png",
            "b.png",
            "A.png",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            [
                "A.png",
                "b.png",
                "Img1.png",
                "img02.png",
                "img2.png",
                "img10.png"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn scans_keep_visible_images_and_links_to_them() {
        let dir = temp_dir("scan");
        fs::create_dir_all(dir.join("nested/.git")).unwrap();
        for file in [
            "frame.png",
            "nested/frame.png",
            ".hidden.png",
            "notes.txt",
            "nested/.git/x.png",
        ] {
            fs::write(dir.join(file), b"only scanned, never decoded").unwrap();
        }
        std::os::unix::fs::symlink(dir.join("frame.png"), dir.join("link.png")).unwrap();
        std::os::unix::fs::symlink(dir.join("nested"), dir.join("linked")).unwrap();
        fs::create_dir(dir.join("album.png")).unwrap();
        // a socket stands in for a FIFO: neither is a regular file
        let _socket = std::os::unix::net::UnixListener::bind(dir.join("socket.png")).unwrap();
        let names = |recursive| -> Vec<String> {
            let list = FileList::build(std::slice::from_ref(&dir), recursive, &["png"]);
            list.files
                .iter()
                .map(|f| f.path.strip_prefix(&dir).unwrap().display().to_string())
                .collect()
        };
        assert_eq!(names(false), ["frame.png", "link.png"]);
        assert_eq!(names(true), ["frame.png", "link.png", "nested/frame.png"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_scan_leaves_out_what_macos_hides_by_a_flag_and_a_named_file_comes_all_the_same() {
        let dir = temp_dir("flags");
        fs::create_dir_all(dir.join("shown")).unwrap();
        fs::create_dir_all(dir.join("folder")).unwrap();
        for file in ["a.png", "b.png", "shown/c.png", "folder/d.png"] {
            fs::write(dir.join(file), b"only scanned, never decoded").unwrap();
        }
        // a visible link to the hidden file: its own flag counts, not its target's
        std::os::unix::fs::symlink(dir.join("b.png"), dir.join("link.png")).unwrap();
        for hidden in ["b.png", "folder"] {
            let status = std::process::Command::new("chflags")
                .arg("hidden")
                .arg(dir.join(hidden))
                .status()
                .unwrap();
            assert!(status.success());
        }
        let list = FileList::build(std::slice::from_ref(&dir), true, &["png"]);
        let names: Vec<String> = (list.files.iter())
            .map(|f| f.path.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["a.png", "link.png", "shown/c.png"]);
        let named = FileList::build(&[dir.join("b.png")], false, &["png"]);
        assert_eq!(named.files.len(), 1, "named, it comes whatever its flag");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn named_files_come_as_given_and_missing_ones_are_reported() {
        let dir = temp_dir("named");
        fs::write(dir.join("notes.txt"), b"").unwrap();
        let list = FileList::build(
            &[dir.join("sub/../notes.txt"), dir.join("gone.png")],
            false,
            &["png"],
        );
        assert_eq!(
            list.files,
            [FileEntry::new(dir.join("notes.txt"))],
            "any extension, with .. resolved by name"
        );
        assert_eq!(list.missing, [dir.join("gone.png")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_named_file_that_is_not_regular_comes_as_given() {
        let dir = temp_dir("pipe");
        // a socket stands in for the pipe that <(curl ...) names
        let _socket = std::os::unix::net::UnixListener::bind(dir.join("pipe.png")).unwrap();
        let list = FileList::build(&[dir.join("pipe.png")], false, &["png"]);
        assert_eq!(list.files, [FileEntry::new(dir.join("pipe.png"))]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn names_that_read_alike_keep_their_byte_order() {
        use std::os::unix::ffi::OsStrExt;
        let dir = temp_dir("bytes");
        // both read as "x\u{fffd}.png" once made UTF-8
        for name in [b"x\x81.png".as_slice(), b"x\x80.png"] {
            match fs::write(dir.join(std::ffi::OsStr::from_bytes(name)), b"") {
                // EILSEQ: a file system that takes UTF-8 names alone, as APFS, has no such files
                Err(e)
                    if e.raw_os_error()
                        == Some(if cfg!(target_os = "macos") { 92 } else { 84 }) =>
                {
                    fs::remove_dir_all(&dir).unwrap();
                    return;
                }
                written => written.unwrap(),
            }
        }
        let list = FileList::build(std::slice::from_ref(&dir), false, &["png"]);
        let names: Vec<&[u8]> = list
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().as_bytes())
            .collect();
        assert_eq!(names, [b"x\x80.png".as_slice(), b"x\x81.png"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
