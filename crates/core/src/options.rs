//! Command-line options, parsed the way nsxiv parses them: short flags that
//! cluster like getopt's (`-rt` is `-r -t`, `-n5` is `-n 5`), long aliases,
//! and `--` before paths that start with a dash.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use crate::Size;
use crate::config::{APP_NAME, SLIDESHOW_DELAY};
use crate::viewport::ScaleMode;

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub no_bar: bool,
    pub fullscreen: bool,
    pub thumbnail_mode: bool,
    pub recursive: bool,
    pub read_stdin: bool,
    pub output_marked: bool,
    pub quiet: bool,
    pub private_mode: bool,
    /// The file number to start at, counting from 1.
    pub start_at: usize,
    /// Seconds between slideshow images.
    pub slideshow: Option<f64>,
    /// 1 is 100%.
    pub zoom: Option<f64>,
    /// Never `ScaleMode::Manual`, which only zooming sets.
    pub scale_mode: Option<ScaleMode>,
    /// A fixed window size instead of one that fits each image.
    pub geometry: Option<Size>,
    pub paths: Vec<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            no_bar: false,
            fullscreen: false,
            thumbnail_mode: false,
            recursive: false,
            read_stdin: false,
            output_marked: false,
            quiet: false,
            private_mode: false,
            start_at: 1,
            slideshow: None,
            zoom: None,
            scale_mode: None,
            geometry: None,
            paths: Vec::new(),
        }
    }
}

/// What the command line asks for.
#[derive(Debug, PartialEq)]
pub enum Command {
    Run(Options),
    Version,
    Help,
}

/// A command line that nsxiv-ng refuses; the caller exits with status 2.
#[derive(Debug, PartialEq, Eq)]
pub enum UsageError {
    MissingValue(String),
    InvalidScaleMode,
    UnknownOption(String),
}

impl UsageError {
    /// Whether the usage text should follow the message.
    pub fn shows_usage(&self) -> bool {
        matches!(self, UsageError::UnknownOption(_))
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageError::MissingValue(flag) => write!(f, "option {flag} requires an argument"),
            UsageError::InvalidScaleMode => {
                f.write_str("invalid scale mode, expected one of: d f F w h")
            }
            UsageError::UnknownOption(flag) => write!(f, "unknown option: {flag}"),
        }
    }
}

pub fn usage() -> String {
    format!(
        "usage: {APP_NAME} [-bfiopqrtv] [-g WxH] [-n NUM] [-S DELAY] [-s MODE] [-z ZOOM] FILE...

  -b            start with the status bar hidden, whatever show_bar in the config says
  -f            start in fullscreen
  -g WxH        fixed window size instead of fitting the window to each image
  -i            read file paths from stdin
  -n NUM        start at file number NUM
  -o            print marked files to stdout on quit
  -p            private mode, do not write a thumbnail cache
  -q            be quiet, disable warnings
  -r            search directories recursively
  -S DELAY      start slideshow with DELAY seconds between images
  -s MODE       set scale mode: d=down, f=fit, F=fill, w=width, h=height
  -t            start in thumbnail mode
  -v            print version and exit
  -z ZOOM       set zoom level in percent"
    )
}

