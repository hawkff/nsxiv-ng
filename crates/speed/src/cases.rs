//! The cases of the speed bar, run against one viewer.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::Flags;
use crate::settle::Frame;
use crate::stats::{Case, HEADER};
use crate::x::{Display, Res, Viewer};

/// Every case runs in a window this large, `-g`, which the screen must hold.
const GEOMETRY: &str = "3840x2160";

fn listed(dir: &Path) -> Res<Vec<PathBuf>> {
    let mut files = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .map(|entry| Ok(entry?.path()))
        .collect::<Res<Vec<_>>>()?;
    files.sort();
    if files.is_empty() {
        return Err(format!("{} holds no files", dir.display()).into());
    }
    Ok(files)
}

/// Copies the sources into a new folder, under names in their order, so the viewer has never
/// seen them, whatever it caches by path or by inode.
fn copies<'a>(dir: &Path, sources: impl Iterator<Item = &'a PathBuf>) -> Res<Vec<PathBuf>> {
    fs::create_dir(dir)?;
    sources
        .enumerate()
        .map(|(i, source)| {
            let extension = source.extension().and_then(|e| e.to_str()).unwrap_or("img");
            let copy = dir.join(format!("{i:05}.{extension}"));
            fs::copy(source, &copy)?;
            Ok(copy)
        })
        .collect()
}

struct Bench<'a> {
    display: Display,
    flags: &'a Flags,
    viewer: &'a [String],
    config: PathBuf,
}

impl Bench<'_> {
    /// Starts the viewer; the late damage after its first frame counts for `case`.
    fn start(
        &self,
        options: &[&str],
        files: &[PathBuf],
        cache: &Path,
        case: &mut Case,
    ) -> Res<(Viewer, Frame)> {
        let mut command = Command::new(&self.viewer[0]);
        command
            .args(&self.viewer[1..])
            .args(["-g", GEOMETRY])
            .args(options)
            .args(files)
            .env("XDG_CACHE_HOME", cache)
            .env("XDG_CONFIG_HOME", &self.config)
            .stdout(Stdio::null());
        let (viewer, first, late) = self.display.start(&mut command, self.flags.settle)?;
        case.late += late;
        Ok((viewer, first))
    }

    /// Lets the viewer finish what it does after a frame, as decoding ahead.
    /// What it draws meanwhile is late, for `case`.
    fn idle(&self, viewer: &Viewer, case: &mut Case) -> Res<()> {
        thread::sleep(Duration::from_secs_f64(self.flags.idle / 1000.0));
        case.late += self.display.quiet(viewer, self.flags.settle)?;
        Ok(())
    }

    /// Presses a key and returns its frame; its late damage counts for `case`.
    fn key(&self, viewer: &Viewer, name: &str, case: &mut Case) -> Res<Option<Frame>> {
        let (frame, late) = self
            .display
            .key(viewer, name, self.flags.settle, self.flags.limit)?;
        case.late += late;
        Ok(frame)
    }

    /// Presses a key, and its frame is a sample of `case`.
    fn sample(&self, viewer: &Viewer, name: &str, case: &mut Case) -> Res<()> {
        let frame = self.key(viewer, name, case)?;
        case.add(frame);
        Ok(())
    }

    /// Quits the viewer and returns its peak memory, read before it was asked to quit.
    fn finish(&self, mut viewer: Viewer) -> Res<f64> {
        let peak = viewer.peak_mib()?;
        self.display.quit(viewer)?;
        Ok(peak)
    }

    /// Pages down the grid until a page draws nothing or the same pixels as the one before.
    /// That last key is no sample.
    fn pages(&self, viewer: &Viewer, case: &mut Case, most: usize) -> Res<()> {
        let mut shown = self.display.digest(viewer)?;
        for _ in 0..most {
            let Some(frame) = self.key(viewer, "C-j", case)? else {
                break;
            };
            let now = self.display.digest(viewer)?;
            if now == shown {
                break;
            }
            shown = now;
            case.add(Some(frame));
        }
        Ok(())
    }

    fn cases(&self, scratch: &Path, fixtures: &Path) -> Res<Vec<Case>> {
        let photos = listed(&fixtures.join("photos"))?;
        let grid = listed(&fixtures.join("grid"))?;
        let big = fixtures.join("big.jpg");
        let repeats = self.flags.repeats;
        let cache = scratch.join("cache");
        fs::create_dir(&cache)?;
        let mut cases = Vec::new();
        let done = |case: &Case| eprintln!("{}", case.row());
        eprintln!("{HEADER}");

        // `]` moves ten files, past where decoding ahead reaches, onto a photo the viewer has
        // not shown; the files between are small images
        let mut cold = Case::new("next-cold");
        let files = (0..=10 * repeats).map(|i| match i % 10 {
            0 => &photos[i / 10 % photos.len()],
            _ => &grid[i % grid.len()],
        });
        let files = copies(&scratch.join("cold"), files)?;
        let (viewer, _) = self.start(&[], &files, &cache, &mut cold)?;
        self.idle(&viewer, &mut cold)?;
        for _ in 0..repeats {
            self.sample(&viewer, "]", &mut cold)?;
        }
        cold.peak_mib = self.finish(viewer)?;
        done(&cold);
        cases.push(cold);

        let mut ahead = Case::new("next-ahead");
        let files = (0..=repeats).map(|i| &photos[i % photos.len()]);
        let files = copies(&scratch.join("ahead"), files)?;
        let (viewer, _) = self.start(&[], &files, &cache, &mut ahead)?;
        for _ in 0..repeats {
            self.idle(&viewer, &mut ahead)?;
            self.sample(&viewer, "j", &mut ahead)?;
        }
        ahead.peak_mib = self.finish(viewer)?;
        done(&ahead);
        cases.push(ahead);

        // each run starts cold with a cache of its own, then warm with what the cold one left
        let mut grids = [
            "grid-cold-start",
            "grid-cold-page",
            "grid-warm-start",
            "grid-warm-page",
        ]
        .map(Case::new);
        for run in 0..self.flags.grid_runs {
            let cache = scratch.join(format!("grid-cache-{run}"));
            fs::create_dir(&cache)?;
            for start in [0, 2] {
                let (viewer, first) = self.start(&["-t"], &grid, &cache, &mut grids[start])?;
                grids[start].add(Some(first));
                self.pages(&viewer, &mut grids[start + 1], grid.len())?;
                let peak = self.finish(viewer)?;
                for case in &mut grids[start..start + 2] {
                    case.peak_mib = case.peak_mib.max(peak);
                }
            }
        }
        grids.iter().for_each(done);
        cases.extend(grids);

        let mut zoom = Case::new("zoom");
        let mut pan = Case::new("pan");
        let (viewer, _) = self.start(&[], &[big], &cache, &mut zoom)?;
        self.idle(&viewer, &mut zoom)?;
        // from the fitted zoom in and out through the zoom levels
        for key in ["+", "+", "+", "+", "-", "-", "-", "-"]
            .iter()
            .cycle()
            .take(repeats)
        {
            self.sample(&viewer, key, &mut zoom)?;
        }
        // at 100% a 50-megapixel photo is larger than the window both ways, and the square of
        // two steps a side stays inside it from the center
        self.key(&viewer, "=", &mut pan)?;
        for key in ["l", "l", "d", "d", "h", "h", "u", "u"]
            .iter()
            .cycle()
            .take(repeats)
        {
            self.sample(&viewer, key, &mut pan)?;
        }
        let peak = self.finish(viewer)?;
        zoom.peak_mib = peak;
        pan.peak_mib = peak;
        done(&zoom);
        done(&pan);
        cases.extend([zoom, pan]);
        Ok(cases)
    }
}

