//! The calibration: the tool measures its own client, whose delays it knows.

use std::process::Command;

use crate::Flags;
use crate::stats::{Case, summary};
use crate::x::{Display, Res};

/// What the client holds, itself and in its child.
const MIB: u64 = 64;
const CHILD_MIB: u64 = 32;
/// Peak memory above what the client holds: two small Rust processes with their libraries.
// shortcut: a fixed allowance, measured on Debian trixie; widen it where libc or x11rb grow
const MEMORY_SLACK_MIB: f64 = 16.0;

/// The client's delays for a settle time: a tenth and four tenths of it in one step, and six
/// tenths in two, in whole milliseconds down, which give 50, 200 and 300 at 500. A key that waits
/// longer than the limit draws nothing, so the limit must hold the largest with the tolerance.
pub fn delays(settle: f64, limit: f64, tolerance: f64) -> Result<[f64; 3], String> {
    let delays = [0.1, 0.4, 0.6].map(|share| (settle * share).floor());
    if delays[2] + tolerance >= limit {
        return Err(format!(
            "--limit-ms {limit} must be above the calibration's longest delay, {} ms, and the \
             tolerance, {tolerance} ms",
            delays[2]
        ));
    }
    Ok(delays)
}

/// Measures the client repainting a tenth and four tenths of the settle time after a key, in
/// one step, and six tenths after a key in two steps, the first at once. Each must measure no
/// less than its delay and no more than the tolerance above it, every key must draw, and
/// nothing may come late. With its second step one and a half settle times after the first, a
/// key must measure its first step and leave the second late. A key the client ignores must
/// draw nothing, and the peak memory must count the client's child. Prints a table and returns
/// whether all of it held.
pub fn run(flags: &Flags) -> Res<bool> {
    let [short, long, two] = delays(flags.settle, flags.limit, flags.tolerance)?;
    let display = Display::open()?;
    let exe = std::env::current_exe()?;
    println!("case\tset_ms\tsamples\tnone\tmedian_ms\tp90_ms\tworst_ms\tpeak_mib\tlate\tresult");
    let mut pass = true;
    // past one settle time and within two, where the wait after a frame sees it
    let late_step = (flags.settle * 1.5).round();
    let runs = [
        ("one-step", short, 1, "j", flags.repeats),
        ("one-step", long, 1, "j", flags.repeats),
        ("two-step", two, 2, "j", flags.repeats),
        ("late-step", late_step, 2, "j", 3),
        ("nothing", 0.0, 1, "x", 3),
    ];
    for (name, delay, steps, key, samples) in runs {
        let mut command = Command::new(&exe);
        command.args(["paint", &format!("{delay:.0}"), &steps.to_string()]);
        command.args([MIB.to_string(), CHILD_MIB.to_string()]);
        let (mut viewer, _, late) = display.start(&mut command, flags.settle)?;
        let mut case = Case::new(name);
        case.late = late + display.quiet(&viewer, flags.settle)?;
        for _ in 0..samples {
            let (frame, late) = display.key(&viewer, key, flags.settle, flags.limit)?;
            case.add(frame);
            case.late += late;
        }
        case.peak_mib = viewer.peak_mib()?;
        display.quit(viewer)?;

        // the late step's key measures its first step, drawn at once
        let set = if name == "late-step" { 0.0 } else { delay };
        let timed = case
            .frames
            .iter()
            .all(|&ms| ms >= set && ms <= set + flags.tolerance);
        let late_ok = case.late == if name == "late-step" { samples } else { 0 };
        let drew = if key == "x" {
            case.none == samples && late_ok
        } else {
            case.none == 0 && timed && late_ok
        };
        let held = (MIB + CHILD_MIB) as f64;
        let counted = case.peak_mib >= held && case.peak_mib <= held + MEMORY_SLACK_MIB;
        pass &= drew && counted;
        let s = summary(&case.frames);
        let ms = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"));
        println!(
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{}\t{}",
            if key == "x" {
                "-".to_string()
            } else {
                set.to_string()
            },
            case.frames.len(),
            case.none,
            ms(s.as_ref().map(|s| s.median)),
            ms(s.as_ref().map(|s| s.p90)),
            ms(s.as_ref().map(|s| s.worst)),
            case.peak_mib,
            case.late,
            match (drew, counted) {
                (true, true) => "ok",
                (false, true) => "off in time",
                (true, false) => "off in memory",
                (false, false) => "off in time and memory",
            }
        );
    }
    Ok(pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delays_follow_the_settle_time() {
        assert_eq!(delays(500.0, 5000.0, 5.0), Ok([50.0, 200.0, 300.0]));
        assert_eq!(delays(250.0, 5000.0, 5.0), Ok([25.0, 100.0, 150.0]));
        assert_eq!(delays(333.0, 5000.0, 5.0), Ok([33.0, 133.0, 199.0]));
        // the two-step frame stays inside its settle time at any setting
        let [_, _, two] = delays(1.0, 5000.0, 5.0).unwrap();
        assert!(two < 1.0);
    }

    #[test]
    fn a_limit_the_longest_delay_does_not_fit_in_is_refused() {
        assert!(delays(500.0, 305.0, 5.0).is_err());
        assert!(delays(500.0, 306.0, 5.0).is_ok());
        assert!(delays(10_000.0, 5000.0, 5.0).is_err());
    }
}
