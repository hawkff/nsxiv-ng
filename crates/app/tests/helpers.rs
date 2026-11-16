//! The decoding helpers, run as the nsxiv-ng binary runs them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nsxiv_ng_decode::DecodeError;
use nsxiv_ng_helper::Helpers;

const PLENTY: u64 = 1 << 30;
const PROGRAM: &str = env!("CARGO_BIN_EXE_nsxiv-ng");

// a billion octaves of noise, which no decode finishes
const NOISE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><filter id="f"><feTurbulence baseFrequency="0.05" numOctaves="1000000000"/></filter><rect width="64" height="64" filter="url(#f)"/></svg>"#;

// macOS has no SOCK_CLOEXEC: a socket pair made while another test spawns a process can stay
// open in that process, as a stand-in viewer, which closes no descriptor it inherits, and keep
// a crashed helper's end from closing; the tests that run helpers and those that spawn take
// turns
static TURNS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp(name: &str, content: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nsxiv-ng-helpers-{}-{name}", std::process::id()));
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn svg_decodes_in_a_helper_that_a_crash_or_a_stall_ends() {
    let _turn = TURNS.lock().unwrap_or_else(|e| e.into_inner());
    let helpers = Helpers::new(PathBuf::from(PROGRAM), 2, PLENTY, Duration::from_secs(3));
    let first = |path: &Path| {
        helpers
            .decode(path, PLENTY)
            .map(|image| (image.width, image.height, image.rgba[..4].to_vec()))
    };
    let shapes = temp(
        "shapes.svg",
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="2"><rect width="4" height="2" fill="red"/></svg>"#,
    );
    assert_eq!(first(&shapes), Ok((4, 2, vec![255, 0, 0, 255])));
    // 30,000 masks that each name the next run a helper's stack out
    let mut chain =
        String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><defs>"#);
    for i in 0..30_000 {
        chain += &format!(
            r#"<mask id="m{i}" mask="url(#m{})"><rect width="1" height="1" fill="white"/></mask>"#,
            i + 1
        );
    }
    chain += r#"</defs><rect width="1" height="1" mask="url(#m0)"/></svg>"#;
    let chain = temp("chain.svg", chain.as_bytes());
    assert_eq!(
        helpers.decode(&chain, PLENTY).map(|image| image.width),
        Err(DecodeError::Unreadable("the decoder crashed".to_owned()))
    );
    // the noise runs past the timeout
    let noise = temp("noise.svg", NOISE);
    assert_eq!(
        helpers.decode(&noise, PLENTY).map(|image| image.width),
        Err(DecodeError::Unreadable(
            "the decoder ran out of time".to_owned()
        ))
    );
    assert_eq!(
        first(&shapes),
        Ok((4, 2, vec![255, 0, 0, 255])),
        "a new helper takes the next file"
    );
    // a format that needs no helper decodes in this process
    let png =
        std::env::temp_dir().join(format!("nsxiv-ng-helpers-{}-blue.png", std::process::id()));
    image::RgbaImage::from_pixel(3, 1, image::Rgba([0, 0, 255, 255]))
        .save(&png)
        .unwrap();
    assert_eq!(first(&png), Ok((3, 1, vec![0, 0, 255, 255])));
    for path in [shapes, chain, noise, png] {
        std::fs::remove_file(path).unwrap();
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures")
        .join(name)
}

// Linux decodes them in a helper, through libheif; macOS in this process, through ImageIO, so
// there a pool whose program cannot start decodes them still
#[cfg(target_os = "linux")]
const HEIF_PROGRAM: &str = PROGRAM;
#[cfg(target_os = "macos")]
const HEIF_PROGRAM: &str = "/nonexistent";

#[test]
fn heic_and_avif_decode_with_their_colors_alpha_and_cap() {
    let helpers = Helpers::new(
        PathBuf::from(HEIF_PROGRAM),
        2,
        PLENTY,
        Duration::from_secs(10),
    );
    // red on top, blue at the bottom
    for name in ["gradient.heic", "gradient.avif"] {
        let image = helpers.decode(&fixture(name), PLENTY).unwrap();
        assert_eq!((image.width, image.height), (320, 200), "{name}");
        let at = |x: u32, y: u32| {
            let i = ((y * image.width + x) * 4) as usize;
            [
                image.rgba[i],
                image.rgba[i + 1],
                image.rgba[i + 2],
                image.rgba[i + 3],
            ]
        };
        let (top, bottom) = (at(160, 0), at(160, 199));
        assert!(
            top[0] > 200 && top[2] < 60 && bottom[2] > 200 && bottom[0] < 60 && top[3] == 255,
            "{name}: {top:?} {bottom:?}"
        );
    }
    // a muted red in Display P3, which sRGB shows more saturated, as rgb(215, 93, 97): named
    // by nclx code points in the AVIF, and by an ICC profile in the HEIC
    for name in ["p3.avif", "p3.heic"] {
        let red = helpers.decode(&fixture(name), PLENTY).unwrap();
        let pixel = &red.rgba[..4];
        assert!(
            pixel[0] >= 210 && pixel[1] <= 96 && pixel[3] == 255,
            "{name}: {pixel:?}"
        );
    }
    // a green disk, transparent around it
    let disk = helpers.decode(&fixture("alpha.heic"), PLENTY).unwrap();
    let alpha = |x: u32, y: u32| disk.rgba[((y * disk.width + x) * 4 + 3) as usize];
    assert!(
        alpha(80, 80) > 245 && alpha(2, 2) < 10,
        "{} {}",
        alpha(80, 80),
        alpha(2, 2)
    );
    // the cap holds before libheif or ImageIO decodes a pixel, counting what each holds beside
    // the copy: these RGBA pixels and their pyramid fit in 500 kB, and the decode does not
    assert_eq!(
        helpers
            .decode(&fixture("gradient.heic"), 500_000)
            .map(|i| i.width),
        Err(DecodeError::TooLarge {
            size: Some((320, 200))
        })
    );
}

#[test]
fn a_heic_thumbnail_covers_the_side() {
    let helpers = Helpers::new(
        PathBuf::from(HEIF_PROGRAM),
        1,
        PLENTY,
        Duration::from_secs(10),
    );
    let gradient = fixture("gradient.heic");
    let size = |path: &Path, side| {
        helpers
            .thumbnail(path, side, PLENTY, None)
            .map(|i| (i.width, i.height))
    };
    // on Linux the HEIC's 64 × 40 thumbnail covers a side of 64, and not one of 65, which the
    // whole image does; on macOS ImageIO scales the image to the side
    assert_eq!(size(&gradient, 64), Ok((64, 40)));
    #[cfg(target_os = "linux")]
    assert_eq!(size(&gradient, 65), Ok((320, 200)));
    #[cfg(target_os = "macos")]
    assert_eq!(size(&gradient, 65), Ok((65, 41)));
    let whole = helpers
        .decode(&gradient, PLENTY)
        .map(|i| (i.width, i.height));
    assert_eq!(whole, Ok((320, 200)));
    // a file of another format gives its whole image
    assert_eq!(size(&fixture("gradient.png"), 64), Ok((320, 200)));
}

#[test]
fn v_reports_a_sandbox_that_stops_what_a_helper_must_not_do() {
    let _turn = TURNS.lock().unwrap_or_else(|e| e.into_inner());
    let output = Command::new(PROGRAM).arg("-v").output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    assert!(
        lines
            .next()
            .is_some_and(|line| line.starts_with("nsxiv-ng ")),
        "{text}"
    );
    // the probe prints this line only once every attempt it makes failed: on Linux open,
    // exec, sockets and io_uring, ended with the libheif it loaded; on macOS a read, a write,
    // fork, exec, TCP and UNIX connects, a mach lookup, an IOKit open, a sysctl and a signal
    let sandbox = lines.next().unwrap_or_default();
    #[cfg(target_os = "linux")]
    assert!(
        sandbox.starts_with("sandbox: seccomp, landlock ") && sandbox.contains("; libheif 1."),
        "{text}"
    );
    #[cfg(target_os = "macos")]
    assert_eq!(sandbox, "sandbox: seatbelt", "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_heic_thumbnail_waits_for_room_in_its_budget() {
    use std::sync::mpsc;
    // a budget a decode in flight fills
    let budget = nsxiv_ng_decode::DecodeBudget::new(1 << 20, Duration::from_secs(30));
    let held = budget.share();
    held.take(1 << 20);
    let helpers = Helpers::new(
        PathBuf::from(HEIF_PROGRAM),
        1,
        PLENTY,
        Duration::from_secs(10),
    );
    let gradient = fixture("gradient.heic");
    let (started, starts) = mpsc::channel();
    let (sender, done) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let share = budget.share();
            started.send(()).unwrap();
            let thumbnail = helpers.thumbnail(&gradient, 64, PLENTY, Some(&share));
            sender.send(thumbnail.map(|i| i.width)).unwrap();
        });
        // the silence counts once the decode is about to start, not before its thread runs
        starts.recv().unwrap();
        let waited = done.recv_timeout(Duration::from_millis(500));
        assert!(waited.is_err(), "decoded with no room: {waited:?}");
        drop(held);
        let decoded = done.recv_timeout(Duration::from_secs(10));
        assert_eq!(decoded, Ok(Ok(64)), "once the room is given back");
    });
}

