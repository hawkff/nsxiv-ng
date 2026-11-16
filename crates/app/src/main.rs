//! nsxiv-ng: parses the command line and starts a viewer.
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nsxiv_ng_core::config::{APP_NAME, Config, VERSION};
use nsxiv_ng_core::file_list::{FileEntry, FileList};
use nsxiv_ng_core::options::{self, Command, Options};
use nsxiv_ng_core::printable;
use nsxiv_ng_helper as helper;

// A line for stderr, cleaned, as it may repeat a name or other text from outside.
fn warn(line: &str) {
    eprintln!("{APP_NAME}: {}", printable(line));
}

fn main() -> ExitCode {
    // a decoding helper, or the probe of its sandbox, both of which the viewer runs as itself
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(flag) if flag.as_os_str() == helper::SERVE => {
            return match args.next().and_then(|cap| cap.to_str()?.parse().ok()) {
                Some(cap) => helper::serve(cap),
                None => ExitCode::FAILURE,
            };
        }
        Some(flag) if flag.as_os_str() == helper::PROBE => return helper::probe(),
        _ => {}
    }
    match options::parse(arguments()) {
        // a closed stdout, as in `nsxiv-ng -v | true`, is no error
        Ok(Command::Version) => {
            let status = std::env::current_exe().map_or_else(
                |e| format!("sandbox: none ({e})"),
                |program| helper::sandbox_status(&program),
            );
            let _ = writeln!(std::io::stdout(), "{APP_NAME} {VERSION}\n{status}");
        }
        Ok(Command::Help) => {
            let _ = writeln!(std::io::stdout(), "{}", options::usage());
        }
        Ok(Command::Run(o)) => return run(o),
        Err(e) => {
            warn(&e.to_string());
            if e.shows_usage() {
                eprintln!("{}", options::usage());
            }
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

// The command line's arguments, but the process serial number an older macOS gives an
// application Finder opens as its first, -psn_0_12345, which is no option of the viewer's.
fn arguments() -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if cfg!(target_os = "macos")
        && args
            .first()
            .is_some_and(|arg| serial_number(arg.as_bytes()))
    {
        args.remove(0);
    }
    args
}

// Whether `arg` is a process serial number as a launcher passes it: -psn_, digits, _, digits.
fn serial_number(arg: &[u8]) -> bool {
    let digits = |part: &[u8]| !part.is_empty() && part.iter().all(u8::is_ascii_digit);
    arg.strip_prefix(b"-psn_")
        .and_then(|rest| {
            rest.iter()
                .position(|&b| b == b'_')
                .map(|i| rest.split_at(i))
        })
        .is_some_and(|(high, low)| digits(high) && digits(&low[1..]))
}

// Whether the viewer is an application bundle's binary, which Finder and `open` start with
// no files on the command line and give them in an event once it launched.
fn in_bundle(exe: &Path) -> bool {
    exe.parent().is_some_and(|dir| {
        dir.ends_with("Contents/MacOS")
            && (dir.parent().and_then(Path::parent))
                .is_some_and(|app| app.extension() == Some(OsStr::new("app")))
    })
}

// The paths -i reads: a path a line, a line ending in \r\n as well, and blank lines left
// out. Each keeps its bytes, so a name that is not UTF-8 still names its file.
fn read_paths(input: impl BufRead) -> Vec<PathBuf> {
    input
        .split(b'\n')
        .map_while(Result::ok)
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            (!line.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(line)))
        })
        .collect()
}

// The viewer: the config, the files from the command line and with -i from stdin, then the
// frontend.
fn run(options: Options) -> ExitCode {
    let (config, warnings) = Config::load();
    if !options.quiet {
        for warning in warnings {
            warn(&warning);
        }
    }
    let mut paths = options.paths.clone();
    if options.read_stdin {
        paths.extend(read_paths(std::io::stdin().lock()));
    }
    // decoding helpers run as this binary
    let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    // the bundle's binary gets its files from Finder, which the frontend waits for
    if paths.is_empty() && !(cfg!(target_os = "macos") && in_bundle(&program)) {
        eprintln!("{}", options::usage());
        return ExitCode::from(2);
    }
    let (recursive, quiet) = (options.recursive, options.quiet);
    let found = move |paths: &[PathBuf]| {
        let list = FileList::build(paths, recursive, nsxiv_ng_decode::EXTENSIONS);
        if !quiet {
            for missing in &list.missing {
                warn(&format!("no such file: {}", missing.display()));
            }
        }
        list.files
    };
    let files = found(&paths);
    if files.is_empty() && !paths.is_empty() {
        warn("no images to display");
        return ExitCode::FAILURE;
    }
    #[cfg(target_os = "macos")]
    return show(options, files, &config, program, Box::new(found));
    #[cfg(target_os = "linux")]
    show(options, files, &config, program)
}

// The first frontend that finds what it shows on.
#[cfg(target_os = "linux")]
fn show(options: Options, files: Vec<FileEntry>, config: &Config, program: PathBuf) -> ExitCode {
    use nsxiv_ng_linux::{terminal, wayland, x11};
    // a Wayland session gets a Wayland window; X11, or XWayland with WAYLAND_DISPLAY unset, an
    // X one; with no display, as over SSH on a headless machine, the terminal shows the images
    let error = if let Some(wayland) = wayland::open() {
        wayland.run(options, files, config, program)
    } else if let Some(x11) = x11::open() {
        x11.run(options, files, config, program)
    } else if let Some(tty) = terminal::open() {
        tty.run(options, files, config, program)
    } else {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| "(DISPLAY unset)".to_owned());
        warn(&format!("cannot open display {display}"));
        return ExitCode::FAILURE;
    };
    warn(&error.to_string());
    ExitCode::FAILURE
}

#[cfg(target_os = "macos")]
fn show(
    options: Options,
    files: Vec<FileEntry>,
    config: &Config,
    program: PathBuf,
    found: nsxiv_ng_macos::Files,
) -> ExitCode {
    let error = nsxiv_ng_macos::run(options, files, config, program, found);
    warn(&error.to_string());
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_gives_a_path_a_line() {
        let input: &[u8] = b"a.png\nb c.png\r\n\n\nd.png";
        assert_eq!(
            read_paths(input),
            ["a.png", "b c.png", "d.png"].map(PathBuf::from)
        );
    }

    #[test]
    fn a_binary_in_an_application_bundle_is_one_finder_opens() {
        assert!(in_bundle(Path::new(
            "/Applications/nsxiv-ng.app/Contents/MacOS/nsxiv-ng"
        )));
        assert!(!in_bundle(Path::new("/opt/homebrew/bin/nsxiv-ng")));
        assert!(!in_bundle(Path::new("/x/Contents/MacOS/nsxiv-ng")));
    }

    #[test]
    fn a_process_serial_number_is_digits_after_psn() {
        assert!(serial_number(b"-psn_0_12345"));
        for other in [
            &b"-psn_notes.png"[..],
            b"-psn_0_",
            b"-psn__1",
            b"-psn_0_1.png",
            b"-psn_0",
        ] {
            assert!(!serial_number(other), "{}", String::from_utf8_lossy(other));
        }
    }

    #[test]
    fn a_path_from_stdin_keeps_bytes_that_are_not_utf8() {
        assert_eq!(
            read_paths(&b"\xff.png\n"[..]),
            [PathBuf::from(OsStr::from_bytes(b"\xff.png"))]
        );
    }
}
