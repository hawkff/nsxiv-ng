//! The user's key-handler: found, started with the key, fed the paths on its stdin, and
//! waited for, as nsxiv runs it.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nsxiv_ng_core::config::Config;

/// Where the key-handler may be: `exec/key-handler` in the folder that holds `config.toml`.
/// Empty when no config path can be resolved.
pub fn key_handlers() -> Vec<PathBuf> {
    beside(Config::path().as_deref())
}

// The key-handlers beside the config file at `config`, if there is one.
fn beside(config: Option<&Path>) -> Vec<PathBuf> {
    let Some(app) = config.and_then(Path::parent) else {
        return Vec::new();
    };
    vec![app.join("exec/key-handler")]
}

/// The first location that holds an executable file, looked up at each run, so a handler
/// installed while the viewer runs serves.
pub(crate) fn find(locations: &[PathBuf]) -> Option<&Path> {
    locations
        .iter()
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .map(PathBuf::as_path)
}

/// The paths a key-handler can be given, and those left out. Its stdin holds a path a line, so
/// a name with a newline in it would reach it as two paths, the second one nobody chose; a
/// handler cannot tell, as nsxiv's protocol has no way to say it.
pub(crate) fn passable(files: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    files
        .into_iter()
        .partition(|file| !file.as_os_str().as_bytes().contains(&b'\n'))
}

/// Runs the handler with `key` as its argument and the paths on its stdin, one a line, and
/// waits for it. It keeps the frontend's stdout and stderr, for its prompts and its output.
/// `prepare` readies its command first, as the frontend asks.
pub(crate) fn run(
    handler: &Path,
    key: &str,
    files: &[PathBuf],
    prepare: fn(&mut Command),
) -> io::Result<()> {
    let mut command = Command::new(handler);
    command.arg(key).stdin(Stdio::piped());
    prepare(&mut command);
    let mut child = command.spawn()?;
    let mut list = Vec::new();
    for file in files {
        list.extend_from_slice(file.as_os_str().as_bytes());
        list.push(b'\n');
    }
    if let Some(mut stdin) = child.stdin.take() {
        // a handler may stop reading early, which is no error
        let _ = stdin.write_all(&list);
    }
    child.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_a_newline_is_left_out() {
        let files = ["/p/a.png", "/p/keep.png\nvictim.png", "/p/b.png"].map(PathBuf::from);
        let (passed, left_out) = passable(files.to_vec());
        assert_eq!(passed, [files[0].clone(), files[2].clone()]);
        assert_eq!(left_out, [files[1].clone()]);
    }

    #[test]
    fn a_handler_that_reads_nothing_and_fails_is_no_error() {
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-quick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let handler = dir.join("key-handler");
        std::fs::write(&handler, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755)).unwrap();
        // more paths than a pipe holds, so writing them meets the closed end
        let files: Vec<PathBuf> = (0..20_000).map(|i| dir.join(format!("{i}.png"))).collect();
        assert!(
            run(&handler, "c", &files, |_| {}).is_ok(),
            "its status and its stdin are its own"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_handler_sits_beside_the_config_and_nowhere_else() {
        let config = Path::new("/home/u/.config/nsxiv-ng/config.toml");
        assert_eq!(
            beside(Some(config)),
            [PathBuf::from("/home/u/.config/nsxiv-ng/exec/key-handler")]
        );
        assert_eq!(
            beside(None),
            [] as [PathBuf; 0],
            "no home folder, no places"
        );
    }
}
