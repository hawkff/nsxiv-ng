//! Measures an X11 image viewer that takes nsxiv's keys and options: the time from a key to the
//! frame it draws, and peak memory. It starts the viewer on the display in `DISPLAY`, presses
//! keys through XTEST and times the damage DAMAGE reports on the viewer's window.
#![forbid(unsafe_code)]

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("nsxiv-ng-speed measures an X11 viewer, on Linux");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
mod calibrate;
#[cfg(target_os = "linux")]
mod cases;
#[cfg(target_os = "linux")]
mod keys;
#[cfg(target_os = "linux")]
mod memory;
#[cfg(target_os = "linux")]
mod paint;
#[cfg(target_os = "linux")]
mod settle;
#[cfg(target_os = "linux")]
mod stats;
#[cfg(target_os = "linux")]
mod x;

#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use x::Res;

#[cfg(target_os = "linux")]
const USAGE: &str = "usage:
  nsxiv-ng-speed calibrate [FLAGS]
  nsxiv-ng-speed measure [FLAGS] FIXTURES VIEWER [ARGS...]
  nsxiv-ng-speed compare [--margin PERCENT] BASELINE.tsv CANDIDATE.tsv [BASELINE.tsv CANDIDATE.tsv]...
flags, with their defaults:
  --settle-ms 500     a frame ends once its window draws nothing this long; not counted
  --limit-ms 5000     a key that damages nothing this long drew nothing
  --repeats 20        samples of each key case
  --idle-ms 1000      the pause after a start, and before each key of next-ahead
  --grid-runs 1       cold and warm starts of the grid
  --tolerance-ms 5    how far the calibration may measure above a delay it set";

/// The measurement's settings, times in milliseconds.
#[cfg(target_os = "linux")]
pub struct Flags {
    pub settle: f64,
    pub limit: f64,
    pub repeats: usize,
    pub idle: f64,
    pub grid_runs: usize,
    pub tolerance: f64,
}

/// Takes the flags off the front of `args`.
#[cfg(target_os = "linux")]
fn flags(args: &mut Vec<String>) -> Res<Flags> {
    let mut flags = Flags {
        settle: 500.0,
        limit: 5000.0,
        repeats: 20,
        idle: 1000.0,
        grid_runs: 1,
        tolerance: 5.0,
    };
    while args.first().is_some_and(|a| a.starts_with("--")) {
        let flag = args.remove(0);
        let value = (!args.is_empty())
            .then(|| args.remove(0))
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let bad = || format!("{flag}: not a finite number of at least 1: {value}");
        let number = value
            .parse::<f64>()
            .ok()
            .filter(|&n| n.is_finite() && n >= 1.0)
            .ok_or_else(bad)?;
        let count = value.parse::<usize>().ok().filter(|&n| n >= 1);
        match flag.as_str() {
            "--settle-ms" => flags.settle = number,
            "--limit-ms" => flags.limit = number,
            "--idle-ms" => flags.idle = number,
            "--tolerance-ms" => flags.tolerance = number,
            "--repeats" => flags.repeats = count.ok_or_else(bad)?,
            "--grid-runs" => flags.grid_runs = count.ok_or_else(bad)?,
            _ => return Err(format!("unknown flag {flag}\n{USAGE}").into()),
        }
    }
    Ok(flags)
}

#[cfg(target_os = "linux")]
fn run(mut args: Vec<String>) -> Res<ExitCode> {
    let command = if args.is_empty() {
        String::new()
    } else {
        args.remove(0)
    };
    match command.as_str() {
        "calibrate" => {
            let flags = flags(&mut args)?;
            if !args.is_empty() {
                return Err(USAGE.into());
            }
            Ok(if calibrate::run(&flags)? {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        "measure" => {
            let flags = flags(&mut args)?;
            if args.len() < 2 {
                return Err(USAGE.into());
            }
            print!("{}", cases::run(&flags, Path::new(&args[0]), &args[1..])?);
            Ok(ExitCode::SUCCESS)
        }
        "compare" => {
            let mut margin = 10.0;
            if args.first().is_some_and(|a| a == "--margin") && args.len() > 1 {
                args.remove(0);
                let text = args.remove(0);
                margin = text
                    .parse()
                    .map_err(|_| format!("--margin: not a number: {text}"))?;
            }
            if args.is_empty() || args.len() % 2 == 1 {
                return Err(USAGE.into());
            }
            let tables = args
                .iter()
                .map(std::fs::read_to_string)
                .collect::<Result<Vec<_>, _>>()?;
            let rounds: Vec<(&str, &str)> = tables
                .chunks(2)
                .map(|pair| (pair[0].as_str(), pair[1].as_str()))
                .collect();
            let (table, pass) = stats::compare(&rounds, margin)?;
            print!("{table}");
            Ok(if pass {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        // the calibration's client and its child
        "paint" => paint::run(&args).map(|()| ExitCode::SUCCESS),
        "hold" => paint::hold(&args).map(|()| ExitCode::SUCCESS),
        _ => Err(USAGE.into()),
    }
}

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    run(std::env::args().skip(1).collect()).unwrap_or_else(|e| {
        eprintln!("nsxiv-ng-speed: {e}");
        ExitCode::from(2)
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Res<Flags> {
        flags(&mut args.iter().map(|a| a.to_string()).collect())
    }

    #[test]
    fn flags_take_finite_numbers_of_at_least_1() {
        let f = parsed(&["--settle-ms", "250", "--repeats", "7"]).unwrap();
        assert_eq!((f.settle, f.repeats, f.limit), (250.0, 7, 5000.0));
        for value in ["inf", "-inf", "nan", "NaN", "0.5", "-3", "fast", "1e400"] {
            assert!(parsed(&["--idle-ms", value]).is_err(), "{value}");
        }
        assert!(parsed(&["--repeats", "2.5"]).is_err());
        assert!(parsed(&["--settle-ms"]).is_err());
        assert!(parsed(&["--fast", "1"]).is_err());
    }
}