/// Parses the arguments after the program name. Paths keep their bytes, so a
/// file name that is not UTF-8 still opens.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, UsageError> {
    let mut o = Options::default();
    let mut args: VecDeque<OsString> = args.into_iter().collect();
    while let Some(arg) = args.pop_front() {
        let Some(a) = arg.to_str() else {
            // flags are ASCII, so an argument that is not UTF-8 is a path, or an unknown option
            if arg.as_encoded_bytes().starts_with(b"-") && arg.len() > 1 {
                return Err(UsageError::UnknownOption(
                    arg.to_string_lossy().into_owned(),
                ));
            }
            o.paths.push(arg.into());
            continue;
        };
        match a {
            "-b" | "--no-bar" => o.no_bar = true,
            "-f" | "--fullscreen" => o.fullscreen = true,
            "-t" | "--thumbnail" => o.thumbnail_mode = true,
            "-r" | "--recursive" => o.recursive = true,
            "-i" | "--stdin" => o.read_stdin = true,
            "-o" | "--output" => o.output_marked = true,
            "-q" | "--quiet" => o.quiet = true,
            "-p" | "--private" => o.private_mode = true,
            "-n" | "--start-at" => o.start_at = value(&mut args, a)?.parse().unwrap_or(1).max(1),
            "-S" | "--ss-delay" => {
                let delay = finite(&value(&mut args, a)?).unwrap_or(SLIDESHOW_DELAY);
                o.slideshow = Some(f64::max(delay, 0.1));
            }
            "-z" | "--zoom" => {
                if let Some(z) = finite(&value(&mut args, a)?)
                    && z > 0.0
                {
                    o.zoom = Some(z / 100.0);
                }
            }
            "-s" | "--scale-mode" => {
                o.scale_mode = Some(match value(&mut args, a)?.as_str() {
                    "d" => ScaleMode::Down,
                    "f" => ScaleMode::Fit,
                    "F" => ScaleMode::Fill,
                    "w" => ScaleMode::Width,
                    "h" => ScaleMode::Height,
                    _ => return Err(UsageError::InvalidScaleMode),
                })
            }
            "-g" | "--geometry" => {
                let text = value(&mut args, a)?.to_lowercase();
                // empty parts drop out, so x800x600 reads as 800x600
                let parts: Vec<&str> = text.split('x').filter(|p| !p.is_empty()).collect();
                if let [w, h] = parts.as_slice()
                    && let (Some(width), Some(height)) = (finite(w), finite(h))
                    && width > 0.0
                    && height > 0.0
                {
                    o.geometry = Some(Size { width, height });
                }
            }
            "-v" | "--version" => return Ok(Command::Version),
            "-h" | "--help" => return Ok(Command::Help),
            "--" => o.paths.extend(args.drain(..).map(PathBuf::from)),
            _ if a.starts_with('-') && !a.starts_with("--") && a.chars().count() > 2 => {
                // a cluster: the first flag splits off, and the rest is its value or more flags
                let flag: String = a.chars().take(2).collect();
                let rest: String = a.chars().skip(2).collect();
                let takes_value = flag.ends_with(['g', 'n', 'S', 's', 'z']);
                let next = if takes_value {
                    rest
                } else {
                    format!("-{rest}")
                };
                args.push_front(next.into());
                args.push_front(flag.into());
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                return Err(UsageError::UnknownOption(a.to_owned()));
            }
            _ => o.paths.push(arg.into()),
        }
    }
    Ok(Command::Run(o))
}

/// A number, unless it is infinite or NaN, which no timer or window size can take.
fn finite(text: &str) -> Option<f64> {
    text.parse().ok().filter(|v: &f64| v.is_finite())
}

