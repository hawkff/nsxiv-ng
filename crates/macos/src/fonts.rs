//! Font files mapped rather than read into memory, where the bytes cannot change under the map:
//! on the system volume, which macOS mounts read-only and as a snapshot (on macOS 27.2 with
//! System Integrity Protection on and off alike).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;

use nsxiv_ng_session::FontBytes;

/// The families annotation text is drawn in, as fontdb names them: San Francisco, the system
/// font (SFNS.ttf, "System Font"), then Helvetica Neue and
/// Helvetica, which every macOS has.
pub(crate) const FAMILIES: [&str; 3] = ["System Font", "Helvetica Neue", "Helvetica"];

/// A map of the font file at `path` where the file system it is on cannot change it: mounted
/// read-only, and a snapshot, as macOS's sealed system volume is, where its own fonts are.
/// Else None, and the file is read into memory as any other.
pub(crate) fn map(path: &Path) -> Option<FontBytes> {
    let file = File::open(path).ok()?;
    if !unchanging(&file) {
        return None;
    }
    // SAFETY: a map is sound while no one changes the file under it. The file was opened
    // first and its own file system checked after: mounted read-only, so no process writes
    // it, and a snapshot, whose files do not change; an update of the system replaces the
    // snapshot whole at a restart rather than changing a file in it
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    Some(Arc::new(map))
}

/// Whether the file system `file` is on is mounted read-only and is a snapshot.
fn unchanging(file: &File) -> bool {
    // SAFETY: a zeroed statfs is a valid one for fstatfs to fill in
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor, and a statfs to fill in, alive for the call
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } != 0 {
        return false;
    }
    let wanted = (libc::MNT_RDONLY | libc::MNT_SNAPSHOT) as u32;
    fs.f_flags & wanted == wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_files_on_a_read_only_snapshot_are_mapped() {
        let emoji = Path::new("/System/Library/Fonts/Apple Color Emoji.ttc");
        let mapped = map(emoji).expect("Apple Color Emoji mapped");
        let bytes = (*mapped).as_ref();
        assert_eq!(bytes.len() as u64, emoji.metadata().unwrap().len());
        // a collection's tag
        assert_eq!(&bytes[..4], b"ttcf");
        let elsewhere = std::env::temp_dir().join(format!("font-{}.ttf", std::process::id()));
        std::fs::write(&elsewhere, b"not a font").unwrap();
        assert!(map(&elsewhere).is_none());
        std::fs::remove_file(elsewhere).unwrap();
    }
}
