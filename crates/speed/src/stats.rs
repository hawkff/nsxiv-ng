//! The statistics of a case, its row in the table, and the comparison of two tables.

use crate::settle::Frame;

/// The table's header. Scripts read the columns by these names, so they stay.
pub const HEADER: &str = "case\tsamples\tnone\tmedian_ms\tp90_ms\tworst_ms\tgap_ms\tpeak_mib\tlate";

/// The median, the 90th percentile by nearest rank, and the largest of some samples.
#[derive(Debug, PartialEq)]
pub struct Summary {
    pub median: f64,
    pub p90: f64,
    pub worst: f64,
}

/// None without samples.
pub fn summary(samples: &[f64]) -> Option<Summary> {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let worst = *sorted.last()?;
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    Some(Summary {
        median,
        p90: sorted[(n * 9).div_ceil(10) - 1],
        worst,
    })
}

/// What one case measured: the frames, the keys that drew nothing, the highest peak memory of
/// the processes it ran, and the damage that came after a frame had ended, which is late.
pub struct Case {
    pub name: &'static str,
    pub frames: Vec<f64>,
    pub gap_ms: f64,
    pub none: usize,
    pub peak_mib: f64,
    pub late: usize,
}

impl Case {
    pub fn new(name: &'static str) -> Self {
        Case {
            name,
            frames: Vec::new(),
            gap_ms: 0.0,
            none: 0,
            peak_mib: 0.0,
            late: 0,
        }
    }

    /// Counts one key's frame, or None for a key that drew nothing.
    pub fn add(&mut self, frame: Option<Frame>) {
        match frame {
            Some(frame) => {
                self.frames.push(frame.ms);
                self.gap_ms = self.gap_ms.max(frame.gap_ms);
            }
            None => self.none += 1,
        }
    }

    /// The case's row under `HEADER`, with `-` where no frame gives a time.
    pub fn row(&self) -> String {
        let ms = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"));
        let s = summary(&self.frames);
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{}",
            self.name,
            self.frames.len(),
            self.none,
            ms(s.as_ref().map(|s| s.median)),
            ms(s.as_ref().map(|s| s.p90)),
            ms(s.as_ref().map(|s| s.worst)),
            ms(s.as_ref().map(|_| self.gap_ms)),
            self.peak_mib,
            self.late,
        )
    }
}

struct Row {
    samples: usize,
    none: usize,
    median: Option<f64>,
    peak: f64,
    late: usize,
}

fn rows(table: &str) -> Result<Vec<(&str, Row)>, String> {
    let mut lines = table.lines();
    if lines.next() != Some(HEADER) {
        return Err("not a table of nsxiv-ng-speed: the header differs".into());
    }
    let rows = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let bad = || format!("a bad row: {line}");
            // a time or a size is a finite number of at least 0
            let number = |text: &str| {
                text.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or_else(bad)
            };
            let fields: Vec<&str> = line.split('\t').collect();
            let [case, samples, none, median, _, _, _, peak, late] = fields[..] else {
                return Err(bad());
            };
            let row = Row {
                samples: samples.parse().map_err(|_| bad())?,
                none: none.parse().map_err(|_| bad())?,
                median: if median == "-" {
                    None
                } else {
                    Some(number(median)?)
                },
                peak: number(peak)?,
                late: late.parse().map_err(|_| bad())?,
            };
            Ok((case, row))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if rows.is_empty() {
        return Err("a table with no case".into());
    }
    Ok(rows)
}

