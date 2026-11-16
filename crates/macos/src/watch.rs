//! The watch on the file the viewer shows, through kqueue as dispatch's vnode sources give it.

use std::ffi::{CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use dispatch2::{
    DispatchObject, DispatchQueue, DispatchRetained, DispatchSource, dispatch_source_vnode_flags_t,
};

/// The file's own events: written, extended, renamed
/// or deleted, the last two as an atomic save replaces it.
const FILE: dispatch_source_vnode_flags_t = dispatch_source_vnode_flags_t(
    dispatch_source_vnode_flags_t::DISPATCH_VNODE_WRITE.0
        | dispatch_source_vnode_flags_t::DISPATCH_VNODE_EXTEND.0
        | dispatch_source_vnode_flags_t::DISPATCH_VNODE_RENAME.0
        | dispatch_source_vnode_flags_t::DISPATCH_VNODE_DELETE.0,
);

/// The number a folder's events pass on; a file's pass its watch's generation, above it.
const FOLDER: usize = 0;

/// The watch on one file, by its name: the file itself, and its folder, whose entries change
/// as a file is saved over the name, so the name is followed to the file it then names. For a
/// link, the folder of the name it ends at too, as Linux's watch has it: a file made there
/// under that name changes what the link shows.
///
/// Arming is not one step: the name is looked at, and a source made. A source stays for as
/// long as its path names the file its descriptor has open, as Linux's watch keeps its
/// folder's across events, so a save between a look and a source is still told by a source
/// that was there; a folder's events are never stale, only looked into. A source is made anew
/// only where its path names another file or none. After the file's source is made the name
/// is looked at once more, and a file that came to it meanwhile, which no event of that
/// source tells, is a change.
pub(crate) struct Watch {
    path: Option<PathBuf>,
    /// The device and inode the file's source has open, none if nothing.
    file: Option<(u64, u64)>,
    file_source: Option<DispatchRetained<DispatchSource>>,
    /// The folders watched: the path each was watched by, and what its source has open.
    folders: Vec<(PathBuf, Opened)>,
    /// Counts the file's sources made, so an event of one made before goes unheard.
    generation: usize,
}

impl Watch {
    pub(crate) fn new() -> Watch {
        Watch {
            path: None,
            file: None,
            file_source: None,
            folders: Vec::new(),
            generation: FOLDER,
        }
    }

    /// Watches `path` from now on, alone, or nothing. An event calls [`crate::app::heard`] on
    /// the main queue with a number for [`Watch::changed`].
    pub(crate) fn set(&mut self, path: Option<&Path>) {
        self.path = path.map(Path::to_path_buf);
        self.follow_folders();
        self.arm();
    }

    /// Whether the event `heard` passed `event` tells the file changed: the file's own events
    /// of the current watch, and its folders' where the name now names another file or none.
    /// Where it does, the watch moves to the file the name now names.
    pub(crate) fn changed(&mut self, event: usize) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        if event == FOLDER {
            // a link may point into another folder now, or the folder be another, which is
            // watched before the look
            self.follow_folders();
            if identity(&path) == self.file {
                return false;
            }
        } else if event != self.generation {
            return false;
        }
        self.arm();
        true
    }

    // The file's source armed, and a change its look finds told.
    fn arm(&mut self) {
        if self.arm_file() {
            crate::app::heard(self.generation);
        }
    }

    // The folders of what the name names now: those whose sources have them open stay, the
    // others go, and the new ones are made.
    fn follow_folders(&mut self) {
        let wanted: Vec<PathBuf> = match &self.path {
            Some(path) => {
                let folder = folder(path).to_path_buf();
                let own = std::fs::canonicalize(&folder).ok();
                let target = named_folder(path).filter(|target| Some(target) != own.as_ref());
                std::iter::once(folder).chain(target).collect()
            }
            None => Vec::new(),
        };
        self.folders.retain(|(folder, opened)| {
            let keep = wanted.contains(folder) && identity(folder) == Some(opened.id);
            if !keep {
                opened.source.cancel();
            }
            keep
        });
        let write = dispatch_source_vnode_flags_t::DISPATCH_VNODE_WRITE;
        for folder in wanted {
            if !self.folders.iter().any(|(watched, _)| *watched == folder)
                && let Some(opened) = source(&folder, write, FOLDER)
            {
                self.folders.push((folder, opened));
            }
        }
    }

    // The source on the file the name names now, kept where it has that file open, and a
    // look after it is made; true where that look tells of a change, which the caller tells
    // as the source's own event would.
    fn arm_file(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            if let Some(source) = self.file_source.take() {
                source.cancel();
            }
            self.file = None;
            return false;
        };
        let now = identity(&path);
        if now.is_none() || now != self.file {
            if let Some(source) = self.file_source.take() {
                source.cancel();
            }
            // past every number an event of the source before can pass
            self.generation += 1;
            let opened = source(&path, FILE, self.generation);
            self.file = opened.as_ref().map(|opened| opened.id);
            self.file_source = opened.map(|opened| opened.source);
        }
        // A file that came to the name between the look and the source is a change. Where no
        // source could be made for a file the look found, as one the user may not open, the
        // second look is held against the first: held against none, it would find a change
        // every time, and arming again would tell it again without end.
        identity(&path) != self.file.or(now)
    }
}