// The Linux smoke script shows a helper ends with a viewer killed mid-decode; here the test
// binary itself, run again, is the viewer
#[cfg(target_os = "macos")]
#[test]
fn a_helper_ends_with_a_viewer_killed_mid_decode() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Child;
    use std::time::Instant;
    const VIEWER: &str = "NSXIV_NG_HELPERS_STAND_IN_VIEWER";
    const MARK: &str = "NSXIV_NG_HELPERS_STAND_IN_MARK";
    const SMALL: &str = "NSXIV_NG_HELPERS_STAND_IN_SMALL";
    if let Some(noise) = std::env::var_os(VIEWER) {
        // the stand-in viewer: a small SVG first, which a helper that locked down answers, then
        // the mark, then the noise, whose decode never returns while its helper lives
        let helpers = Helpers::new(PathBuf::from(PROGRAM), 1, PLENTY, Duration::from_secs(600));
        let small = std::env::var_os(SMALL).unwrap();
        if helpers.decode(Path::new(&small), PLENTY).is_err() {
            std::process::exit(3);
        }
        std::fs::write(std::env::var_os(MARK).unwrap(), b"").unwrap();
        let _ = helpers.decode(Path::new(&noise), PLENTY);
        std::process::exit(4);
    }
    // a process ended, and reaped, on every way out of the test
    struct Reaped(Option<Child>);
    impl Drop for Reaped {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    // ends the helper where the test leaves while it may still run; None once it was seen gone,
    // as its pid may be another process's from then on
    struct Killed(Option<String>);
    impl Drop for Killed {
        fn drop(&mut self) {
            if let Some(pid) = &self.0 {
                let _ = Command::new("kill").args(["-9", pid]).status();
            }
        }
    }
    // a process's CPU time in seconds, from ps's m:ss.cc
    let cpu = |pid: &str| -> Option<f64> {
        let out = Command::new("ps")
            .args(["-o", "time=", "-p", pid])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let (minutes, seconds) = text.split_once(':')?;
        Some(minutes.parse::<f64>().ok()? * 60.0 + seconds.parse::<f64>().ok()?)
    };
    let _turn = TURNS.lock().unwrap_or_else(|e| e.into_inner());
    let noise = temp("ended.svg", NOISE);
    let small = temp(
        "small.svg",
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="red"/></svg>"#,
    );
    let mark = std::env::temp_dir().join(format!("nsxiv-ng-helpers-{}-mark", std::process::id()));
    let _ = std::fs::remove_file(&mark);
    let name = "a_helper_ends_with_a_viewer_killed_mid_decode";
    let mut viewer = Reaped(Some(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--test-threads=1"])
            .env(VIEWER, &noise)
            .env(MARK, &mark)
            .env(SMALL, &small)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let pid = viewer.0.as_ref().unwrap().id().to_string();
    // the mark: its helper locked down and answered, and the noise goes next
    let started = Instant::now();
    while !mark.exists() {
        let status = viewer.0.as_mut().unwrap().try_wait().unwrap();
        assert!(
            status.is_none(),
            "the viewer ended before its noise: {status:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the viewer left no mark"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let children = Command::new("pgrep").args(["-P", &pid]).output().unwrap();
    let children = String::from_utf8_lossy(&children.stdout).into_owned();
    let helper = children
        .split_whitespace()
        .next()
        .expect("a helper")
        .to_owned();
    let mut guard = Killed(Some(helper.clone()));
    // the decode under way: the helper's CPU time grows as the noise takes it
    let before = cpu(&helper).expect("the helper's CPU time");
    while cpu(&helper).is_none_or(|now| now < before + 0.3) {
        let status = viewer.0.as_mut().unwrap().try_wait().unwrap();
        assert!(status.is_none(), "the viewer ended mid-decode: {status:?}");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the noise took no CPU"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut child = viewer.0.take().unwrap();
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "the viewer died of the kill: {status:?}"
    );
    let killed = Instant::now();
    let alive = || {
        Command::new("kill")
            .args(["-0", &helper])
            .status()
            .unwrap()
            .success()
    };
    while alive() && killed.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let outlived = alive();
    if !outlived {
        guard.0 = None;
    }
    for path in [noise, small, mark] {
        let _ = std::fs::remove_file(path);
    }
    assert!(!outlived, "the helper outlived its viewer by 5 s");
}