/// Compares a candidate with a baseline over rounds, each a pair of tables the two made one after
/// the other, which may reduce how much drift of the machine tells them apart. For each case a
/// round gives the ratio of the candidate's median time to the baseline's, and of its peak memory
/// to the baseline's. The candidate fails a case where the median of those ratios is above 1 by
/// more than `margin_percent`; where in some round more of its keys draw nothing, or more of its
/// damage comes late; where in some round its keys, counted with those that drew nothing, are not
/// as many as the baseline's, as when a grid stops paging early; where the baseline has no time, or
/// no memory, to compare with; or where a table of a round lacks a case some table has. Returns the
/// comparison as a table with every round's ratios, and whether every case passed.
pub fn compare(rounds: &[(&str, &str)], margin_percent: f64) -> Result<(String, bool), String> {
    if !(margin_percent.is_finite() && margin_percent >= 0.0) {
        return Err(format!(
            "the margin must be a finite percentage, at least 0: {margin_percent}"
        ));
    }
    let rounds = rounds
        .iter()
        .map(|&(baseline, candidate)| Ok((rows(baseline)?, rows(candidate)?)))
        .collect::<Result<Vec<_>, String>>()?;
    if rounds.is_empty() {
        return Err("no rounds to compare".into());
    }
    // every case of every table, in the order they first appear
    let mut cases: Vec<&str> = Vec::new();
    for (baseline, candidate) in &rounds {
        for (case, _) in baseline.iter().chain(candidate) {
            if !cases.contains(case) {
                cases.push(case);
            }
        }
    }
    let allowed = 1.0 + margin_percent / 100.0;
    let numbered = |name: &str| {
        (1..=rounds.len())
            .map(|r| format!("\t{name}_{r}"))
            .collect::<String>()
    };
    let mut out = format!(
        "case{}\ttime_ratio{}\tmemory_ratio\tresult\n",
        numbered("time"),
        numbered("memory")
    );
    let mut pass = true;
    let ratio = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
    for case in cases {
        let (mut times, mut memory) = (Vec::new(), Vec::new());
        let (mut time_cells, mut memory_cells) = (String::new(), String::new());
        let (mut missing, mut nothing, mut late, mut unknown) = (false, false, false, false);
        let mut keys = false;
        for (baseline, candidate) in &rounds {
            let find = |rows: &[(&str, Row)]| rows.iter().position(|(c, _)| *c == case);
            let (Some(b), Some(c)) = (find(baseline), find(candidate)) else {
                missing = true;
                time_cells += "\t-";
                memory_cells += "\t-";
                continue;
            };
            let (base, cand) = (&baseline[b].1, &candidate[c].1);
            let time = match (base.median, cand.median) {
                (Some(b), Some(c)) if b > 0.0 => Some(c / b),
                _ => None,
            };
            let peak = (base.peak > 0.0).then_some(cand.peak / base.peak);
            unknown |= !base.median.is_some_and(|b| b > 0.0) || peak.is_none();
            nothing |= cand.none > base.none || (base.median.is_some() && cand.median.is_none());
            late |= cand.late > base.late;
            keys |= cand.samples + cand.none != base.samples + base.none;
            times.extend(time);
            memory.extend(peak);
            time_cells += &format!("\t{}", ratio(time));
            memory_cells += &format!("\t{}", ratio(peak));
        }
        let time = summary(&times).map(|s| s.median);
        let peak = summary(&memory).map(|s| s.median);
        let mut faults = Vec::new();
        if missing {
            faults.push("missing");
        }
        if time.is_some_and(|t| t > allowed) {
            faults.push("slower");
        }
        if peak.is_some_and(|m| m > allowed) {
            faults.push("more memory");
        }
        if nothing {
            faults.push("draws nothing");
        }
        if keys {
            faults.push("other keys");
        }
        if late {
            faults.push("draws late");
        }
        if unknown {
            faults.push("no baseline to compare with");
        }
        pass &= faults.is_empty();
        let result = if faults.is_empty() {
            "ok".to_string()
        } else {
            faults.join(", ")
        };
        out += &format!(
            "{case}{time_cells}\t{}{memory_cells}\t{}\t{result}\n",
            ratio(time),
            ratio(peak)
        );
    }
    Ok((out, pass))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_takes_the_middle_the_nearest_rank_and_the_largest() {
        let odd = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(
            summary(&odd),
            Some(Summary {
                median: 3.0,
                p90: 5.0,
                worst: 5.0
            })
        );
        let twenty: Vec<f64> = (1..=20).rev().map(f64::from).collect();
        assert_eq!(
            summary(&twenty),
            Some(Summary {
                median: 10.5,
                p90: 18.0,
                worst: 20.0
            })
        );
        assert_eq!(summary(&[7.0]).map(|s| s.p90), Some(7.0));
        assert_eq!(summary(&[]), None);
    }

    #[test]
    fn a_row_counts_keys_that_drew_nothing_apart_from_frames() {
        let mut case = Case::new("next-cold");
        case.add(Some(Frame {
            ms: 120.0,
            gap_ms: 2.5,
        }));
        case.add(None);
        case.add(Some(Frame {
            ms: 80.0,
            gap_ms: 40.0,
        }));
        case.peak_mib = 211.24;
        case.late = 3;
        assert_eq!(
            case.row(),
            "next-cold\t2\t1\t100.0\t120.0\t120.0\t40.0\t211.2\t3"
        );
        let mut empty = Case::new("pan");
        empty.add(None);
        assert_eq!(empty.row(), "pan\t0\t1\t-\t-\t-\t-\t0.0\t0");
    }

    fn table(rows: &[&str]) -> String {
        std::iter::once(HEADER)
            .chain(rows.iter().copied())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row(case: &str, median: f64, peak: f64) -> String {
        format!("{case}\t20\t0\t{median:.1}\t1\t1\t1\t{peak:.1}\t0")
    }

    #[test]
    fn drift_that_hits_both_runs_of_a_round_cancels() {
        // the machine is twice as slow in the second round, for both
        let rounds = [
            (
                table(&[&row("zoom", 100.0, 200.0)]),
                table(&[&row("zoom", 105.0, 202.0)]),
            ),
            (
                table(&[&row("zoom", 200.0, 200.0)]),
                table(&[&row("zoom", 210.0, 202.0)]),
            ),
            (
                table(&[&row("zoom", 100.0, 200.0)]),
                table(&[&row("zoom", 100.0, 200.0)]),
            ),
        ];
        let pairs: Vec<(&str, &str)> = rounds
            .iter()
            .map(|(b, c)| (b.as_str(), c.as_str()))
            .collect();
        let (out, pass) = compare(&pairs, 10.0).unwrap();
        assert!(pass, "{out}");
        assert_eq!(
            out,
            "case\ttime_1\ttime_2\ttime_3\ttime_ratio\tmemory_1\tmemory_2\tmemory_3\tmemory_ratio\tresult\n\
             zoom\t1.05\t1.05\t1.00\t1.05\t1.01\t1.01\t1.00\t1.01\tok\n"
        );
    }

    fn compared(ratios: &[(f64, f64)], margin: f64) -> (String, bool) {
        let rounds: Vec<(String, String)> = ratios
            .iter()
            .map(|&(time, memory)| {
                (
                    table(&[&row("pan", 100.0, 100.0)]),
                    table(&[&row("pan", 100.0 * time, 100.0 * memory)]),
                )
            })
            .collect();
        let pairs: Vec<(&str, &str)> = rounds
            .iter()
            .map(|(b, c)| (b.as_str(), c.as_str()))
            .collect();
        compare(&pairs, margin).unwrap()
    }

    #[test]
    fn the_median_of_the_rounds_ratios_meets_the_margin() {
        // one slow round of three does not fail the case
        assert!(compared(&[(1.05, 1.0), (1.6, 1.0), (1.08, 1.0)], 10.0).1);
        let (out, pass) = compared(&[(1.2, 1.0), (1.3, 1.0), (1.05, 1.0)], 10.0);
        assert!(!pass);
        assert!(
            out.ends_with("\t1.20\t1.00\t1.00\t1.00\t1.00\tslower\n"),
            "{out}"
        );
        assert!(compared(&[(1.2, 1.0), (1.3, 1.0), (1.05, 1.0)], 21.0).1);
        let (out, pass) = compared(&[(1.0, 1.12), (1.0, 1.11), (1.0, 1.0)], 10.0);
        assert!(!pass);
        assert!(out.ends_with("\tmore memory\n"), "{out}");
    }

    #[test]
    fn keys_that_draw_nothing_or_a_missing_case_fail() {
        let base = table(&[&row("zoom", 100.0, 200.0), &row("pan", 10.0, 200.0)]);
        let cand = table(&["zoom\t0\t20\t-\t-\t-\t-\t200.0\t0"]);
        let (out, pass) = compare(&[(&*base, &*cand)], 10.0).unwrap();
        assert!(!pass);
        assert!(
            out.contains("zoom\t-\t-\t1.00\t1.00\tdraws nothing\n"),
            "{out}"
        );
        assert!(out.contains("pan\t-\t-\t-\t-\tmissing\n"), "{out}");
        // one round of two that draws nothing is enough
        let (_, pass) = compare(&[(&*base, &*base), (&*base, &*cand)], 10.0).unwrap();
        assert!(!pass);
    }

    #[test]
    fn a_margin_that_is_no_finite_percentage_is_refused() {
        let good = table(&[&row("zoom", 100.0, 200.0)]);
        for margin in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert!(compare(&[(&*good, &*good)], margin).is_err(), "{margin}");
        }
        assert!(compare(&[(&*good, &*good)], 0.0).unwrap().1);
    }

    #[test]
    fn more_late_damage_than_the_baseline_fails() {
        let base = table(&["zoom\t20\t0\t100.0\t1\t1\t1\t200.0\t2"]);
        let same = table(&["zoom\t20\t0\t100.0\t1\t1\t1\t200.0\t2"]);
        let later = table(&["zoom\t20\t0\t100.0\t1\t1\t1\t200.0\t3"]);
        assert!(compare(&[(&*base, &*same)], 10.0).unwrap().1);
        let (out, pass) = compare(&[(&*base, &*same), (&*base, &*later)], 10.0).unwrap();
        assert!(!pass);
        assert!(out.ends_with("\tdraws late\n"), "{out}");
    }

    #[test]
    fn a_baseline_without_a_time_or_memory_fails() {
        // neither drew: there is nothing to say the candidate is fine
        let neither = table(&["zoom\t0\t20\t-\t-\t-\t-\t200.0\t0"]);
        let (out, pass) = compare(&[(&*neither, &*neither)], 10.0).unwrap();
        assert!(!pass);
        assert!(out.ends_with("\tno baseline to compare with\n"), "{out}");
        let zero = table(&[&row("zoom", 0.0, 200.0)]);
        let fine = table(&[&row("zoom", 10.0, 200.0)]);
        let (out, pass) = compare(&[(&*zero, &*fine)], 10.0).unwrap();
        assert!(!pass);
        assert!(out.contains("zoom\t-\t-\t1.00"), "{out}");
        let no_memory = table(&[&row("zoom", 10.0, 0.0)]);
        assert!(!compare(&[(&*no_memory, &*fine)], 10.0).unwrap().1);
    }

    #[test]
    fn a_case_any_table_has_must_be_in_every_table() {
        let both = table(&[&row("zoom", 100.0, 200.0), &row("pan", 10.0, 200.0)]);
        let zoom = table(&[&row("zoom", 100.0, 200.0)]);
        // the first baseline lacks pan, a later candidate has it
        let (out, pass) = compare(&[(&*zoom, &*zoom), (&*zoom, &*both)], 10.0).unwrap();
        assert!(!pass);
        assert!(out.contains("zoom\t1.00\t1.00\t1.00"), "{out}");
        assert!(out.ends_with("pan\t-\t-\t-\t-\t-\t-\tmissing\n"), "{out}");
        assert!(
            compare(&[(&*both, &*both), (&*both, &*both)], 10.0)
                .unwrap()
                .1
        );
    }

    #[test]
    fn a_candidate_that_answered_fewer_or_more_keys_fails() {
        // the baseline paged 2 times; the candidate stopped after 1, or ran with other repeats
        let base = table(&["grid-cold-page\t2\t0\t100.0\t1\t1\t1\t200.0\t0"]);
        for other in ["1\t0", "3\t0", "1\t2"] {
            let cand = table(&[&format!(
                "grid-cold-page\t{other}\t100.0\t1\t1\t1\t200.0\t0"
            )]);
            let (out, pass) = compare(&[(&*base, &*base), (&*base, &*cand)], 10.0).unwrap();
            assert!(!pass, "{other}");
            assert!(out.contains("other keys"), "{out}");
        }
        // a key that drew nothing still counts as a key it answered
        let none = table(&["grid-cold-page\t1\t1\t100.0\t1\t1\t1\t200.0\t0"]);
        let (out, _) = compare(&[(&*base, &*none)], 10.0).unwrap();
        assert!(out.ends_with("\tdraws nothing\n"), "{out}");
    }

    #[test]
    fn rows_take_only_finite_numbers_of_at_least_0() {
        let good = table(&[&row("zoom", 100.0, 200.0)]);
        for (median, peak) in [
            ("NaN", "1"),
            ("inf", "1"),
            ("-1.0", "1"),
            ("1", "NaN"),
            ("1", "-inf"),
            ("1", "-0.5"),
        ] {
            let bad = table(&[&format!("zoom\t20\t0\t{median}\t1\t1\t1\t{peak}\t0")]);
            assert!(
                compare(&[(&*good, &*bad)], 10.0).is_err(),
                "{median} {peak}"
            );
            assert!(
                compare(&[(&*bad, &*good)], 10.0).is_err(),
                "{median} {peak}"
            );
        }
        let late = table(&["zoom\t20\t0\t1\t1\t1\t1\t1\t-1"]);
        assert!(compare(&[(&*good, &*late)], 10.0).is_err());
    }

    #[test]
    fn no_rounds_no_case_a_table_without_the_header_or_a_bad_row_is_refused() {
        let good = table(&[&row("zoom", 100.0, 200.0)]);
        assert!(compare(&[], 10.0).is_err());
        assert!(compare(&[(&*good, &*table(&[]))], 10.0).is_err());
        assert!(compare(&[("case\tms\n", &*good)], 10.0).is_err());
        let fast = table(&["zoom\t1\t0\tfast\t1\t1\t1\t1\t0"]);
        assert!(compare(&[(&*good, &*fast)], 10.0).is_err());
        assert!(compare(&[(&*good, &*table(&["zoom\t1"]))], 10.0).is_err());
        // a table of before the late column
        let old = table(&["zoom\t20\t0\t1\t1\t1\t1\t1"]);
        assert!(compare(&[(&*good, &*old)], 10.0).is_err());
    }
}
