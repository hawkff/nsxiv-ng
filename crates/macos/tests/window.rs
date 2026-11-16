//! The macOS frontend, run as the viewer runs it: each scenario is a child process of this
//! binary that starts the frontend on images it makes, and drives it from a thread of its own
//! with events posted to the application, reading the window's frame and title and the view's
//! pixels back through AppKit. The child's exit status and what `-o` printed are checked here,
//! from outside. With `NSXIV_NG_SHOTS` set to a folder, the frames read back are saved there.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    macos::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::io::Write;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use dispatch2::DispatchQueue;
    use nsxiv_ng_core::config::Config;
    use nsxiv_ng_core::file_list::{FileEntry, FileList};
    use nsxiv_ng_core::options::{self, Command as Parsed};
    use nsxiv_ng_helper as helper;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, MessageReceiver, NSObject, Sel};
    use objc2::{AnyThread, ClassType, MainThreadMarker, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
        NSApplicationActivationPolicy, NSApplicationTerminateReply, NSBitmapImageFileType,
        NSBitmapImageRep, NSColorSpace, NSDeviceRGBColorSpace, NSEvent, NSEventModifierFlags,
        NSEventType, NSPasteboard, NSPasteboardTypePNG, NSScreen, NSTextField, NSWindow,
        NSWindowButton, NSWindowStyleMask, NSWindowTabbingMode,
    };
    use objc2_core_foundation::CGPoint;
    use objc2_core_graphics::{CGEvent, CGEventFlags, CGScrollEventUnit};
    use objc2_foundation::{
        NSArray, NSDictionary, NSNotification, NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
        NSURL,
    };

    /// How long a scenario may take before it counts as hung.
    const LIMIT: Duration = Duration::from_secs(40);

    struct Scenario {
        name: &'static str,
        /// The viewer's arguments, given the folder of the images.
        args: fn(&Path) -> Vec<String>,
        /// What the child does once the frontend runs; it ends the viewer or fails.
        drive: fn(&Path),
        /// The status the child must end with, the lines `-o` must print, and a line that
        /// must be on stderr.
        status: i32,
        printed: fn(&Path) -> Vec<String>,
        warned: Option<&'static str>,
    }

    fn path(dir: &Path, name: &str) -> String {
        dir.join(name).to_string_lossy().into_owned()
    }

    fn none(_: &Path) -> Vec<String> {
        Vec::new()
    }

    /// The scenarios that start with the status bar hidden, as the viewer does by default; the
    /// others show it from the start, as `show_bar` in the config does, as what they check lies
    /// in it or beside it.
    const BAR_HIDDEN: &[&str] = &["bar-hidden", "edit-bar-hidden"];

    const SCENARIOS: &[Scenario] = &[
        Scenario {
            name: "bar-hidden",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: bar_hidden,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "edit-bar-hidden",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: edit_bar_hidden,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "first-frame",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: first_frame,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "first-frame-slow",
            args: |dir| vec![path(dir, "7-slow.ppm")],
            drive: first_frame_slow,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "keys",
            args: |dir| {
                vec![
                    "-o".into(),
                    path(dir, "1-red-blue.ppm"),
                    path(dir, "2-green.ppm"),
                ]
            },
            drive: keys,
            status: 0,
            printed: |dir| vec![path(dir, "1-red-blue.ppm")],
            warned: None,
        },
        Scenario {
            name: "bar-alone",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: bar_alone,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "backing-scale",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: backing_scale,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "fit",
            args: |dir| vec![path(dir, "3-large.ppm"), path(dir, "2-green.ppm")],
            drive: fit,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "fullscreen-fit",
            args: |dir| {
                let files = ["1-red-blue.ppm", "2-green.ppm", "4-unreadable.png"];
                files.iter().map(|file| path(dir, file)).collect()
            },
            drive: fullscreen_fit,
            status: 0,
            printed: none,
            warned: Some("4-unreadable.png: "),
        },
        Scenario {
            name: "grid",
            args: |dir| {
                vec![
                    "-t".into(),
                    path(dir, "1-red-blue.ppm"),
                    path(dir, "2-green.ppm"),
                ]
            },
            drive: grid,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "appearance",
            args: |dir| vec![path(dir, "4-unreadable.png")],
            drive: appearance,
            status: 0,
            printed: none,
            warned: Some("4-unreadable.png: "),
        },
        Scenario {
            name: "grid-placeholder",
            args: |dir| {
                vec![
                    "-t".to_owned(),
                    path(dir, "4-unreadable.png"),
                    path(dir, "1-red-blue.ppm"),
                ]
            },
            drive: grid_placeholder,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "appearance-config",
            args: |dir| {
                let config = "background = \"#123456\"\n";
                std::fs::write(dir.join("config/nsxiv-ng/config.toml"), config).unwrap();
                vec![path(dir, "4-unreadable.png")]
            },
            drive: appearance_config,
            status: 0,
            printed: none,
            warned: Some("4-unreadable.png: "),
        },
        Scenario {
            name: "title-bar",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: title_bar,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "user-resize",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: user_resize,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "zoom-button",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: zoom_button,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "unreadable",
            args: |dir| vec![path(dir, "4-unreadable.png")],
            drive: unreadable,
            status: 0,
            printed: none,
            warned: Some("4-unreadable.png: "),
        },
        Scenario {
            name: "pointer",
            args: |dir| vec![path(dir, "1-red-blue.ppm"), path(dir, "2-green.ppm")],
            drive: pointer,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "trackpad",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: trackpad,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "zoom-at-pointer",
            args: |dir| vec![path(dir, "10-red-blue-sideways.ppm")],
            drive: zoom_at_pointer,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "joined",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: joined,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "pan-as-whole",
            args: |dir| vec![path(dir, "11-detail.ppm")],
            drive: pan_as_whole,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "open-events",
            args: none,
            drive: open_events,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "trackpad-sideways",
            args: |dir| vec![path(dir, "10-red-blue-sideways.ppm")],
            drive: trackpad_sideways,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "trackpad-grid",
            args: |dir| {
                let mut args = vec!["-t".to_owned()];
                args.extend((0..40).map(|i| path(dir, &format!("grid/{i:02}.ppm"))));
                args
            },
            drive: trackpad_grid,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "grid-control-scroll",
            args: |dir| {
                let mut args = vec!["-t".to_owned()];
                args.extend((0..40).map(|i| path(dir, &format!("grid/{i:02}.ppm"))));
                args
            },
            drive: grid_control_scroll,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "resize",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: resize,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "slideshow",
            args: |dir| {
                vec![
                    "-S".into(),
                    "1".into(),
                    path(dir, "1-red-blue.ppm"),
                    path(dir, "2-green.ppm"),
                ]
            },
            drive: slideshow,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "watch",
            args: |dir| vec![path(dir, "5-watched.ppm")],
            drive: watch,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "watch-burst",
            args: |dir| vec![path(dir, "5-watched.ppm")],
            drive: watch_burst,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "watch-folder-replaced",
            args: |dir| vec![path(dir, "replaced/a.ppm"), path(dir, "replaced/b.ppm")],
            drive: watch_folder_replaced,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "watch-link",
            args: |dir| vec![path(dir, "8-link.ppm")],
            drive: watch_link,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "watch-link-elsewhere",
            args: |dir| vec![path(dir, "9-link.ppm")],
            drive: watch_link_elsewhere,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "close",
            args: |dir| vec!["-o".into(), path(dir, "1-red-blue.ppm")],
            drive: close,
            status: 0,
            printed: |dir| vec![path(dir, "1-red-blue.ppm")],
            warned: None,
        },
        Scenario {
            name: "cmd-q",
            args: |dir| vec!["-o".into(), path(dir, "1-red-blue.ppm")],
            drive: cmd_q,
            status: 0,
            printed: |dir| vec![path(dir, "1-red-blue.ppm")],
            warned: None,
        },
        Scenario {
            name: "terminate",
            args: |dir| vec!["-o".into(), path(dir, "1-red-blue.ppm")],
            drive: terminate,
            status: 0,
            printed: |dir| vec![path(dir, "1-red-blue.ppm")],
            warned: None,
        },
        Scenario {
            name: "sigint",
            args: |dir| vec!["-o".into(), path(dir, "1-red-blue.ppm")],
            drive: |_| signal(libc::SIGINT),
            status: 0,
            printed: |dir| vec![path(dir, "1-red-blue.ppm")],
            warned: None,
        },
        Scenario {
            name: "sigterm",
            args: |dir| vec!["-o".into(), path(dir, "1-red-blue.ppm")],
            drive: |_| signal(libc::SIGTERM),
            status: 0,
            printed: |dir| vec![path(dir, "1-red-blue.ppm")],
            warned: None,
        },
        Scenario {
            name: "sighup",
            args: |dir| vec!["-o".into(), path(dir, "1-red-blue.ppm")],
            drive: |_| signal(libc::SIGHUP),
            status: 0,
            printed: |dir| vec![path(dir, "1-red-blue.ppm")],
            warned: None,
        },
        Scenario {
            name: "key-handler",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: key_handler,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "edit",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: edit,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "edit-keys",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: edit_takes_its_keys,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "edit-field-return",
            args: |dir| vec![path(dir, "1-red-blue.ppm")],
            drive: edit_field_return,
            status: 0,
            printed: none,
            warned: None,
        },
        Scenario {
            name: "edit-save",
            args: |dir| vec![path(dir, "edit/1-red-blue.ppm")],
            drive: edit_save,
            status: 0,
            printed: none,
            warned: None,
        },
    ];

    pub(super) fn main() -> ExitCode {
        // a decoding helper, or the probe of its sandbox, as the viewer's own main answers
        // them: a child gives this binary as the program its helpers run as
        let mut flags = std::env::args_os().skip(1);
        match flags.next() {
            Some(flag) if flag == helper::SERVE => {
                return match flags.next().and_then(|cap| cap.to_str()?.parse().ok()) {
                    Some(cap) => helper::serve(cap),
                    None => ExitCode::FAILURE,
                };
            }
            Some(flag) if flag == helper::PROBE => return helper::probe(),
            _ => {}
        }
        let args: Vec<String> = std::env::args().collect();
        if let Some(at) = args.iter().position(|a| a == "--scenario") {
            child(&args[at + 1], &args[at + 2], &args[at + 3..]);
        }
        let Some(mtm) = MainThreadMarker::new() else {
            panic!("the scenarios start from the main thread");
        };
        if NSScreen::screens(mtm).is_empty() {
            println!("window: no screen, so no scenario ran");
            return ExitCode::SUCCESS;
        }
        let only = args.get(1).filter(|a| !a.starts_with('-'));
        let dir = std::env::temp_dir().join(format!("nsxiv-ng-window-{}", std::process::id()));
        let screen = NSScreen::mainScreen(mtm).unwrap().visibleFrame().size;
        let mut failed = Vec::new();
        for scenario in SCENARIOS
            .iter()
            .filter(|s| only.is_none_or(|o| o == s.name))
        {
            images(&dir, (screen.width, screen.height));
            match parent(scenario, &dir) {
                Ok(()) => println!("window: {} ... ok", scenario.name),
                Err(why) => {
                    println!("window: {} ... FAILED: {why}", scenario.name);
                    failed.push(scenario.name);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(failed.is_empty(), "scenarios failed: {failed:?}");
        ExitCode::SUCCESS
    }

    // The images the scenarios show, netpbm, whose colors are exact; the one a scenario
    // changes made anew for each, and no key-handler. `screen` is the size of the screen's
    // visible frame, which the large image is larger than both ways.
    fn images(dir: &Path, screen: (f64, f64)) {
        let _ = std::fs::remove_dir_all(dir.join("config"));
        let _ = std::fs::remove_dir_all(dir.join("cache"));
        std::fs::create_dir_all(dir.join("config/nsxiv-ng/exec")).unwrap();
        let ppm =
            |name: &str, width: usize, height: usize, color: &dyn Fn(usize, usize) -> [u8; 3]| {
                let changed = ["5-watched.ppm", "target/8-target.ppm"].contains(&name);
                if !changed && dir.join(name).exists() {
                    return;
                }
                let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
                for y in 0..height {
                    for x in 0..width {
                        bytes.extend_from_slice(&color(x, y));
                    }
                }
                std::fs::write(dir.join(name), bytes).unwrap();
            };
        // red above, blue below, which tells the frame's way up
        ppm("1-red-blue.ppm", 300, 200, &|_, y| {
            if y < 100 { [255, 0, 0] } else { [0, 0, 255] }
        });
        ppm("2-green.ppm", 200, 100, &|_, _| [0, 255, 0]);
        // detail in every pixel, which a frame moved a pixel wrong would show
        ppm("11-detail.ppm", 300, 200, &|x, y| {
            [
                (x * 7 + y * 3) as u8,
                (y * 11 + x) as u8,
                ((x ^ y) * 5) as u8,
            ]
        });
        // red on the left, blue on the right
        ppm("10-red-blue-sideways.ppm", 300, 200, &|x, _| {
            if x < 150 { [255, 0, 0] } else { [0, 0, 255] }
        });
        let large = |side: f64| (side * 1.5) as usize;
        ppm("3-large.ppm", large(screen.0), large(screen.1), &|_, _| {
            [128, 128, 128]
        });
        std::fs::write(dir.join("4-unreadable.png"), b"not a picture").unwrap();
        ppm("5-watched.ppm", 300, 200, &|_, _| [255, 0, 0]);
        ppm("6-replacement.ppm", 300, 200, &|_, _| [0, 0, 255]);
        // a link to a file in another folder
        std::fs::create_dir_all(dir.join("target")).unwrap();
        ppm("target/8-target.ppm", 300, 200, &|_, _| [255, 0, 0]);
        let _ = std::os::unix::fs::symlink("target/8-target.ppm", dir.join("8-link.ppm"));
        // a link a scenario points elsewhere, made anew, and a folder with no file yet
        let _ = std::fs::remove_file(dir.join("9-link.ppm"));
        std::os::unix::fs::symlink("target/8-target.ppm", dir.join("9-link.ppm")).unwrap();
        let _ = std::fs::remove_dir_all(dir.join("later"));
        std::fs::create_dir_all(dir.join("later")).unwrap();
        // a folder a scenario replaces under its path
        let _ = std::fs::remove_dir_all(dir.join("replaced"));
        let _ = std::fs::remove_dir_all(dir.join("replaced-old"));
        std::fs::create_dir_all(dir.join("replaced")).unwrap();
        for name in ["replaced/a.ppm", "replaced/b.ppm"] {
            ppm(name, 300, 200, &|_, _| [255, 0, 0]);
        }
        std::fs::create_dir_all(dir.join("grid")).unwrap();
        for i in 0..40 {
            let color = [(i * 6) as u8, 255 - (i * 6) as u8, 128];
            ppm(&format!("grid/{i:02}.ppm"), 120, 90, &|_, _| color);
        }
        // a copy an edit is saved beside, made anew
        let _ = std::fs::remove_dir_all(dir.join("edit"));
        std::fs::create_dir_all(dir.join("edit")).unwrap();
        std::fs::copy(dir.join("1-red-blue.ppm"), dir.join("edit/1-red-blue.ppm")).unwrap();
        // a decode that takes longer than the fonts, a second or more
        ppm("7-slow.ppm", 4000, 3000, &|_, y| [0, (y % 256) as u8, 128]);
    }

    // Runs the scenario's child and checks how it ended. Its output goes to files in the
    // scenario's folder, read once it ended; at the limit its whole process group is killed, so
    // nothing it started, as a key-handler, holds the run.
    fn parent(scenario: &Scenario, dir: &Path) -> Result<(), String> {
        let file = |name: &str| std::fs::File::create(dir.join(name)).map_err(|e| e.to_string());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--scenario")
            .arg(scenario.name)
            .arg(dir)
            .args((scenario.args)(dir))
            // the key-handler's place, and the grid's thumbnails, in the scenario's folder
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .stdin(Stdio::null())
            .stdout(file("stdout")?)
            .stderr(file("stderr")?)
            .process_group(0)
            .spawn()
            .map_err(|e| e.to_string())?;
        let read = |name: &str| {
            let bytes = std::fs::read(dir.join(name)).unwrap_or_default();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if start.elapsed() > LIMIT {
                // SAFETY: a signal to the child's own process group, which it leads
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                let _ = child.wait();
                return Err(format!("hung, stderr: {}", read("stderr")));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let (stdout, stderr) = (read("stdout"), read("stderr"));
        if status.code() != Some(scenario.status) {
            return Err(format!(
                "ended with {:?} (signal {:?}), not {}; stderr: {stderr}",
                status.code(),
                status.signal(),
                scenario.status
            ));
        }
        let printed: Vec<&str> = stdout.lines().collect();
        if printed != (scenario.printed)(dir) {
            return Err(format!("-o printed {printed:?}; stderr: {stderr}"));
        }
        if let Some(warned) = scenario.warned
            && !stderr
                .lines()
                .any(|line| line.starts_with("nsxiv-ng: ") && line.contains(warned))
        {
            return Err(format!("no warning naming {warned} on stderr: {stderr}"));
        }
        Ok(())
    }

    // The child: the frontend on the main thread, the scenario on another.
    fn child(name: &str, dir: &str, args: &[String]) -> ! {
        let scenario = SCENARIOS.iter().find(|s| s.name == name).unwrap();
        let Ok(Parsed::Run(options)) = options::parse(args.iter().map(Into::into)) else {
            fail("the viewer's arguments do not parse");
        };
        let files = options
            .paths
            .iter()
            .map(|path| FileEntry {
                path: path.clone(),
                marked: false,
            })
            .collect();
        let dir = PathBuf::from(dir);
        let drive = scenario.drive;
        let driven = dir.clone();
        std::thread::spawn(move || {
            drive(&driven);
            // the scenario ended the viewer, which exits
            std::thread::sleep(LIMIT);
        });
        let program = std::env::current_exe().unwrap();
        // the config file a scenario's arguments wrote, as the viewer reads it
        let file = dir.join("config/nsxiv-ng/config.toml");
        let mut config = std::fs::read_to_string(file)
            .map_or_else(|_| Config::default(), |text| Config::parse(&text).0);
        // show_bar, as a config file sets it
        config.show_bar = !BAR_HIDDEN.contains(&name);
        // files opened from Finder, as the viewer's command line finds them
        let recursive = options.recursive;
        let found = Box::new(move |paths: &[PathBuf]| {
            FileList::build(paths, recursive, nsxiv_ng_decode::EXTENSIONS).files
        });
        let error = nsxiv_ng_macos::run(options, files, &config, program, found);
        fail(&format!("the frontend did not start: {error}"));
    }

    fn fail(why: &str) -> ! {
        let _ = writeln!(std::io::stderr(), "scenario failed: {why}");
        std::process::exit(3)
    }

    fn check(holds: bool, why: impl FnOnce() -> String) {
        if !holds {
            fail(&why());
        }
    }

    // Runs `f` on the main thread, between the application's events, and gives its result.
    fn on_main<R: Send>(f: impl FnOnce(MainThreadMarker) -> R + Send) -> R {
        let mut result = None;
        DispatchQueue::main().exec_sync(|| {
            result = Some(f(MainThreadMarker::new().unwrap()));
        });
        result.unwrap()
    }

    // The viewer's window, made by the time the frontend runs.
    fn window(mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
        NSApplication::sharedApplication(mtm)
            .windows()
            .firstObject()
    }

    // Waits up to ten seconds for `holds`, else fails with `what`.
    fn wait(what: &str, holds: impl Fn() -> bool) {
        let start = Instant::now();
        while !holds() {
            if start.elapsed() > Duration::from_secs(10) {
                fail(&format!("waited in vain for {what}"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// What a test reads of the window: whether it shows, its title, the file it represents,
    /// its frame and its content's size in points.
    #[derive(Clone, Debug, PartialEq)]
    struct Seen {
        visible: bool,
        title: String,
        file: String,
        frame: [f64; 4],
        content: [f64; 2],
    }

    fn seen() -> Seen {
        on_main(|mtm| {
            let window = window(mtm).unwrap_or_else(|| fail("no window"));
            let frame = window.frame();
            let content = window.contentView().unwrap().bounds();
            let file = window.representedURL().and_then(|url| url.path());
            Seen {
                visible: window.isVisible(),
                title: window.title().to_string(),
                file: file.map(|p| p.to_string()).unwrap_or_default(),
                frame: [
                    frame.origin.x,
                    frame.origin.y,
                    frame.size.width,
                    frame.size.height,
                ],
                content: [content.size.width, content.size.height],
            }
        })
    }

    fn visible_frame() -> [f64; 4] {
        on_main(|mtm| {
            let window = window(mtm).unwrap();
            let screen = window
                .screen()
                .or_else(|| NSScreen::mainScreen(mtm))
                .unwrap();
            let v = screen.visibleFrame();
            [v.origin.x, v.origin.y, v.size.width, v.size.height]
        })
    }

    /// The view's pixels as the screen would show them, RGBA in sRGB, rows from the top.
    /// Sizes and places are in points, which the backing scale makes pixels, so a scenario
    /// holds on any screen.
    struct Pixels {
        width: usize,
        height: usize,
        scale: f64,
        /// The bytes of a row of pixels.
        row: usize,
        rgba: Vec<u8>,
    }

    impl Pixels {
        /// The pixel at the top left of point `x`, `y`.
        fn at(&self, x: usize, y: usize) -> [u8; 3] {
            let pixel = |point: usize| (point as f64 * self.scale) as usize;
            let i = pixel(y) * self.row + pixel(x) * 4;
            [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]]
        }

        /// The pixels of the rows of points from `top` to `bottom`.
        fn rows(&self, top: usize, bottom: usize) -> &[u8] {
            let row = |point: usize| (point as f64 * self.scale) as usize * self.row;
            &self.rgba[row(top)..row(bottom)]
        }
    }

    // Reads the view back through AppKit into a bitmap tagged sRGB, the frame's own colors;
    // saves it as `name`.png where NSXIV_NG_SHOTS names a folder.
    fn pixels(name: &str) -> Pixels {
        let shots = std::env::var_os("NSXIV_NG_SHOTS").map(PathBuf::from);
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let view = window.contentView().unwrap();
            let bounds = view.bounds();
            let scale = window.backingScaleFactor();
            let width = (bounds.size.width * scale).round() as usize;
            let height = (bounds.size.height * scale).round() as usize;
            // SAFETY: a bitmap that allocates its own memory, 8 bits a sample, 4 samples
            let rep = unsafe {
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(),
                    std::ptr::null_mut(),
                    width as isize,
                    height as isize,
                    8,
                    4,
                    true,
                    false,
                    NSDeviceRGBColorSpace,
                    (width * 4) as isize,
                    32,
                )
            }
            .unwrap();
            let rep = rep
                .bitmapImageRepByRetaggingWithColorSpace(&NSColorSpace::sRGBColorSpace())
                .unwrap();
            rep.setSize(bounds.size);
            view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
            // SAFETY: the bitmap's data, width x height pixels of four bytes, alive with it
            let rgba = unsafe { std::slice::from_raw_parts(rep.bitmapData(), width * height * 4) };
            if let Some(shots) = shots {
                // SAFETY: an empty dictionary of properties for PNG
                let png = unsafe {
                    rep.representationUsingType_properties(
                        NSBitmapImageFileType::PNG,
                        &NSDictionary::new(),
                    )
                };
                if let Some(png) = png {
                    let _ = std::fs::create_dir_all(&shots);
                    let _ = std::fs::write(shots.join(format!("{name}.png")), png.to_vec());
                }
            }
            Pixels {
                width: bounds.size.width as usize,
                height: bounds.size.height as usize,
                scale,
                row: width * 4,
                rgba: rgba.to_vec(),
            }
        })
    }

    fn now() -> f64 {
        NSProcessInfo::processInfo().systemUptime()
    }

    // Posts a key to the application, as typed into the viewer's window: `characters` without
    // modifiers, its virtual `code`, typed `waited` seconds ago, a repeat if `repeat`.
    fn key_event(
        characters: &str,
        code: u16,
        flags: NSEventModifierFlags,
        waited: f64,
        repeat: bool,
    ) {
        let characters = characters.to_owned();
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let text = NSString::from_str(&characters);
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyDown,
                NSPoint::ZERO,
                flags,
                now() - waited,
                window.windowNumber(),
                None,
                &text,
                &text,
                repeat,
                code,
            )
            .unwrap();
            NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false);
        });
    }

    fn key(characters: &str) {
        key_event(characters, 0, NSEventModifierFlags::empty(), 0.0, false);
    }

    // Posts a mouse event of `kind` at `x`, `y` in the view's points from its top left.
    fn mouse(kind: NSEventType, x: f64, y: f64) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let event = mouse_event(&window, kind, x, y);
            NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false);
        });
    }

    fn mouse_event(window: &NSWindow, kind: NSEventType, x: f64, y: f64) -> Retained<NSEvent> {
        let height = window.contentView().unwrap().bounds().size.height;
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind,
            NSPoint::new(x, height - y),
            NSEventModifierFlags::empty(),
            now(),
            window.windowNumber(),
            None,
            0,
            1,
            1.0,
        )
        .unwrap()
    }

    // A wheel's `lines`, positive away from the user, at the view's center. Posted, such an
    // event never reaches a view of a window the pointer is not over, so it goes through the
    // window's own dispatch, as a window that has it sends it on.
    fn wheel(lines: i32) {
        scroll(CGScrollEventUnit::Line, 0, lines);
    }

    // A trackpad's scroll of `dx`, `dy` pixels, which AppKit gives as precise deltas, positive
    // as the content follows fingers that move right and down; as `wheel`.
    fn swipe(dx: i32, dy: i32) {
        scroll(CGScrollEventUnit::Pixel, dx, dy);
    }

    fn scroll(unit: CGScrollEventUnit, dx: i32, dy: i32) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            window.sendEvent(&scroll_event(mtm, &window, unit, dx, dy));
        });
    }

    // A scroll of `dy` pixels with Control held, the pointer at `x`, `y` in the view's points
    // from its top left.
    fn control_scroll(dy: i32, x: f64, y: f64) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let event = scroll_event(mtm, &window, CGScrollEventUnit::Pixel, 0, dy);
            let event = event.CGEvent().unwrap();
            let height = window.contentView().unwrap().bounds().size.height;
            let screen = NSScreen::screens(mtm).firstObject().unwrap().frame();
            CGEvent::set_location(
                Some(&event),
                CGPoint::new(x, screen.size.height - height + y),
            );
            CGEvent::set_flags(Some(&event), CGEventFlags::MaskControl);
            window.sendEvent(&NSEvent::eventWithCGEvent(&event).unwrap());
        });
    }

    fn scroll_event(
        mtm: MainThreadMarker,
        window: &NSWindow,
        unit: CGScrollEventUnit,
        dx: i32,
        dy: i32,
    ) -> Retained<NSEvent> {
        let bounds = window.contentView().unwrap().bounds();
        let screen = NSScreen::screens(mtm).firstObject().unwrap().frame();
        let event = CGEvent::new_scroll_wheel_event2(None, unit, 2, dy, dx, 0).unwrap();
        // no keys, where an event made with no source takes those held at the moment
        CGEvent::set_flags(Some(&event), CGEventFlags(0));
        // a window point, as the CG location of an event with no window reads
        let at = CGPoint::new(
            bounds.size.width / 2.0,
            screen.size.height - bounds.size.height / 2.0,
        );
        CGEvent::set_location(Some(&event), at);
        let event = NSEvent::eventWithCGEvent(&event).unwrap();
        let precise = unit == CGScrollEventUnit::Pixel;
        check(event.hasPreciseScrollingDeltas() == precise, || {
            format!("a scroll in {unit:?} was not taken as precise {precise}")
        });
        event
    }

    // The window's pixels once two snapshots a while apart are the same.
    fn settled(name: &str) -> Pixels {
        let mut last = pixels(name);
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            let now = pixels(name);
            if now.rgba == last.rgba {
                return now;
            }
            last = now;
        }
        fail("the window's pixels never settled")
    }

    // Waits for the window to show, with its first frame.
    fn shown() -> Seen {
        wait("the window to show", || {
            on_main(|mtm| window(mtm).is_some_and(|w| w.isVisible()))
        });
        seen()
    }

    // The width of the window's content, in points.
    fn width() -> f64 {
        on_main(|mtm| {
            window(mtm)
                .unwrap()
                .contentView()
                .unwrap()
                .bounds()
                .size
                .width
        })
    }

    fn red(p: [u8; 3]) -> bool {
        p == [255, 0, 0]
    }

    fn blue(p: [u8; 3]) -> bool {
        p == [0, 0, 255]
    }

    fn green(p: [u8; 3]) -> bool {
        p == [0, 255, 0]
    }

    // Waits for the title to be `name`.
    fn titled(name: &'static str) {
        wait(&format!("the title {name}"), || seen().title == name);
    }

    // The bar starts hidden: the window first shows at the image's size, with no bar's rows,
    // the image down to its last row; b shows the bar below the image, the window growing by
    // its rows, and b hides it again.
    fn bar_hidden(_: &Path) {
        let first = shown();
        check(first.content == [300.0, 200.0], || {
            format!(
                "the window first showed at {:?}, not 300x200",
                first.content
            )
        });
        let image = pixels("no-bar");
        check(blue(image.at(150, 199)), || {
            format!(
                "the last row is {:?}, not the image's blue",
                image.at(150, 199)
            )
        });
        key("b");
        wait("the bar to show", || seen().content[1] > 200.0);
        let bar = seen().content[1] - 200.0;
        check(bar > 10.0 && bar < 40.0, || {
            format!("a bar of {bar} points")
        });
        let below = pixels("bar").at(2, 200 + bar as usize / 2);
        check(!red(below) && !blue(below), || {
            format!("no bar below: {below:?}")
        });
        key("b");
        wait("the bar to go", || seen().content[1] == 200.0);
        key("q");
    }

    // With the bar hidden, an edit shows its status line in the window as it is, over the
    // bottom rows, with the line's width that ] and [ change, and leaving it hides the line
    // again. The window keeps its size throughout, also on leaving fullscreen during the edit,
    // which fits it by the viewer's hidden bar, not by the edit's line. The host never reaches
    // fullscreen, so the window's delegate is told of it, as AppKit tells it.
    fn edit_bar_hidden(_: &Path) {
        shown();
        let bottom = |p: &Pixels| p.at(150, 195);
        check(blue(bottom(&pixels("viewer"))), || {
            "the image's bottom is not blue".into()
        });
        key("x");
        wait("the edit's status line", || !blue(bottom(&pixels("edit"))));
        check(seen().content == [300.0, 200.0], || {
            format!("the edit resized the window to {:?}", seen().content)
        });
        let footer = |p: &Pixels| p.rows(190, 200).to_vec();
        let four = footer(&pixels("edit"));
        key("]");
        wait("] in the status line", || {
            footer(&pixels("edit-wider")) != four
        });
        key("[");
        wait("[ in the status line", || footer(&pixels("edit")) == four);
        tell(
            sel!(windowDidEnterFullScreen:),
            "NSWindowDidEnterFullScreenNotification",
        );
        tell(
            sel!(windowDidExitFullScreen:),
            "NSWindowDidExitFullScreenNotification",
        );
        std::thread::sleep(Duration::from_millis(500));
        check(seen().content == [300.0, 200.0], || {
            format!("leaving fullscreen in the edit gave {:?}", seen().content)
        });
        key("q");
        wait("the status line to go", || blue(bottom(&pixels("left"))));
        check(seen().content == [300.0, 200.0], || {
            format!("leaving resized the window to {:?}", seen().content)
        });
        key("q");
    }

    // The first frame shows the window at its final size, the image's with the bar's rows, as
    // it was never on screen before, titled with the file's name and representing it, in the
    // frame's own color space; red above blue, and the bar below.
    fn first_frame(dir: &Path) {
        let first = shown();
        let bar = first.content[1] - 200.0;
        check(
            first.content[0] == 300.0 && bar > 10.0 && bar < 40.0,
            || {
                format!(
                    "the window first showed at {:?}, not 300x200 with the bar",
                    first.content
                )
            },
        );
        check(first.title == "1-red-blue.ppm", || {
            format!("titled {}", first.title)
        });
        let srgb = on_main(|mtm| {
            let space = window(mtm).unwrap().colorSpace();
            space.is_some_and(|space| *space == *NSColorSpace::sRGBColorSpace())
        });
        check(srgb, || "the window's color space is not sRGB".into());
        let represented = path(dir, "1-red-blue.ppm");
        check(first.file.ends_with("1-red-blue.ppm"), || {
            format!("represents {}, not {represented}", first.file)
        });
        let pixels = pixels("first-image");
        check(
            (pixels.width, pixels.height) == (300, 200 + bar as usize),
            || format!("the frame is {}x{}", pixels.width, pixels.height),
        );
        check(
            red(pixels.at(150, 5)) && red(pixels.at(0, 0)) && red(pixels.at(299, 99)),
            || format!("the top is {:?}, not red", pixels.at(150, 5)),
        );
        check(
            blue(pixels.at(150, 195)) && blue(pixels.at(299, 199)),
            || format!("the bottom is {:?}, not blue", pixels.at(150, 195)),
        );
        let below = pixels.at(2, 200 + bar as usize / 2);
        check(!red(below) && !blue(below), || {
            format!("no bar below: {below:?}")
        });
        // a regular application, with About and Quit on Cmd-Q, and a window of every kind of
        // button, resizable, which no tab joins
        let (policy, menu, style, tabbing) = on_main(|mtm| {
            let app = NSApplication::sharedApplication(mtm);
            let items = app
                .mainMenu()
                .unwrap()
                .itemAtIndex(0)
                .unwrap()
                .submenu()
                .unwrap()
                .itemArray();
            let menu: Vec<String> = (0..items.count())
                .map(|i| items.objectAtIndex(i))
                .map(|item| format!("{}/{}", item.title(), item.keyEquivalent()))
                .collect();
            let window = window(mtm).unwrap();
            (
                app.activationPolicy(),
                menu,
                window.styleMask(),
                window.tabbingMode(),
            )
        });
        check(policy == NSApplicationActivationPolicy::Regular, || {
            "not a regular application".into()
        });
        check(menu == ["About nsxiv-ng/", "/", "Quit nsxiv-ng/q"], || {
            format!("the menu is {menu:?}")
        });
        let kinds = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        check(style == kinds, || {
            format!("the window's style is {style:?}")
        });
        check(tabbing == NSWindowTabbingMode::Disallowed, || {
            "the window takes tabs".into()
        });
        key("q");
    }

    // The window shows at its final size where the image decodes slower than the fonts load,
    // when a frame could come with the bar and no image yet: as it first shows, it has the
    // size it keeps.
    fn first_frame_slow(_: &Path) {
        let first = shown();
        std::thread::sleep(Duration::from_millis(1500));
        let later = seen();
        check(
            first.frame == later.frame && first.content == later.content,
            || {
                format!(
                    "the window showed at {:?}, then went to {:?}",
                    first.content, later.content
                )
            },
        );
        let bar = later.content[1] - later.content[0] * 3.0 / 4.0;
        check(bar > 10.0 && bar < 40.0, || {
            format!("{:?} is not the image's shape", later.content)
        });
        key("q");
    }

    // m marks and goes on; a repeat that waited goes unheard, a fresh one does not; a key
    // without a name ends a C-x prefix, and so does a key with Command, which q then quits,
    // where it would have gone to the key-handler.
    fn keys(_: &Path) {
        shown();
        key("m");
        titled("2-green.ppm");
        key_event("k", 40, NSEventModifierFlags::empty(), 0.5, true);
        std::thread::sleep(Duration::from_millis(300));
        let after = seen();
        check(after.title == "2-green.ppm", || {
            "a repeat that waited went back".into()
        });
        key_event("k", 40, NSEventModifierFlags::empty(), 0.0, true);
        titled("1-red-blue.ppm");
        // Tab, which has no name, ends the prefix, so j goes on, where it would have gone to
        // the key-handler
        key_event("x", 7, NSEventModifierFlags::Control, 0.0, false);
        key_event("\t", 48, NSEventModifierFlags::empty(), 0.0, false);
        key("j");
        titled("2-green.ppm");
        key_event("x", 7, NSEventModifierFlags::Control, 0.0, false);
        key_event("b", 11, NSEventModifierFlags::Command, 0.0, false);
        key("q");
    }

    // m on the only image redraws the bar alone: the image layer keeps its image, the bar's
    // gets a new one, and the bar's pixels change.
    fn bar_alone(_: &Path) {
        shown();
        let before = shown_images();
        let bar_before = pixels("bar-before");
        key("m");
        wait("the bar to change", || shown_images().1 != before.1);
        let after = shown_images();
        check(after.0 == before.0 && after.0.is_some(), || {
            "the mark redrew the image above the bar".into()
        });
        let bar_after = pixels("bar-after");
        check(bar_before.rows(0, 200) == bar_after.rows(0, 200), || {
            "the image's pixels changed".into()
        });
        check(bar_before.rgba != bar_after.rgba, || {
            "the bar's pixels stayed".into()
        });
        key("q");
    }

    // The addresses of the images the view's two layers show, the frame above the bar and the
    // bar's.
    fn shown_images() -> (Option<usize>, Option<usize>) {
        on_main(|mtm| {
            let view = window(mtm).unwrap().contentView().unwrap();
            // SAFETY: the view's layer has the two layers the frontend added
            let layers = unsafe { view.layer().unwrap().sublayers() }.unwrap();
            let image = |i| {
                // SAFETY: a layer's contents, read as an object's address alone
                let contents = unsafe { layers.objectAtIndex(i).contents() };
                contents.map(|c| Retained::as_ptr(&c) as usize)
            };
            (image(0), image(1))
        })
    }

    // A change of the window's backing scale, as on a move to a screen of another, lays the
    // frame out and draws it anew, for the new scale. The host has one screen, so the window's
    // delegate is told of the change here, as AppKit tells it.
    fn backing_scale(_: &Path) {
        shown();
        let before = shown_images();
        tell(
            sel!(windowDidChangeBackingProperties:),
            "NSWindowDidChangeBackingPropertiesNotification",
        );
        wait("a frame drawn anew", || shown_images().0 != before.0);
        key("q");
    }

    // A large image fits inside the visible frame, the window centered; the next, smaller one
    // takes its own size, around the same center.
    fn fit(_: &Path) {
        let first = shown();
        let visible = visible_frame();
        let [x, y, w, h] = first.frame;
        check(
            x >= visible[0]
                && y >= visible[1]
                && x + w <= visible[0] + visible[2]
                && y + h <= visible[1] + visible[3],
            || {
                format!(
                    "the window {:?} reaches past the visible frame {visible:?}",
                    first.frame
                )
            },
        );
        // half again the visible frame both ways, within the room: one side fills it
        check(w == visible[2] || h == visible[3], || {
            format!(
                "the window {:?} does not fill the visible frame {visible:?}",
                first.frame
            )
        });
        key("j");
        titled("2-green.ppm");
        wait("the window to fit the green image", || width() == 200.0);
        let second = seen();
        let center = |f: [f64; 4]| (f[0] + f[2] / 2.0, f[1] + f[3] / 2.0);
        let (a, b) = (center(first.frame), center(second.frame));
        check((a.0 - b.0).abs() <= 1.0 && (a.1 - b.1).abs() <= 1.0, || {
            format!("the center moved from {a:?} to {b:?}")
        });
        key("q");
    }

    // Leaving fullscreen fits the window by what shows then: an image that came in fullscreen
    // asked for a size it could not have and gets it on leaving, and a file that cannot be
    // read, which has no size, leaves the window as it is, not at the image's before it. The
    // host never reaches fullscreen, so the window's delegate is told here of entering and
    // leaving it, as AppKit tells it.
    fn fullscreen_fit(_: &Path) {
        let first = shown();
        let enter = || {
            let name = "NSWindowDidEnterFullScreenNotification";
            tell(sel!(windowDidEnterFullScreen:), name);
        };
        let exit = || {
            let name = "NSWindowDidExitFullScreenNotification";
            tell(sel!(windowDidExitFullScreen:), name);
        };
        enter();
        key("j");
        titled("2-green.ppm");
        wait("the green image", || {
            green(pixels("fullscreen").at(100, 50))
        });
        key("j");
        titled("4-unreadable.png");
        wait("the text of what went wrong, the green image gone", || {
            let pixels = pixels("fullscreen");
            !green(pixels.at(100, 50)) && text(&pixels) > 50
        });
        exit();
        std::thread::sleep(Duration::from_millis(500));
        let content = seen().content;
        check(content == first.content, || {
            format!("the window took {content:?} on leaving, with nothing to fit")
        });
        enter();
        key("k");
        titled("2-green.ppm");
        wait("the green image", || {
            green(pixels("fullscreen").at(100, 50))
        });
        std::thread::sleep(Duration::from_millis(300));
        check(seen().content == first.content, || {
            "the window took the image's size in fullscreen".into()
        });
        exit();
        let bar = first.content[1] - 200.0;
        wait("the green image's size on leaving fullscreen", || {
            seen().content == [200.0, 100.0 + bar]
        });
        key("q");
    }

    // Tells the window's delegate, with `method`, of the notification `name`, as AppKit would.
    fn tell(method: Sel, name: &'static str) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let delegate = window.delegate().unwrap();
            // SAFETY: a notification of the window, its object
            let note = unsafe {
                NSNotification::notificationWithName_object(
                    &NSString::from_str(name),
                    Some(&window),
                )
            };
            // SAFETY: a window delegate's method that takes a notification and returns nothing,
            // as AppKit calls it
            let () = unsafe { (&*delegate).send_message(method, (&*note,)) };
        });
    }

    // The grid starts at the config's size, and Return opens the image, which fits.
    fn grid(_: &Path) {
        let first = shown();
        check(first.content == [800.0, 600.0], || {
            format!("the grid's window is {:?}, not 800x600", first.content)
        });
        std::thread::sleep(Duration::from_millis(1000));
        pixels("grid");
        key_event("\r", 36, NSEventModifierFlags::empty(), 0.0, false);
        wait("the image to fit", || width() == 300.0);
        key("q");
    }

    // The colors the config file leaves out follow the system's appearance: the application's
    // set to dark gives a dark background and bar, and set back to light, light ones.
    fn appearance(_: &Path) {
        shown();
        let light = |pixel: [u8; 3]| pixel.iter().all(|&c| c > 200);
        let dark = |pixel: [u8; 3]| pixel.iter().all(|&c| c < 80);
        let corners = || {
            let pixels = pixels("appearance");
            (pixels.at(2, 2), pixels.at(2, pixels.height - 2))
        };
        // light first, whatever the host's appearance
        set_appearance(false);
        wait("a light background and bar", || {
            let (background, bar) = corners();
            light(background) && light(bar)
        });
        set_appearance(true);
        wait("a dark background and bar", || {
            let (background, bar) = corners();
            dark(background) && dark(bar)
        });
        set_appearance(false);
        wait("a light background and bar again", || {
            let (background, bar) = corners();
            light(background) && light(bar)
        });
        key("q");
    }

    // A background the config file sets stays as set, light or dark, where the bar follows.
    // A cell whose thumbnail is not in shows a placeholder, the system's faint label
    // color over the background: darker than it in the light appearance, lighter in the dark.
    // A file that gives no thumbnail keeps its placeholder.
    fn grid_placeholder(_: &Path) {
        shown();
        // the middle of the first cell, 30 points in, 148 a side
        let colors = || {
            let pixels = pixels("grid-placeholder");
            (pixels.at(104, 74), pixels.at(2, 2))
        };
        let sum = |p: [u8; 3]| p.iter().map(|&c| u32::from(c)).sum::<u32>();
        set_appearance(false);
        wait("a placeholder darker than the light background", || {
            let (placeholder, background) = colors();
            sum(background) > 600 && sum(placeholder) < sum(background)
        });
        set_appearance(true);
        wait("a placeholder lighter than the dark background", || {
            let (placeholder, background) = colors();
            sum(background) < 240 && sum(placeholder) > sum(background)
        });
        key("q");
    }

    fn appearance_config(_: &Path) {
        shown();
        let set = [0x12, 0x34, 0x56];
        let corners = || {
            let pixels = pixels("appearance-config");
            (pixels.at(2, 2), pixels.at(2, pixels.height - 2))
        };
        set_appearance(false);
        wait("the set background and a light bar", || {
            let (background, bar) = corners();
            background == set && bar.iter().all(|&c| c > 200)
        });
        set_appearance(true);
        wait("the set background and a dark bar", || {
            let (background, bar) = corners();
            background == set && bar.iter().all(|&c| c < 80)
        });
        set_appearance(false);
        wait("the set background and a light bar again", || {
            let (background, bar) = corners();
            background == set && bar.iter().all(|&c| c > 200)
        });
        key("q");
    }

    // The application's appearance, dark or light.
    fn set_appearance(dark: bool) {
        on_main(move |mtm| {
            // SAFETY: constant strings AppKit defines
            let name = unsafe {
                if dark {
                    NSAppearanceNameDarkAqua
                } else {
                    NSAppearanceNameAqua
                }
            };
            let appearance = NSAppearance::appearanceNamed(name);
            NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
        });
    }

    // The title bar fades: the frame fills the window, its top row the image's
    // under a strip and the system's bar that start invisible; a move over the content shows
    // both, and outside the strip they go half a second after it; leaving the window hides
    // them at once. In fullscreen the system's bar shows and the strip does not. The strip's
    // handlers are called here as the content's tracking area calls them, and the delegate
    // is told of fullscreen as AppKit tells it.
    fn title_bar(_: &Path) {
        let first = shown();
        check(first.frame[2..] == first.content[..], || {
            format!(
                "the frame {:?} is not the content {:?}",
                first.frame, first.content
            )
        });
        check(red(pixels("title-bar").at(150, 0)), || {
            "the top row is not the image's".into()
        });
        check(bars() == (0.0, 0.0), || {
            format!("the bars start at {:?}", bars())
        });
        to_strip(sel!(mouseMoved:), 150.0, 5.0);
        wait("the bars to show", || bars() == (1.0, 1.0));
        // the strip shown: the top rows read back are no longer the image's. The read-back
        // draws a view's subviews over it whatever their layers' order, so the strip's layer
        // is checked to come after the frame's two
        wait("the strip over the image's top", || {
            !red(pixels("strip-shown").at(150, 5))
        });
        let order = on_main(|mtm| {
            let view = window(mtm).unwrap().contentView().unwrap();
            let strip = view.subviews().firstObject().unwrap();
            let strip = strip.layer().map(|layer| Retained::as_ptr(&layer));
            // SAFETY: the view's layer, with the frame's two layers and the strip's
            let layers = unsafe { view.layer().unwrap().sublayers() }.unwrap();
            (0..layers.count())
                .map(|i| {
                    let layer = layers.objectAtIndex(i);
                    (Some(Retained::as_ptr(&layer)) == strip, layer.zPosition())
                })
                .collect::<Vec<_>>()
        });
        let above = order
            .iter()
            .position(|&(strip, _)| strip)
            .is_some_and(|at| at > 1 && order[..2].iter().all(|&(_, z)| z <= order[at].1));
        check(above, || {
            format!("the strip's layer is not above the frame's two: {order:?}")
        });
        std::thread::sleep(Duration::from_millis(800));
        check(bars() == (1.0, 1.0), || {
            "a move inside the strip hid the bars".into()
        });
        to_strip(sel!(mouseMoved:), 150.0, 150.0);
        std::thread::sleep(Duration::from_millis(300));
        check(bars() == (1.0, 1.0), || {
            "the bars went before half a second".into()
        });
        wait("the bars to go after a move outside", || {
            bars() == (0.0, 0.0)
        });
        wait("the image's top again", || {
            red(pixels("strip-hidden").at(150, 5))
        });
        to_strip(sel!(mouseMoved:), 150.0, 5.0);
        wait("the bars to show", || bars() == (1.0, 1.0));
        to_strip(sel!(mouseExited:), 150.0, -5.0);
        std::thread::sleep(Duration::from_millis(100));
        check(bars() == (0.0, 0.0), || {
            "leaving the window kept the bars".into()
        });
        tell(
            sel!(windowDidEnterFullScreen:),
            "NSWindowDidEnterFullScreenNotification",
        );
        wait("the system's bar alone in fullscreen", || {
            bars() == (0.0, 1.0)
        });
        to_strip(sel!(mouseMoved:), 150.0, 5.0);
        std::thread::sleep(Duration::from_millis(300));
        check(bars() == (0.0, 1.0), || {
            format!("a move in fullscreen gave {:?}", bars())
        });
        tell(
            sel!(windowDidExitFullScreen:),
            "NSWindowDidExitFullScreenNotification",
        );
        wait("both bars to go on leaving fullscreen", || {
            bars() == (0.0, 0.0)
        });
        key("q");
    }

    // The strip's alpha and the system's bar's.
    fn bars() -> (f64, f64) {
        on_main(|mtm| {
            let window = window(mtm).unwrap();
            let strip = window
                .contentView()
                .unwrap()
                .subviews()
                .firstObject()
                .unwrap();
            let button = window
                .standardWindowButton(NSWindowButton::CloseButton)
                .unwrap();
            // SAFETY: the view the button is in, which the window keeps
            let bar = unsafe { button.superview() }.unwrap();
            (strip.alphaValue(), bar.alphaValue())
        })
    }

    // Calls the strip's handler `method` with a mouse event at `x`, `y` in the view's points
    // from its top left.
    fn to_strip(method: Sel, x: f64, y: f64) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let strip = window
                .contentView()
                .unwrap()
                .subviews()
                .firstObject()
                .unwrap();
            let event = mouse_event(&window, NSEventType::MouseMoved, x, y);
            // SAFETY: a mouse handler of the strip's, with a mouse event, as AppKit calls it
            let () = unsafe { (&*strip).send_message(method, (&*event,)) };
        });
    }

    // A user's resize keeps the image's shape, the bar outside it: an edge drag drives the
    // other side, a corner drag gives the largest frame inside the proposed one; the grid
    // takes the proposal as it is. AppKit asks the window's delegate for a user's resize
    // alone, so it is asked here, as AppKit asks it.
    fn user_resize(_: &Path) {
        let first = shown();
        let bar = first.content[1] - 200.0;
        let chrome = (
            first.frame[2] - first.content[0],
            first.frame[3] - first.content[1],
        );
        let frame = |width: f64, height: f64| (width + chrome.0, height + chrome.1);
        for (proposed, kept, what) in [
            ((450.0, 200.0 + bar), (450.0, 300.0 + bar), "an edge drag"),
            (
                (300.0, 300.0 + bar),
                (450.0, 300.0 + bar),
                "a drag of the bottom edge",
            ),
            ((600.0, 300.0 + bar), (450.0, 300.0 + bar), "a corner drag"),
        ] {
            let answer = will_resize(frame(proposed.0, proposed.1));
            check(answer == frame(kept.0, kept.1), || {
                format!("{what} to {proposed:?} gave {answer:?}, not {kept:?} with the chrome")
            });
        }
        key_event("\r", 36, NSEventModifierFlags::empty(), 0.0, false);
        wait("the grid's size", || width() == 800.0);
        let proposed = frame(500.0, 300.0);
        let answer = will_resize(proposed);
        check(answer == proposed, || {
            format!("the grid's resize to {proposed:?} gave {answer:?}")
        });
        key("q");
    }

    // The zoom button gives the largest frame of the image's shape inside AppKit's standard
    // one, which a resize to it keeps, so a second press finds the window there and goes back;
    // the grid takes the standard frame as it is. The delegate is asked as AppKit asks it.
    fn zoom_button(_: &Path) {
        let first = shown();
        let bar = first.content[1] - 200.0;
        let default = (0.0, 60.0, 1024.0, 678.0);
        let wide = 1024f64.min((678.0 - bar) * 1.5);
        let high = wide / 1.5 + bar;
        let expected = (
            (1024.0 - wide) / 2.0,
            60.0 + (678.0 - high) / 2.0,
            wide,
            high,
        );
        let answer = standard_frame(default);
        check(answer == expected, || {
            format!("the zoom gave {answer:?}, not {expected:?}")
        });
        let kept = will_resize((answer.2, answer.3));
        check(kept == (answer.2, answer.3), || {
            format!("a resize to the zoom's frame gave {kept:?}")
        });
        key_event("\r", 36, NSEventModifierFlags::empty(), 0.0, false);
        wait("the grid's size", || width() == 800.0);
        let answer = standard_frame(default);
        check(answer == default, || {
            format!("the grid's zoom gave {answer:?}")
        });
        key("q");
    }

    // What the window's delegate answers the zoom button for AppKit's standard frame
    // `default`, x and y its bottom left corner.
    fn standard_frame(default: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let delegate = window.delegate().unwrap();
            let default = NSRect::new(
                NSPoint::new(default.0, default.1),
                NSSize::new(default.2, default.3),
            );
            // SAFETY: the delegate's own method, with the window, as AppKit calls it
            let answer: NSRect = unsafe {
                msg_send![&*delegate, windowWillUseStandardFrame: &*window, defaultFrame: default]
            };
            (
                answer.origin.x,
                answer.origin.y,
                answer.size.width,
                answer.size.height,
            )
        })
    }

    // What the window's delegate answers a user's resize of the window's frame to `size`.
    fn will_resize(size: (f64, f64)) -> (f64, f64) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let delegate = window.delegate().unwrap();
            let size = NSSize::new(size.0, size.1);
            // SAFETY: the delegate's own method, with the window, as AppKit calls it
            let answer: NSSize =
                unsafe { msg_send![&*delegate, windowWillResize: &*window, toSize: size] };
            (answer.width, answer.height)
        })
    }

    // A file that cannot be read shows the text of what went wrong, in the config's size.
    fn unreadable(_: &Path) {
        let first = shown();
        check(first.content == [800.0, 600.0], || {
            format!("the window is {:?}", first.content)
        });
        let text = text(&pixels("unreadable"));
        check(text > 50, || format!("{text} pixels of text in the middle"));
        // in the system's secondary label color: its darkest pixels a mid gray over the light
        // background, not the label's near black
        set_appearance(false);
        let darkest = || {
            let pixels = pixels("unreadable-gray");
            (pixels.height / 2 - 20..pixels.height / 2 + 20)
                .flat_map(|y| (0..pixels.width).map(move |x| (x, y)))
                .map(|(x, y)| pixels.at(x, y).iter().map(|&c| u32::from(c)).sum::<u32>())
                .min()
                .unwrap_or(0)
        };
        wait("text in a mid gray", || (180..500).contains(&darkest()));
        key("q");
    }

    // The pixels of text in the middle rows, those unlike the corner's background.
    fn text(pixels: &Pixels) -> usize {
        let background = pixels.at(2, 2);
        (0..pixels.height / 2 + 40)
            .flat_map(|y| (0..pixels.width).map(move |x| (x, y)))
            .filter(|&(x, y)| y > pixels.height / 2 - 40 && pixels.at(x, y) != background)
            .count()
    }

    // The left button, through the view's own handlers called directly: the host's window is
    // never key, and AppKit gives the first click on a window that is not key to no view. A
    // click on the right goes on, on the left back; a wheel's line away zooms in; a drag pans
    // and its release does not go on. The right button, posted, goes to the grid, whose window
    // takes the config's size.
    fn pointer(_: &Path) {
        shown();
        click(280.0, 50.0);
        titled("2-green.ppm");
        wait("the green image", || green(pixels("pointer").at(100, 50)));
        click(10.0, 50.0);
        titled("1-red-blue.ppm");
        wait("the red and blue image", || {
            red(pixels("pointer").at(150, 5))
        });
        // the zoom shows in the bar; the image, zoomed about its middle, reaches past the frame
        let bar = |p: &Pixels| p.rows(200, p.height).to_vec();
        let before = bar(&pixels("pointer"));
        wheel(1);
        wait("the wheel to zoom in, red above the middle", || {
            let pixels = pixels("zoomed");
            bar(&pixels) != before && red(pixels.at(150, 90))
        });
        // dragged up, the image's blue half comes up past where the red was
        handle(NSEventType::LeftMouseDown, 280.0, 100.0);
        handle(NSEventType::LeftMouseDragged, 280.0, 40.0);
        handle(NSEventType::LeftMouseUp, 280.0, 40.0);
        wait("the drag to pan", || blue(pixels("panned").at(150, 90)));
        std::thread::sleep(Duration::from_millis(300));
        check(seen().title == "1-red-blue.ppm", || {
            "a drag's release went on".into()
        });
        mouse(NSEventType::RightMouseDown, 150.0, 50.0);
        mouse(NSEventType::RightMouseUp, 150.0, 50.0);
        wait("the grid's size", || width() == 800.0);
        key("q");
    }

    // A click of the left button at `x`, `y`, through the view's own handlers.
    fn click(x: f64, y: f64) {
        handle(NSEventType::LeftMouseDown, x, y);
        handle(NSEventType::LeftMouseUp, x, y);
    }

    // A mouse event of `kind` at `x`, `y` in the view's points from its top left, given to the
    // view's own handler for it.
    fn handle(kind: NSEventType, x: f64, y: f64) {
        on_main(move |mtm| {
            let window = window(mtm).unwrap();
            let view = window.contentView().unwrap();
            let event = mouse_event(&window, kind, x, y);
            match kind {
                NSEventType::LeftMouseDown => view.mouseDown(&event),
                NSEventType::LeftMouseDragged => view.mouseDragged(&event),
                _ => view.mouseUp(&event),
            }
        });
    }

    // A trackpad's scroll pans the zoomed image: fingers that move up take
    // the image up with them, the blue half up past where the red was.
    fn trackpad(_: &Path) {
        shown();
        wait("the red and blue image", || {
            red(pixels("trackpad").at(150, 5))
        });
        let bar = |p: &Pixels| p.rows(200, p.height).to_vec();
        let before = bar(&pixels("trackpad"));
        wheel(1);
        wait("the wheel to zoom in, red above the middle", || {
            let pixels = pixels("trackpad");
            bar(&pixels) != before && red(pixels.at(150, 90))
        });
        swipe(0, -60);
        wait("the scroll to pan", || blue(pixels("swiped").at(150, 90)));
        key("q");
    }

    // A trackpad's pans of an image at 200%, each frame moved from the one before where the
    // session can, end on the very frame drawn whole: the antialiasing off and on again draws
    // that.
    fn pan_as_whole(_: &Path) {
        shown();
        key("=");
        key("+");
        key("+");
        settled("pan-zoomed");
        for (dx, dy) in [(-20, 0), (0, -15), (13, 9), (-7, 30), (25, -11), (0, 4)] {
            swipe(dx, dy);
            settled("panned");
        }
        let moved = settled("moved");
        let moves = nsxiv_ng_macos::MOVED_FRAMES.load(Ordering::Relaxed);
        check(moves >= 6, || {
            format!("{moves} frames moved the last, not every pan's")
        });
        key("a");
        settled("nearest");
        key("a");
        let whole = settled("whole");
        check(moved.rgba == whole.rgba, || {
            "the frame moved by the pans is not the frame drawn whole".into()
        });
        key("q");
    }

    // Tells the application's delegate of files opened, as Finder's open event does.
    fn open_urls(paths: &[PathBuf]) {
        let paths = paths.to_vec();
        on_main(move |mtm| {
            let app = NSApplication::sharedApplication(mtm);
            let delegate = app.delegate().unwrap();
            let urls: Vec<_> = (paths.iter())
                .map(|path| NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap())))
                .collect();
            let urls = NSArray::from_retained_slice(&urls);
            // SAFETY: the application delegate's method for files opened, which takes the
            // application and an array of file URLs and returns nothing, as AppKit calls it
            let () =
                unsafe { (&*delegate).send_message(sel!(application:openURLs:), (&*app, &*urls)) };
        });
    }

    // Opened as Finder opens an application, with no files on its command line, the viewer
    // waits for the event that gives them, then shows them; files opened while it runs come
    // after the current one, and show.
    fn open_events(dir: &Path) {
        std::thread::sleep(Duration::from_millis(500));
        check(on_main(|mtm| window(mtm).is_none()), || {
            "a window came before any file did".into()
        });
        open_urls(&[dir.join("1-red-blue.ppm"), dir.join("2-green.ppm")]);
        let first = shown();
        check(first.title == "1-red-blue.ppm", || {
            format!(
                "the window shows '{}', not the first file opened",
                first.title
            )
        });
        showing("the first file opened, red above", [255, 0, 0]);
        open_urls(&[dir.join("6-replacement.ppm")]);
        showing("the file opened while the viewer runs, blue", [0, 0, 255]);
        check(seen().title == "6-replacement.ppm", || {
            format!("the title is '{}', not the file opened", seen().title)
        });
        // it came after the current file, before the second opened at first
        key("j");
        showing("the next file, green", [0, 255, 0]);
        // a folder opened gives its images, which come after the current file, the first shown
        open_urls(&[dir.join("target")]);
        wait("the folder's image", || seen().title == "8-target.ppm");
        showing("the folder's image, red", [255, 0, 0]);
        key("q");
    }

    // Waits for the image shown to have `color` near its top left corner, a point that is
    // the image's in every file opened.
    fn showing(what: &str, color: [u8; 3]) {
        let near = |a: u8, b: u8| a.abs_diff(b) < 40;
        wait(what, || {
            let pixel = pixels("open-events").at(10, 10);
            (0..3).all(|i| near(pixel[i], color[i]))
        });
    }

    // A scroll with Control zooms about the pointer: half a step's delta at a
    // quarter of the width, on red left of blue, takes the edge between them right, where a
    // zoom about the middle would leave it there.
    fn zoom_at_pointer(_: &Path) {
        shown();
        wait("the red and blue image", || {
            let pixels = pixels("zoom-at-pointer");
            red(pixels.at(140, 100)) && blue(pixels.at(160, 100))
        });
        control_scroll(50, 75.0, 50.0);
        // the edge from 150 to 75 + 75 * 1.5
        wait("the edge to move right", || {
            let pixels = pixels("zoomed-at-pointer");
            red(pixels.at(180, 100)) && blue(pixels.at(195, 100))
        });
        key("q");
    }

    // Twenty scrolls that wait together, as a trackpad's do behind a slow frame, give one
    // frame: the image's layer gets new contents once, and they show all twenty.
    fn joined(_: &Path) {
        shown();
        wheel(1);
        std::thread::sleep(Duration::from_millis(500));
        settled("joined");
        on_main(|mtm| {
            let view = window(mtm).unwrap().contentView().unwrap();
            // SAFETY: the view's layer has the two layers the frontend added
            let layers = unsafe { view.layer().unwrap().sublayers() }.unwrap();
            let counter = FrameCounter::new();
            // SAFETY: an observer of the image layer's contents, kept for the process's life
            unsafe {
                let _: () = msg_send![&*layers.objectAtIndex(0), addObserver: &*counter,
                    forKeyPath: &*NSString::from_str("contents"), options: 0usize,
                    context: std::ptr::null_mut::<c_void>()];
            }
            std::mem::forget(counter);
        });
        on_main(|mtm| {
            let window = window(mtm).unwrap();
            for _ in 0..20 {
                window.sendEvent(&scroll_event(mtm, &window, CGScrollEventUnit::Pixel, 0, -2));
            }
        });
        // the image zoomed to 300 rows in 200 pans up 40 points, short of the 50 it can: the
        // edge between red and blue from 100 to 60, read just at each side of it
        wait("the image panned by all twenty", || {
            let pixels = pixels("joined-after");
            red(pixels.at(150, 58)) && blue(pixels.at(150, 62))
        });
        std::thread::sleep(Duration::from_millis(500));
        let frames = FRAMES.load(Ordering::Relaxed);
        check(frames == 1, || {
            format!("twenty scrolls drew {frames} frames, not one")
        });
        key("q");
    }

    /// The frames the image layer was given, as [`FrameCounter`] hears of them.
    static FRAMES: AtomicUsize = AtomicUsize::new(0);

    define_class!(
        /// Counts the changes of what it observes in [`FRAMES`].
        #[unsafe(super(NSObject))]
        #[name = "NsxivNgFrameCounter"]
        struct FrameCounter;

        impl FrameCounter {
            #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
            fn observe(
                &self,
                _: Option<&NSString>,
                _: Option<&AnyObject>,
                _: Option<&AnyObject>,
                _: *mut c_void,
            ) {
                FRAMES.fetch_add(1, Ordering::Relaxed);
            }
        }
    );

    impl FrameCounter {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            // SAFETY: NSObject's init, on an object just allocated
            unsafe { msg_send![super(this), init] }
        }
    }

    // A trackpad's scroll sideways pans the zoomed image: fingers that move
    // left take the image left with them, the blue half left past where the red was.
    fn trackpad_sideways(_: &Path) {
        shown();
        wait("the red and blue image", || {
            red(pixels("sideways").at(5, 100))
        });
        let bar = |p: &Pixels| p.rows(200, p.height).to_vec();
        let before = bar(&pixels("sideways"));
        wheel(1);
        wait("the wheel to zoom in, red left of the middle", || {
            let pixels = pixels("sideways");
            bar(&pixels) != before && red(pixels.at(140, 100))
        });
        swipe(-60, 0);
        wait("the scroll to pan", || blue(pixels("swiped").at(140, 100)));
        key("q");
    }

    // A trackpad's scroll in the grid scrolls it: fingers that move up take
    // the cells up with them. A scroll of nothing, as a gesture begins and ends, draws nothing.
    fn trackpad_grid(_: &Path) {
        shown();
        let before = settled("grid-before");
        let frame = shown_images().0;
        swipe(0, 0);
        std::thread::sleep(Duration::from_millis(500));
        check(shown_images().0 == frame, || {
            "a scroll of nothing drew a frame".into()
        });
        swipe(0, -100);
        // moved, in the snapshot that shows rows unlike those there before, so a grid that
        // stayed, or a blank one, does not pass
        let moved = |after: &Pixels| {
            after.rows(0, 400) == before.rows(100, 500) && after.rows(0, 400) != before.rows(0, 400)
        };
        wait("the grid to scroll up by 100 points", || {
            moved(&pixels("grid-after"))
        });
        key("q");
    }

    // A trackpad's scroll with Control in the grid scrolls it as without: the grid takes no
    // keys; only the image view zooms.
    fn grid_control_scroll(_: &Path) {
        shown();
        let before = settled("grid-before");
        control_scroll(-100, 400.0, 300.0);
        let moved = |after: &Pixels| {
            after.rows(0, 400) == before.rows(100, 500) && after.rows(0, 400) != before.rows(0, 400)
        };
        wait("the grid to scroll up by 100 points", || {
            moved(&pixels("grid-after"))
        });
        key("q");
    }

    // A window made larger, as by its edge, gets a frame of its new size.
    fn resize(_: &Path) {
        let first = shown();
        on_main(|mtm| {
            let window = window(mtm).unwrap();
            let mut frame = window.frame();
            frame.size.width += 100.0;
            frame.size.height += 50.0;
            window.setFrame_display(frame, true);
        });
        let width = first.content[0] as usize + 100;
        let height = first.content[1] as usize + 50;
        // the frame for the new size, read in one snapshot: the image centered in it, blue at
        // the center below the middle, the background on its left, and the bar the whole width
        wait("a frame of the new size, the image centered", || {
            let pixels = pixels("resized");
            if (pixels.width, pixels.height) != (width, height) {
                return false;
            }
            let left = pixels.at(10, 100);
            let (start, end) = (pixels.at(5, height - 5), pixels.at(width - 5, height - 5));
            blue(pixels.at(width / 2, height / 2 + 30))
                && !red(left)
                && !blue(left)
                && start == end
                && !red(start)
                && !blue(start)
        });
        key("q");
    }

    // The slideshow's timer goes on to the next image by itself.
    fn slideshow(_: &Path) {
        shown();
        titled("2-green.ppm");
        wait("the green image", || green(pixels("slideshow").at(100, 50)));
        key("q");
    }

    // The watched file saved over: once written in place, once replaced by a rename, once
    // deleted and made again, the image shows the new file each time.
    fn watch(dir: &Path) {
        shown();
        let watched = dir.join("5-watched.ppm");
        let red = std::fs::read(&watched).unwrap();
        let blue = std::fs::read(dir.join("6-replacement.ppm")).unwrap();
        let color = |is: fn([u8; 3]) -> bool, what: &str| {
            wait(what, || is(pixels("watch").at(150, 100)));
        };
        std::fs::write(&watched, &blue).unwrap();
        color(self::blue, "the file written in place");
        let temp = dir.join("5-watched.ppm.new");
        std::fs::write(&temp, &red).unwrap();
        std::fs::rename(&temp, &watched).unwrap();
        color(self::red, "the file replaced by a rename");
        // deleted, the viewer looks for the file again for two seconds, then keeps the image;
        // made after that, only the name's folder tells of a file where none was
        std::fs::remove_file(&watched).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        std::fs::write(&watched, &blue).unwrap();
        color(self::blue, "the file made again");
        key("q");
    }

    // The watched file saved over many times in a row, a few milliseconds apart, each by a
    // rename as an editor saves: the last save shows.
    fn watch_burst(dir: &Path) {
        shown();
        wait("the red image", || red(pixels("watch-burst").at(150, 100)));
        let watched = dir.join("5-watched.ppm");
        let solid = |rgb: [u8; 3]| {
            let mut bytes = b"P6\n300 200\n255\n".to_vec();
            bytes.extend(std::iter::repeat_n(rgb, 300 * 200).flatten());
            bytes
        };
        let saves = 30;
        for i in 0..saves {
            let rgb = match i {
                _ if i == saves - 1 => [0, 255, 0],
                _ if i % 2 == 0 => [0, 0, 255],
                _ => [255, 0, 0],
            };
            let temp = dir.join(format!("5-watched.ppm.{i}"));
            std::fs::write(&temp, solid(rgb)).unwrap();
            std::fs::rename(&temp, &watched).unwrap();
            std::thread::sleep(Duration::from_millis(3));
        }
        wait("the last save", || {
            green(pixels("watch-burst").at(150, 100))
        });
        key("q");
    }

    // The folder of the file shown renamed away and another made under its path; on to a file
    // in the new one, deleted and made again after the viewer stopped looking for it: it shows,
    // as only the new folder tells of it.
    fn watch_folder_replaced(dir: &Path) {
        shown();
        wait("the red image", || {
            red(pixels("watch-folder-replaced").at(150, 100))
        });
        let folder = dir.join("replaced");
        std::fs::rename(&folder, dir.join("replaced-old")).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let red = std::fs::read(dir.join("5-watched.ppm")).unwrap();
        let blue = std::fs::read(dir.join("6-replacement.ppm")).unwrap();
        std::fs::write(folder.join("b.ppm"), &red).unwrap();
        key("j");
        titled("b.ppm");
        wait("the red image in the new folder", || {
            self::red(pixels("watch-folder-replaced").at(150, 100))
        });
        std::fs::remove_file(folder.join("b.ppm")).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        std::fs::write(folder.join("b.ppm"), &blue).unwrap();
        wait("the file made again in the new folder", || {
            self::blue(pixels("watch-folder-replaced").at(150, 100))
        });
        key("q");
    }

    // A link shown: the file it names, in another folder, deleted and made again after the
    // viewer stopped looking for it, shows again: only that folder tells of it.
    fn watch_link(dir: &Path) {
        shown();
        wait("the red image", || red(pixels("watch-link").at(150, 100)));
        let target = dir.join("target/8-target.ppm");
        let blue = std::fs::read(dir.join("6-replacement.ppm")).unwrap();
        std::fs::remove_file(&target).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        std::fs::write(&target, blue).unwrap();
        wait("the file the link names, made again", || {
            self::blue(pixels("watch-link").at(150, 100))
        });
        key("q");
    }

    // A link shown, removed and then pointed at a name in a folder with no such file yet: the
    // file made there after the viewer stopped looking for it shows, as only that folder tells
    // of it.
    fn watch_link_elsewhere(dir: &Path) {
        shown();
        wait("the red image", || {
            red(pixels("watch-link-elsewhere").at(150, 100))
        });
        let link = dir.join("9-link.ppm");
        std::fs::remove_file(&link).unwrap();
        // the removal heard alone, the link dangling, before it points elsewhere
        std::thread::sleep(Duration::from_secs(1));
        std::os::unix::fs::symlink("later/9-later.ppm", &link).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let blue = std::fs::read(dir.join("6-replacement.ppm")).unwrap();
        std::fs::write(dir.join("later/9-later.ppm"), blue).unwrap();
        wait("the file made where the link points", || {
            self::blue(pixels("watch-link-elsewhere").at(150, 100))
        });
        key("q");
    }

    // Closing the window ends the viewer as q does: -o prints the mark.
    fn close(_: &Path) {
        shown();
        key("m");
        std::thread::sleep(Duration::from_millis(300));
        on_main(|mtm| window(mtm).unwrap().performClose(None));
        std::thread::sleep(LIMIT);
    }

    // Cmd-Q, through the menu, ends the viewer as q does.
    fn cmd_q(_: &Path) {
        shown();
        // the application's menu: About, a separator, and Quit on Command-q
        let items = on_main(|mtm| {
            let app = NSApplication::sharedApplication(mtm);
            let menu = app
                .mainMenu()
                .unwrap()
                .itemAtIndex(0)
                .unwrap()
                .submenu()
                .unwrap();
            let items = menu.itemArray();
            (0..items.count())
                .map(|i| items.objectAtIndex(i))
                .map(|item| {
                    let action = item
                        .action()
                        .map(|a| a.name().to_string_lossy().into_owned());
                    format!(
                        "{}/{}/{}",
                        item.title(),
                        item.keyEquivalent(),
                        action.unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
        });
        let expected = [
            "About nsxiv-ng//orderFrontStandardAboutPanel:",
            "//",
            "Quit nsxiv-ng/q/terminate:",
        ];
        check(items == expected, || {
            format!("the application's menu is {items:?}")
        });
        key("m");
        std::thread::sleep(Duration::from_millis(300));
        key_event("q", 12, NSEventModifierFlags::Command, 0.0, false);
        std::thread::sleep(LIMIT);
    }

    // The system's request to quit, for a logout, reaches the application's delegate as Cmd-Q
    // does, and with the viewer free, as here, its answer never cancels the logout: the viewer
    // ends within the call, as q does. A busy viewer's answer is not checked. The host cannot
    // log out, so the delegate's method is called here, and an answer that comes back at all
    // fails.
    fn terminate(_: &Path) {
        shown();
        key("m");
        std::thread::sleep(Duration::from_millis(300));
        on_main(|mtm| {
            let app = NSApplication::sharedApplication(mtm);
            let delegate = app.delegate().unwrap();
            // SAFETY: the delegate's own method, with the application, as AppKit calls it
            let reply: NSApplicationTerminateReply =
                unsafe { msg_send![&*delegate, applicationShouldTerminate: &*app] };
            fail(&format!(
                "the application's quit was answered with {reply:?}"
            ));
        });
    }

    // SIGINT, SIGTERM and SIGHUP each end the viewer as q does: -o prints the mark.
    fn signal(signal: libc::c_int) {
        shown();
        key("m");
        std::thread::sleep(Duration::from_millis(300));
        // SAFETY: a signal to this process, which the viewer takes
        unsafe { libc::kill(libc::getpid(), signal) };
        std::thread::sleep(LIMIT);
    }

    // A key-handler that sends itself SIGTERM dies of it: it starts with the signal at its
    // default, though the viewer catches it. Its wrapper notes the status it ended with.
    fn key_handler(dir: &Path) {
        let handler = dir.join("config/nsxiv-ng/exec/key-handler");
        let status = dir.join("handler-status");
        // none left by a run before this one, which would answer for this one's handler
        let _ = std::fs::remove_file(&status);
        let script = format!(
            "#!/bin/sh\nsh -c 'kill -TERM $$; exit 0'\necho $? >'{}'\n",
            status.display()
        );
        std::fs::write(&handler, script).unwrap();
        std::fs::set_permissions(
            &handler,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        shown();
        key_event("x", 7, NSEventModifierFlags::Control, 0.0, false);
        key("a");
        // the line, which the shell writes after it makes the file
        let read = || std::fs::read_to_string(&status).unwrap_or_default();
        let start = Instant::now();
        while !read().ends_with('\n') && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let ended = read();
        check(ended.trim() == "143", || {
            format!(
                "the key-handler's child ended with '{}', not killed by SIGTERM (143)",
                ended.trim()
            )
        });
        key("q");
    }

    /// The edit's red, systemRed as macOS gives it in the light appearance.
    fn edit_red(p: [u8; 3]) -> bool {
        p == [255, 56, 60]
    }

    // x, and the edit's status in the bar, where the keys posted have come through.
    fn enter_edit() {
        let bar = |p: &Pixels| p.rows(200, p.height).to_vec();
        let before = bar(&pixels("viewer-bar"));
        key("x");
        wait("the edit's status", || bar(&pixels("edit-bar")) != before);
        // the line's width as the bar shows it, 4 and 5, from ] and [ alone
        let four = bar(&pixels("edit-bar"));
        key("]");
        wait("] in the bar", || bar(&pixels("edit-bar")) != four);
        let five = bar(&pixels("edit-bar"));
        key("[");
        wait("[ in the bar", || bar(&pixels("edit-bar")) == four);
        let row = pixels("edit-bar").row;
        let columns: Vec<usize> = (0..row)
            .filter(|&i| {
                four.chunks(row)
                    .zip(five.chunks(row))
                    .any(|(a, b)| a[i] != b[i])
            })
            .collect();
        let width = Width {
            four: width_shown(&four, row, &columns),
            five: width_shown(&five, row, &columns),
            columns,
        };
        let _ = WIDTH.set(width);
    }

    /// Where the bar shows the line's width, the bytes of its rows that ] and [ change, and
    /// those bytes as it shows 4 and 5. The bar's left text is cut at its start, so its end,
    /// and the width in it, stay put whatever the tool before it.
    struct Width {
        columns: Vec<usize>,
        four: Vec<u8>,
        five: Vec<u8>,
    }

    static WIDTH: std::sync::OnceLock<Width> = std::sync::OnceLock::new();

    fn width_shown(bar: &[u8], row: usize, columns: &[usize]) -> Vec<u8> {
        bar.chunks(row)
            .flat_map(|line| columns.iter().map(|&i| line[i]))
            .collect()
    }

    // Keys to the edit, posted, which come through in their own time, in order: a width of 5
    // in the bar, which only the ] posted after them gives, says they are in; a width of 4
    // again, that the [ after it is.
    fn edit_keys(names: &[&str]) {
        let width = WIDTH.get().expect("the edit entered");
        let shown = || {
            let p = pixels("edit-bar");
            width_shown(p.rows(200, p.height), p.row, &width.columns)
        };
        for name in names {
            key(name);
        }
        key("]");
        wait("the keys before ] in", || shown() == width.five);
        key("[");
        wait("[ in", || shown() == width.four);
    }

    // A filled rectangle drawn from 50, 110 to 150, 160 of the canvas, through the view's own
    // pointer handlers, over the image's blue half.
    fn filled_rectangle() {
        edit_keys(&["r", "f", "f"]);
        handle(NSEventType::LeftMouseDown, 50.0, 110.0);
        handle(NSEventType::LeftMouseDragged, 150.0, 160.0);
        handle(NSEventType::LeftMouseUp, 150.0, 160.0);
    }

    // The edit's text field, where one is open: its frame in the view's points from the bottom
    // left, and its text.
    fn field() -> Option<([f64; 4], String)> {
        on_main(|mtm| {
            let view = window(mtm).unwrap().contentView().unwrap();
            let subviews = view.subviews();
            (0..subviews.count()).find_map(|i| {
                let field = subviews.objectAtIndex(i).downcast::<NSTextField>().ok()?;
                let f = field.frame();
                let frame = [f.origin.x, f.origin.y, f.size.width, f.size.height];
                Some((frame, field.stringValue().to_string()))
            })
        })
    }

    // x enters the edit, with the window at its size; a rectangle drawn shows in the frame, and
    // Command-z takes it away. Text's field shows at the text's place, empty; its words, set as
    // typing would and ended with Escape, show in the frame. Command-c puts the edit's PNG on
    // the pasteboard. q leaves the edit, and q then quits. Typing into the field cannot be
    // posted, as the host's window is never key: its text is set on the field itself.
    // An edit takes every key without Command: one it binds to nothing, as Tab
    // or Control-x, goes on to no window, which would beep, and a repeat that waited is not
    // dropped. The viewer keeps the codes of the keys its monitor let go on.
    fn edit_takes_its_keys(_: &Path) {
        shown();
        enter_edit();
        let passed = || nsxiv_ng_macos::KEYS_PASSED.load(Ordering::Relaxed);
        let last = || nsxiv_ng_macos::LAST_KEY_PASSED.load(Ordering::Acquire);
        let before = passed();
        key_event("\t", 48, NSEventModifierFlags::empty(), 0.0, false);
        key_event("x", 7, NSEventModifierFlags::Control, 0.0, false);
        // Command-F5, which goes on to the menu in an edit too: once it has gone by, so have
        // the keys before it
        key_event("\u{f708}", 96, NSEventModifierFlags::Command, 0.0, false);
        wait("the marker", || last() == 96);
        let passed = passed() - before;
        check(passed == 1, || {
            format!("the edit let {passed} keys go on to the window, not the marker alone")
        });
        // a repeat of ], posted a second ago, widens the line all the same
        let bar = |p: &Pixels| p.rows(200, p.height).to_vec();
        let before = bar(&pixels("edit-keys"));
        key_event("]", 30, NSEventModifierFlags::empty(), 1.0, true);
        wait("the repeated ] in the bar", || {
            bar(&pixels("edit-keys")) != before
        });
        key("q");
        key("q");
    }

    fn edit(_: &Path) {
        let before = shown();
        wait("the red and blue image", || {
            blue(pixels("edit").at(100, 150))
        });
        let viewer_frame = pixels("edit");
        // an Edit menu, whose actions go to the first responder, an open field's: the field takes
        // Command-v and the rest through it. The host has no pasteboard to paste from
        let edit_menu = on_main(|mtm| {
            let app = NSApplication::sharedApplication(mtm);
            let menu = app
                .mainMenu()
                .unwrap()
                .itemAtIndex(1)
                .unwrap()
                .submenu()
                .unwrap();
            let items = menu.itemArray();
            let items: Vec<String> = (0..items.count())
                .map(|i| items.objectAtIndex(i))
                .map(|item| {
                    let action = item
                        .action()
                        .map(|a| a.name().to_string_lossy().into_owned());
                    let targeted = item.target().is_some();
                    format!(
                        "{}/{}/{action:?}/{targeted}",
                        item.title(),
                        item.keyEquivalent()
                    )
                })
                .collect();
            (menu.title().to_string(), items)
        });
        let expected = [
            "Cut/x/Some(\"cut:\")/false",
            "Copy/c/Some(\"copy:\")/false",
            "Paste/v/Some(\"paste:\")/false",
            "Select All/a/Some(\"selectAll:\")/false",
        ];
        // AppKit adds its own items after them to a menu titled Edit: dictation, emoji
        check(
            edit_menu.0 == "Edit" && edit_menu.1.get(..4).is_some_and(|items| items == expected),
            || format!("the Edit menu is {edit_menu:?}"),
        );
        enter_edit();
        filled_rectangle();
        wait("the rectangle", || {
            edit_red(pixels("edit-rectangle").at(100, 150))
        });
        check(seen().content == before.content, || {
            "the window resized".into()
        });
        key_event("z", 6, NSEventModifierFlags::Command, 0.0, false);
        wait("Command-z to take it away", || {
            blue(pixels("edit-undone").at(100, 150))
        });
        // a text at 40, 150 of the canvas: from 4 below its baseline, 28 + 12 high, as wide as
        // the canvas leaves but 220 at least
        let blank = pixels("edit-blank");
        edit_keys(&["t"]);
        handle(NSEventType::LeftMouseDown, 40.0, 150.0);
        handle(NSEventType::LeftMouseUp, 40.0, 150.0);
        wait("the text field", || field().is_some());
        let height = seen().content[1];
        let (frame, text) = field().unwrap();
        let expected = [40.0, height - 154.0, 240.0, 40.0];
        check(frame == expected && text.is_empty(), || {
            format!("the field is at {frame:?} with {text:?}, not at {expected:?}")
        });
        on_main(|mtm| {
            let view = window(mtm).unwrap().contentView().unwrap();
            let subviews = view.subviews();
            for i in 0..subviews.count() {
                if let Ok(field) = subviews.objectAtIndex(i).downcast::<NSTextField>() {
                    field.setStringValue(&NSString::from_str("Hi there"));
                }
            }
        });
        key_event("\u{1b}", 53, NSEventModifierFlags::empty(), 0.0, false);
        wait("the field to end", || field().is_none());
        wait("the text in the frame", || {
            pixels("edit-text").rows(125, 150) != blank.rows(125, 150)
        });
        // read back where the session has a pasteboard, which one logged in over ssh alone has
        // not: there the copy warns, and the viewer goes on. Cleared first, it can only hold the
        // PNG Command-c puts there, with a rectangle drawn for it
        let board = on_main(|_| {
            // SAFETY: generalPasteboard takes nothing, and gives the pasteboard or nil
            let board: Option<Retained<NSPasteboard>> =
                unsafe { msg_send![NSPasteboard::class(), generalPasteboard] };
            board.map(|board| board.clearContents()).is_some()
        });
        // the rectangle tool again, still filling
        edit_keys(&["r"]);
        handle(NSEventType::LeftMouseDown, 50.0, 110.0);
        handle(NSEventType::LeftMouseDragged, 150.0, 160.0);
        handle(NSEventType::LeftMouseUp, 150.0, 160.0);
        wait("the rectangle again", || {
            edit_red(pixels("edit-copied").at(100, 150))
        });
        key_event("c", 8, NSEventModifierFlags::Command, 0.0, false);
        if board {
            wait(
                "the edit's PNG on the pasteboard, the rectangle in it",
                || {
                    on_main(|_| {
                        let board = NSPasteboard::generalPasteboard();
                        // SAFETY: a constant string AppKit defines
                        let png = board.dataForType(unsafe { NSPasteboardTypePNG });
                        let rep = png.and_then(|png| NSBitmapImageRep::imageRepWithData(&png));
                        rep.and_then(|rep| rep.colorAtX_y(100, 150))
                            .is_some_and(|color| {
                                let channel = |c: f64| (c * 255.0).round() as u8;
                                let rgb = [
                                    color.redComponent(),
                                    color.greenComponent(),
                                    color.blueComponent(),
                                ];
                                edit_red(rgb.map(channel))
                            })
                    })
                },
            );
        } else {
            std::thread::sleep(Duration::from_millis(500));
        }
        key("q");
        wait("the edit to end, the image as it was", || {
            pixels("edit-left").rows(125, 150) == viewer_frame.rows(125, 150)
        });
        check(seen().visible, || "q quit the viewer from the edit".into());
        key("q");
    }

    // Command-s saves the edit beside its file, as <stem>-edit.png, and the viewer shows it
    // next, the rectangle in it.
    fn edit_save(dir: &Path) {
        shown();
        wait("the red and blue image", || {
            blue(pixels("edit-save").at(100, 150))
        });
        enter_edit();
        filled_rectangle();
        wait("the rectangle", || {
            edit_red(pixels("edit-save-rectangle").at(100, 150))
        });
        key_event("s", 1, NSEventModifierFlags::Command, 0.0, false);
        titled("1-red-blue-edit.png");
        check(dir.join("edit/1-red-blue-edit.png").exists(), || {
            "no edit beside the image".into()
        });
        wait("the saved edit, the rectangle in it", || {
            edit_red(pixels("edit-saved").at(100, 150))
        });
        key("q");
    }

    // Return in a text field ends that field alone. Its end comes queued, and before it runs,
    // a press that ends the field and opens another at a second text, all in one turn of the
    // main thread; the queued end then leaves the second field open. Return is the field's
    // action sent as AppKit sends it, which a key posted to the never-key window cannot reach.
    fn edit_field_return(_: &Path) {
        shown();
        enter_edit();
        edit_keys(&["t"]);
        handle(NSEventType::LeftMouseDown, 40.0, 150.0);
        handle(NSEventType::LeftMouseUp, 40.0, 150.0);
        wait("the first field", || field().is_some());
        let first = field().unwrap().0;
        on_main(|mtm| {
            let window = window(mtm).unwrap();
            let view = window.contentView().unwrap();
            let subviews = view.subviews();
            let field = (0..subviews.count())
                .find_map(|i| subviews.objectAtIndex(i).downcast::<NSTextField>().ok())
                .unwrap();
            field.setStringValue(&NSString::from_str("first"));
            let target = field.target().unwrap();
            // SAFETY: the field's target takes its action with the field as the sender
            let _: () = unsafe { msg_send![&*target, ended: &*field] };
            let down = mouse_event(&window, NSEventType::LeftMouseDown, 40.0, 80.0);
            view.mouseDown(&down);
            let up = mouse_event(&window, NSEventType::LeftMouseUp, 40.0, 80.0);
            view.mouseUp(&up);
        });
        std::thread::sleep(Duration::from_millis(300));
        let second = field().map(|(frame, _)| frame);
        check(second.is_some_and(|frame| frame != first), || {
            format!("the second field is {second:?}, the first was at {first:?}")
        });
        key_event("\u{1b}", 53, NSEventModifierFlags::empty(), 0.0, false);
        wait("the second field to end", || field().is_none());
        key("q");
        key("q");
    }
}
