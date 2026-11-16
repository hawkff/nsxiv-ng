//! `etc/nsxiv-rifle`, which opens one image with its whole directory: its list of extensions
//! is the viewer's, and it hands the viewer the directory's images in the viewer's own order.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use nsxiv_ng_core::file_list::FileList;
use nsxiv_ng_decode::EXTENSIONS;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../etc/nsxiv-rifle")
}

#[test]
fn the_script_takes_the_extensions_the_viewer_scans_for() {
    let text = std::fs::read_to_string(script()).unwrap();
    let list = text
        .split_once("extensions='")
        .and_then(|(_, rest)| rest.split_once('\''))
        .map(|(list, _)| list)
        .expect("a line extensions='...'");
    let words: Vec<&str> = list.split_whitespace().collect();
    assert_eq!(
        words, EXTENSIONS,
        "etc/nsxiv-rifle must list decode's EXTENSIONS"
    );
}

/// Where the script runs, and with what.
struct Run<'a> {
    bin: &'a Path,
    cwd: Option<&'a Path>,
    env: &'a [(&'a str, &'a Path)],
}

impl Run<'_> {
    // The script run with a stand-in nsxiv-ng first on PATH, which prints its working directory,
    // its arguments a line each, `--stdin--` and what it read on stdin: its status and that.
    fn rifle(&self, args: &[&OsStr]) -> (Option<i32>, String) {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new("sh");
        command.arg(script()).args(args).env("PATH", path);
        if let Some(cwd) = self.cwd {
            command.current_dir(cwd);
        }
        for (name, value) in self.env {
            command.env(name, value);
        }
        let out = command.output().unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    }
}

// What the stand-in prints for a run in `cwd` with `args` and `stdin`, as lines.
fn printed(cwd: &Path, args: &[&str], stdin: &[String]) -> String {
    let mut lines = vec![cwd.display().to_string()];
    lines.extend(args.iter().map(|a| a.to_string()));
    lines.push("--stdin--".into());
    lines.extend(stdin.iter().cloned());
    lines.iter().map(|line| format!("{line}\n")).collect()
}

fn touch(dir: &Path, names: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    for name in names {
        std::fs::write(dir.join(name), b"").unwrap();
    }
}

