//! What the binary writes to stderr before it opens a terminal.

use std::io::Write;
use std::process::{Command, Stdio};

const PROGRAM: &str = env!("CARGO_BIN_EXE_nsxiv-ng");

// whether text holds no control character but the newlines between its lines
fn clean(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c != '\n' && matches!(c, '\0'..='\x1f' | '\x7f'..='\u{9f}'))
}

#[test]
fn names_from_a_pipe_a_config_path_and_an_option_reach_stderr_without_control_characters() {
    // a config whose folder holds an escape, with a key it does not know
    let config = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("config-{}-\x1b]0;title\x07", std::process::id()));
    std::fs::create_dir_all(config.join("nsxiv-ng")).unwrap();
    std::fs::write(config.join("nsxiv-ng/config.toml"), "no_such_key = 1\n").unwrap();
    let mut viewer = Command::new(PROGRAM)
        .arg("-i")
        .env("XDG_CONFIG_HOME", &config)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let name = "/nonexistent/a\x1b]52;c;eA==\x07b\u{9b}.png\n";
    viewer
        .stdin
        .take()
        .unwrap()
        .write_all(name.as_bytes())
        .unwrap();
    let out = viewer.wait_with_output().unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(clean(&stderr), "{stderr:?}");
    assert!(
        stderr.contains("nsxiv-ng: no such file: /nonexistent/a?]52;c;eA==?b?.png\n"),
        "{stderr:?}"
    );
    assert!(
        stderr.contains("]0;title?/nsxiv-ng/config.toml"),
        "{stderr:?}"
    );
    assert!(stderr.ends_with("nsxiv-ng: no images to display\n"));
    std::fs::remove_dir_all(&config).unwrap();

    let out = Command::new(PROGRAM)
        .arg("--\x1b[2J")
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(clean(&stderr), "{stderr:?}");
    assert!(stderr.contains("?[2J"), "{stderr:?}");
}

#[test]
fn no_files_a_quiet_miss_and_help_end_with_their_own_statuses() {
    // a folder of the test's own: an empty config folder, and an image that is not there
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("endings-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let missing = dir.join("missing.png");
    let missing = missing.to_str().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(PROGRAM)
            .args(args)
            .env("XDG_CONFIG_HOME", &config)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = |bytes| String::from_utf8(bytes).unwrap();
        (out.status.code(), text(out.stdout), text(out.stderr))
    };
    // no paths: the usage on stderr, status 2
    let (status, stdout, stderr) = run(&[]);
    assert_eq!((status, stdout.as_str()), (Some(2), ""));
    assert!(
        stderr.starts_with("usage: nsxiv-ng [-bfiopqrtv]"),
        "{stderr}"
    );
    // -q leaves out the missing file, not that no image is left
    let (status, _, stderr) = run(&["-q", missing]);
    assert_eq!(
        (status, stderr.as_str()),
        (Some(1), "nsxiv-ng: no images to display\n")
    );
    // -h: the usage on stdout, status 0
    let (status, stdout, stderr) = run(&["-h"]);
    assert_eq!((status, stderr.as_str()), (Some(0), ""));
    assert!(stdout.starts_with("usage: nsxiv-ng"), "{stdout}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn the_process_serial_number_an_older_macos_gives_is_no_option() {
    let out = Command::new(PROGRAM)
        .args(["-psn_0_12345", "-v"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.starts_with("nsxiv-ng "),
        "{out:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_bundle_s_binary_with_no_files_waits_for_finder_s_then_shows_the_usage() {
    // the binary as an application bundle's, which Finder opens with no files on its line
    let app = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("bundle-{}/nsxiv-ng.app", std::process::id()));
    let bin = app.join("Contents/MacOS");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(PROGRAM, bin.join("nsxiv-ng")).unwrap();
    let started = std::time::Instant::now();
    let out = Command::new(bin.join("nsxiv-ng")).output().unwrap();
    let waited = started.elapsed();
    let _ = std::fs::remove_dir_all(app.parent().unwrap());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("usage: nsxiv-ng"), "{stderr}");
    assert!(
        waited.as_secs_f64() >= 2.0,
        "waited {waited:?} for files from Finder"
    );
}

#[test]
fn names_after_double_dash_that_start_as_a_process_serial_number_stay_files() {
    let out = Command::new(PROGRAM)
        .args(["--", "-psn_notes.png", "-psn_0_12345"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    for name in ["-psn_notes.png", "-psn_0_12345"] {
        assert!(
            stderr.contains(&format!("no such file: {name}")),
            "{stderr}"
        );
    }
}