fn value(args: &mut VecDeque<OsString>, flag: &str) -> Result<String, UsageError> {
    args.pop_front()
        .map(|v| v.to_string_lossy().into_owned())
        .ok_or_else(|| UsageError::MissingValue(flag.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Options {
        match parse(args.iter().map(OsString::from)) {
            Ok(Command::Run(o)) => o,
            other => panic!("{args:?} gave {other:?}"),
        }
    }

    fn error(args: &[&str]) -> UsageError {
        parse(args.iter().map(OsString::from)).expect_err("a usage error")
    }

    #[test]
    fn short_options_cluster_like_getopt() {
        let o = run(&["-rtn5", "-sw", "a.png", "--", "-b.png"]);
        assert!(o.recursive && o.thumbnail_mode);
        assert_eq!(o.start_at, 5);
        assert_eq!(o.scale_mode, Some(ScaleMode::Width));
        assert_eq!(o.paths, [PathBuf::from("a.png"), PathBuf::from("-b.png")]);
    }

    #[test]
    fn every_flag_and_its_long_name_sets_its_option() {
        let set = |o: &Options| {
            [
                o.no_bar,
                o.fullscreen,
                o.thumbnail_mode,
                o.recursive,
                o.read_stdin,
                o.output_marked,
                o.quiet,
                o.private_mode,
            ]
        };
        assert_eq!(set(&run(&["-bftriopq"])), [true; 8]);
        let long = [
            "--no-bar",
            "--fullscreen",
            "--thumbnail",
            "--recursive",
            "--stdin",
            "--output",
            "--quiet",
            "--private",
        ];
        assert_eq!(set(&run(&long)), [true; 8]);
        assert_eq!(set(&Options::default()), [false; 8]);
        let o = run(&[
            "--start-at",
            "3",
            "--ss-delay",
            "2",
            "--zoom",
            "50",
            "--scale-mode",
            "F",
            "--geometry",
            "640X480",
        ]);
        assert_eq!(
            (o.start_at, o.slideshow, o.zoom, o.scale_mode),
            (3, Some(2.0), Some(0.5), Some(ScaleMode::Fill))
        );
        assert_eq!(
            o.geometry,
            Some(Size {
                width: 640.0,
                height: 480.0
            })
        );
        for (mode, scale) in [
            ("d", ScaleMode::Down),
            ("f", ScaleMode::Fit),
            ("w", ScaleMode::Width),
            ("h", ScaleMode::Height),
        ] {
            assert_eq!(run(&["-s", mode]).scale_mode, Some(scale));
        }
    }

    #[test]
    fn values_parse_or_fall_back() {
        let o = run(&["-g", "800x600", "-z", "150", "-S", "0.01", "-n", "x"]);
        assert_eq!(
            o.geometry,
            Some(Size {
                width: 800.0,
                height: 600.0
            })
        );
        assert_eq!(o.zoom, Some(1.5));
        assert_eq!(o.slideshow, Some(0.1));
        assert_eq!(o.start_at, 1);
        let o = run(&["-gX640", "-z", "0", "-S", "soon"]);
        assert_eq!(o.geometry, None);
        assert_eq!(o.zoom, None);
        assert_eq!(o.slideshow, Some(SLIDESHOW_DELAY));
        let o = run(&["-S", "inf", "-z", "inf", "-g", "infx600"]);
        assert_eq!(
            (o.slideshow, o.zoom, o.geometry),
            (Some(SLIDESHOW_DELAY), None, None)
        );
        assert_eq!(run(&["-n0"]).start_at, 1);
        assert_eq!(run(&["--start-at", "0"]).start_at, 1);
    }

    #[test]
    fn bad_command_lines_are_refused() {
        assert_eq!(
            error(&["a.png", "-n"]),
            UsageError::MissingValue("-n".into())
        );
        assert_eq!(error(&["-s", "q"]), UsageError::InvalidScaleMode);
        assert_eq!(error(&["-rx"]), UsageError::UnknownOption("-x".into()));
        assert_eq!(
            error(&["--zoom=50"]),
            UsageError::UnknownOption("--zoom=50".into())
        );
        assert!(error(&["-x"]).shows_usage() && !UsageError::InvalidScaleMode.shows_usage());
    }

    #[test]
    fn version_help_and_lone_dash() {
        assert_eq!(parse(["-qv"].map(OsString::from)), Ok(Command::Version));
        assert_eq!(parse(["--help"].map(OsString::from)), Ok(Command::Help));
        assert_eq!(run(&["-"]).paths, [PathBuf::from("-")]);
    }

    #[cfg(unix)]
    #[test]
    fn paths_keep_bytes_that_are_not_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let name = OsString::from_vec(b"caf\xe9.png".to_vec());
        let Ok(Command::Run(o)) = parse([name.clone()]) else {
            panic!("not a run")
        };
        assert_eq!(o.paths, [PathBuf::from(name)]);
    }
}