/// Makes the scratch folder, only this user's: where it is already there, someone may have
/// planted links in it that the copies would write through, so that is an error.
fn make_scratch(dir: &Path) -> Res<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(())
}

/// Runs every case against the viewer command, over the fixtures `ci/speed.sh` makes: a folder
/// `photos` of large photos, a folder `grid` of 1,000 small images, and `big.jpg` of 50
/// megapixels. Returns the table.
pub fn run(flags: &Flags, fixtures: &Path, viewer: &[String]) -> Res<String> {
    let fixtures = fixtures.canonicalize()?;
    let scratch = std::env::temp_dir().join(format!("nsxiv-ng-speed-{}", std::process::id()));
    make_scratch(&scratch)?;
    let config = scratch.join("config");
    fs::create_dir(&config)?;
    let bench = Bench {
        display: Display::open()?,
        flags,
        viewer,
        config,
    };
    let cases = bench.cases(&scratch, &fixtures);
    fs::remove_dir_all(&scratch)?;
    let mut table = format!("{HEADER}\n");
    for case in cases? {
        table += &case.row();
        table += "\n";
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_scratch_folder_that_is_there_already_is_refused() {
        let there = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(make_scratch(&there).is_err());
        // a new one is this user's alone
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let new = crate_dir.join(format!("scratch-test-{}", std::process::id()));
        make_scratch(&new).unwrap();
        let mode = fs::metadata(&new).unwrap().permissions().mode() & 0o777;
        fs::remove_dir(&new).unwrap();
        assert_eq!(mode, 0o700);
    }
}