// The folder `path` is in.
fn folder(path: &Path) -> &Path {
    match path.parent() {
        Some(folder) if !folder.as_os_str().is_empty() => folder,
        _ => Path::new("."),
    }
}

// For a link, the folder of the name it ends at, made canonical, whether a file is there or
// not: its text followed link by link, as many as the system follows, 32, and no more.
fn named_folder(path: &Path) -> Option<PathBuf> {
    let mut name = path.to_path_buf();
    let mut links = 0;
    while let Ok(text) = std::fs::read_link(&name) {
        if links == 32 {
            return None;
        }
        links += 1;
        name = folder(&name).join(text);
    }
    std::fs::canonicalize(folder(&name))
        .ok()
        .filter(|_| links > 0)
}

// The device and inode `path` names now, following a link as the decoder's open does.
fn identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

// What a source's handlers get: the number its events pass on, and the descriptor it watches.
struct Context {
    event: usize,
    fd: libc::c_int,
}

// A source, and the device and inode of what its descriptor has open, which the descriptor
// keeps from being used again while it is open.
struct Opened {
    id: (u64, u64),
    source: DispatchRetained<DispatchSource>,
}

// A vnode source on `path` for `events`, whose events pass `event` on to the main queue; the
// descriptor, opened for events alone, closes once the source is done with it.
fn source(path: &Path, events: dispatch_source_vnode_flags_t, event: usize) -> Option<Opened> {
    let name = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `name` is a C string that lives across the call
    let fd = unsafe { libc::open(name.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a zeroed stat is a valid one for fstat to fill in
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor just opened, and a stat to fill in
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        // SAFETY: the descriptor just opened, which nothing else has
        unsafe { libc::close(fd) };
        return None;
    }
    // as std's metadata gives them, which `identity` compares
    let id = (stat.st_dev as u64, stat.st_ino);
    // SAFETY: a vnode source takes an open descriptor and a mask of vnode events
    let source = unsafe {
        DispatchSource::new(
            (&raw const dispatch2::_dispatch_source_type_vnode).cast_mut(),
            fd as usize,
            events.0 as usize,
            Some(DispatchQueue::main()),
        )
    };
    let context = Box::into_raw(Box::new(Context { event, fd }));
    // SAFETY: the context lives until the cancel handler, which runs after the last event
    // handler, frees it
    unsafe { source.set_context(context.cast()) };
    source.set_event_handler_f(fired);
    source.set_cancel_handler_f(canceled);
    source.activate();
    Some(Opened { id, source })
}

extern "C" fn fired(context: *mut c_void) {
    // SAFETY: the context `source` gave, alive until the source is canceled
    let event = unsafe { (*context.cast::<Context>()).event };
    crate::app::heard(event);
}

extern "C" fn canceled(context: *mut c_void) {
    // SAFETY: the context `source` gave; this is the last handler, so it is ours to free
    let context = unsafe { Box::from_raw(context.cast::<Context>()) };
    // SAFETY: the descriptor `source` opened, which nothing else closes
    unsafe { libc::close(context.fd) };
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use super::{FOLDER, Watch, identity, named_folder};

    // A folder of its own for a test, empty.
    fn scratch(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // A save over the name moves the file's source, and the folder's stays as it was, so no
    // save between a look and a source goes untold.
    #[test]
    fn the_folder_s_source_stays_across_the_saves_it_tells() {
        let dir = scratch("watch-keep");
        let name = dir.join("watched");
        std::fs::write(&name, b"one").unwrap();
        let mut watch = Watch::new();
        watch.set(Some(&name));
        let folder = |watch: &Watch| {
            assert_eq!(watch.folders.len(), 1);
            &*watch.folders[0].1.source as *const _
        };
        let (kept, generation) = (folder(&watch), watch.generation);
        std::fs::write(dir.join("new"), b"two").unwrap();
        std::fs::rename(dir.join("new"), &name).unwrap();
        assert!(watch.changed(FOLDER), "the folder tells of the save");
        assert_eq!(folder(&watch), kept, "the folder's source stays");
        assert!(
            !watch.changed(generation),
            "the file's source before is gone"
        );
        assert!(!watch.changed(FOLDER), "the same file again is no change");
        watch.set(None);
        assert!(watch.folders.is_empty() && watch.file_source.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The file's source stays where the name names the file it has open: through a write in
    // place, which its own event tells, and the set that follows as the viewer decodes again;
    // a save by a rename moves it.
    #[test]
    fn the_file_s_source_stays_for_the_file_it_has_open() {
        let dir = scratch("watch-file");
        let name = dir.join("watched");
        std::fs::write(&name, b"one").unwrap();
        let mut watch = Watch::new();
        watch.set(Some(&name));
        let source = |watch: &Watch| {
            let source = watch.file_source.as_ref().expect("a file's source");
            (&**source as *const _, watch.generation)
        };
        let kept = source(&watch);
        std::fs::write(&name, b"two").unwrap();
        assert!(
            watch.changed(kept.1),
            "the file's own event tells of the write"
        );
        assert_eq!(source(&watch), kept, "the write leaves the source");
        watch.set(Some(&name));
        assert_eq!(source(&watch), kept, "and so does the set after it");
        std::fs::write(dir.join("new"), b"three").unwrap();
        std::fs::rename(dir.join("new"), &name).unwrap();
        assert!(
            watch.changed(kept.1),
            "the file's own event tells of the rename"
        );
        assert_ne!(
            source(&watch).1,
            kept.1,
            "the save by a rename moves the source"
        );
        assert_eq!(watch.file, identity(&name));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // A file the look finds and no source can open, here one with no permission bits: arming
    // tells nothing of it, where a second look held against no file would tell a change at
    // every arming.
    #[test]
    fn a_file_that_cannot_be_opened_tells_no_change_as_it_is_armed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("watch-closed");
        let name = dir.join("closed");
        std::fs::write(&name, b"closed").unwrap();
        std::fs::set_permissions(&name, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut watch = Watch::new();
        watch.path = Some(name.clone());
        assert!(!watch.arm_file(), "the first arming tells no change");
        if watch.file_source.is_some() {
            // root opens it whatever its bits say: the case cannot be made here
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        assert!(identity(&name).is_some());
        assert!(!watch.arm_file(), "nor does arming again");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // A folder renamed away and another made under its path: the next set watches the new one.
    #[test]
    fn a_folder_replaced_under_its_path_gets_a_new_source() {
        let dir = scratch("watch-folder");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a"), b"a").unwrap();
        let mut watch = Watch::new();
        watch.set(Some(&sub.join("a")));
        let folder = |watch: &Watch| {
            assert_eq!(watch.folders.len(), 1);
            watch.folders[0].1.id
        };
        let old = folder(&watch);
        std::fs::rename(&sub, dir.join("old")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("b"), b"b").unwrap();
        watch.set(Some(&sub.join("b")));
        assert_ne!(
            folder(&watch),
            old,
            "the folder renamed away is not watched"
        );
        assert_eq!(Some(folder(&watch)), identity(&sub), "the new folder is");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Real links in a folder of their own: the folder each ends at, and none where the system
    // would not open what it names either.
    #[test]
    fn named_folder_follows_links_as_far_as_the_system_does() {
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for folder in ["a", "b", "chain"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let link = |name: &str, text: &Path| symlink(text, dir.join(name)).unwrap();
        std::fs::write(dir.join("a/file"), b"").unwrap();
        link("relative", Path::new("a/file"));
        link("missing", Path::new("b/nothing"));
        link("absolute", &dir.join("b/nothing"));
        link("twice", Path::new("relative"));
        link("loop-1", Path::new("loop-2"));
        link("loop-2", Path::new("loop-1"));
        // chain/0 is the file; chain/n links to n - 1, so chain/n is n links from it
        std::fs::write(dir.join("chain/0"), b"").unwrap();
        for n in 1..=33 {
            symlink(format!("{}", n - 1), dir.join(format!("chain/{n}"))).unwrap();
        }
        let named = |name: &str| named_folder(&dir.join(name));
        assert_eq!(named("a/file"), None);
        assert_eq!(named("relative"), Some(dir.join("a")));
        assert_eq!(named("missing"), Some(dir.join("b")));
        assert_eq!(named("absolute"), Some(dir.join("b")));
        assert_eq!(named("twice"), Some(dir.join("a")));
        assert_eq!(named("loop-1"), None);
        assert_eq!(named("chain/32"), Some(dir.join("chain")));
        assert!(std::fs::metadata(dir.join("chain/32")).is_ok());
        assert_eq!(named("chain/33"), None);
        assert!(std::fs::metadata(dir.join("chain/33")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