#[test]
fn one_image_opens_its_directory_in_the_viewer_s_order_at_that_image() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rifle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // the physical path, which the script and the stand-in print
    let base = base.canonicalize().unwrap();
    let (bin, dir) = (base.join("bin"), base.join("pictures"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(dir.join("album.png")).unwrap();
    let viewer = bin.join("nsxiv-ng");
    let stand_in = "#!/bin/sh\npwd -P\nprintf '%s\\n' \"$@\"\necho --stdin--\ncat\n";
    std::fs::write(&viewer, stand_in).unwrap();
    std::fs::set_permissions(&viewer, std::fs::Permissions::from_mode(0o755)).unwrap();
    touch(
        &dir,
        &[
            "img10.png",
            "img2.png",
            "Img1.png",
            "img02.png",
            "b c.png",
            "-dash.png",
            "photo.JPG",
            "a.jxl",
            "notes.txt",
            "png",
            ".hidden.png",
        ],
    );
    std::os::unix::fs::symlink(dir.join("img2.png"), dir.join("link.png")).unwrap();
    let plain = Run {
        bin: &bin,
        cwd: None,
        env: &[],
    };

    // the viewer's own order for the directory, by name, on stdin, and -n at the image
    let scanned: Vec<String> = FileList::build(std::slice::from_ref(&dir), false, EXTENSIONS)
        .files
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(scanned.len(), 9, "{scanned:?}");
    let at = scanned.iter().position(|n| n == "img2.png").unwrap() + 1;
    assert!(at > 1 && at < scanned.len(), "the target in the middle");
    let at = at.to_string();
    let expected = printed(&dir, &["-i", "-n", &at], &scanned);
    let target = dir.join("img2.png");
    assert_eq!(
        plain.rifle(&[target.as_os_str()]),
        (Some(0), expected.clone())
    );

    // a relative argument, with an exported CDPATH that holds another pictures folder
    touch(&base.join("other/pictures"), &["img2.png", "decoy.png"]);
    let cdpath = base.join("other");
    let away = Run {
        bin: &bin,
        cwd: Some(&base),
        env: &[("CDPATH", &cdpath)],
    };
    let relative = OsStr::new("pictures/img2.png");
    assert_eq!(away.rifle(&[relative]), (Some(0), expected));

    // .. from a working folder that is a link: the folder the link points into, as the kernel
    // resolves the path, not the one the link sits in
    touch(&base.join("real/inner"), &[]);
    touch(&base.join("real"), &["near.png", "x.png"]);
    let link = base.join("link");
    std::os::unix::fs::symlink(base.join("real/inner"), &link).unwrap();
    let through = Run {
        bin: &bin,
        cwd: Some(&link),
        env: &[("PWD", &link)],
    };
    let pair = ["near.png".to_owned(), "x.png".to_owned()];
    assert_eq!(
        through.rifle(&[OsStr::new("../x.png")]),
        (
            Some(0),
            printed(&base.join("real"), &["-i", "-n", "2"], &pair)
        )
    );

    // a folder named -, which cd would take for the folder before, OLDPWD, where an image of
    // the same name waits
    touch(&base.join("-"), &["img.png", "next.png"]);
    touch(&base.join("before"), &["img.png", "decoy.png"]);
    let before = base.join("before");
    let dashed = Run {
        bin: &bin,
        cwd: Some(&base),
        env: &[("OLDPWD", &before)],
    };
    let both = ["img.png".to_owned(), "next.png".to_owned()];
    assert_eq!(
        dashed.rifle(&[OsStr::new("-/img.png")]),
        (Some(0), printed(&base.join("-"), &["-i", "-n", "1"], &both))
    );

    // runs of 99 and 100 digits, ordered by their length as the viewer orders them
    let long = base.join("long");
    let (shorter, longer) = (
        format!("a{}.png", "9".repeat(99)),
        format!("a1{}.png", "0".repeat(99)),
    );
    touch(&long, &[&longer, &shorter]);
    let order: Vec<String> = FileList::build(std::slice::from_ref(&long), false, EXTENSIONS)
        .files
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(order, [shorter.clone(), longer.clone()]);
    assert_eq!(
        plain.rifle(&[long.join(&longer).as_os_str()]),
        (Some(0), printed(&long, &["-i", "-n", "2"], &order))
    );

    // a name that ends in a newline goes as it is
    touch(&dir, &["frame.png\n"]);
    let odd = dir.join("frame.png\n");
    let (status, stdout) = plain.rifle(&[odd.as_os_str()]);
    assert_eq!(status, Some(0));
    assert!(
        stdout.ends_with(&format!("\n--\n{}\n--stdin--\n", odd.display())),
        "{stdout:?}"
    );

    // no argument: nothing opens
    assert_eq!(plain.rifle(&[]), (Some(2), String::new()));
    // several, or a directory: as they are
    let (a, b) = (dir.join("img2.png"), dir.join("b c.png"));
    let (_, stdout) = plain.rifle(&[a.as_os_str(), b.as_os_str()]);
    let several = format!("\n--\n{}\n{}\n--stdin--\n", a.display(), b.display());
    assert!(stdout.ends_with(&several), "{stdout:?}");
    let (_, stdout) = plain.rifle(&[dir.as_os_str()]);
    let whole = format!("\n--\n{}\n--stdin--\n", dir.display());
    assert!(stdout.ends_with(&whole), "{stdout:?}");
    // a file the scan would not list: itself alone
    let notes = dir.join("notes.txt");
    assert_eq!(
        plain.rifle(&[notes.as_os_str()]),
        (Some(0), printed(&dir, &["--", "notes.txt"], &[]))
    );
    std::fs::remove_dir_all(&base).unwrap();
}
