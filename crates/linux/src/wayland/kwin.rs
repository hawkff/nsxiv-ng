//! The KWin script that keeps the viewer's window on screen on KDE. KWin lets a window that
//! resizes itself grow past the work area. A script, loaded once over D-Bus, watches this
//! process's window from inside KWin and moves it back in when a resize pushes it past an
//! edge, as mutter does on GNOME. Nothing goes over D-Bus per image, and the script unloads
//! itself when the window goes.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Loads and runs the script for the window of process `pid`, through a file in the session's
/// runtime folder that KWin reads; does nothing where a step fails.
// shortcut: nothing waits for the helper thread at exit, so a viewer that quits while this
// runs may leave the script's file in the runtime folder, or the script loaded and never run,
// until a viewer with the same pid starts; wait for the helper at exit, within a bound, if that
// ever matters
pub(super) fn load(pid: u32, runtime: &Path) {
    let plugin = format!("nsxiv-ng-{pid}");
    let path = runtime.join(format!("{plugin}.js"));
    // an earlier process with this pid may have left its script behind
    let _ = std::fs::remove_file(&path);
    let _ = kwin(
        "/Scripting",
        "org.kde.kwin.Scripting.unloadScript",
        &[&format!("string:{plugin}")],
    );
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| file.write_all(script(pid, &plugin).as_bytes()));
    if written.is_err() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let reply = kwin(
        "/Scripting",
        "org.kde.kwin.Scripting.loadScript",
        &[
            &format!("string:{}", path.display()),
            &format!("string:{plugin}"),
        ],
    );
    // Plasma 6 puts the script at /Scripting/Script<id>, Plasma 5 at /<id>
    let run = "org.kde.kwin.Script.run";
    if let Some(id) = reply.as_deref().and_then(script_id)
        && kwin(&format!("/Scripting/Script{id}"), run, &[]).is_none()
        && kwin(&format!("/{id}"), run, &[]).is_none()
    {
        let _ = kwin(
            "/Scripting",
            "org.kde.kwin.Scripting.unloadScript",
            &[&format!("string:{plugin}")],
        );
    }
    // run replies once KWin has read the file
    let _ = std::fs::remove_file(&path);
}

/// The id of the script `loadScript` replied with: its last word, a number not below 0.
fn script_id(reply: &str) -> Option<i64> {
    reply
        .split_whitespace()
        .last()?
        .parse()
        .ok()
        .filter(|&id| id >= 0)
}

// A method of KWin's over the session's D-Bus; its reply, None when it failed.
fn kwin(path: &str, method: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("dbus-send")
        .args(["--session", "--print-reply", "--reply-timeout=5000"])
        .args(["--dest=org.kde.KWin", path, method])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The script for the window of process `pid`, loaded as `plugin`. KWin reports a new frame
/// before it applies a fullscreen, maximized or tiled state, so the check waits for KWin's
/// event loop; then a frame that grew past the work area of the screen it was on moves back
/// inside, unless the user had parked it across an edge.
fn script(pid: u32, plugin: &str) -> String {
    format!(
        r#"// loaded by nsxiv-ng for the window of process {pid}; it unloads itself with the window
const pid = {pid}, plugin = "{plugin}";
let tracked = null;

function rect(r) {{ return {{ x: r.x, y: r.y, width: r.width, height: r.height }}; }}
function inside(r, a) {{
    return r.x >= a.x && r.y >= a.y && r.x + r.width <= a.x + a.width && r.y + r.height <= a.y + a.height;
}}
function unload() {{
    callDBus("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", plugin);
}}
// the work area of the screen the frame was on: an output on Plasma 6, a number on Plasma 5
function areaAround(r, w) {{
    const output = workspace.screenAt({{ x: r.x + r.width / 2, y: r.y + r.height / 2 }});
    const found = typeof output === "number" ? output >= 0 : !!output;
    return found ? workspace.clientArea(KWin.MaximizeArea, output, workspace.currentDesktop)
                 : workspace.clientArea(KWin.MaximizeArea, w);
}}

// fullscreen, maximized or tiled: KWin, not the viewer, sizes the window
function held(w, r) {{
    if (w.fullScreen || w.tile) return true;
    if (w.maximizeMode !== undefined) return w.maximizeMode !== 0;
    const a = workspace.clientArea(KWin.MaximizeArea, w);
    return r.x === a.x && r.y === a.y && r.width === a.width && r.height === a.height;
}}

// the viewer has one window; Plasma 5 gives resourceClass as bytes, so the pid alone picks it
function track(w) {{
    if (tracked !== null || w.pid !== pid) return;
    tracked = w;
    let last = rect(w.frameGeometry), before = null;
    const check = new QTimer();
    check.singleShot = true;
    check.interval = 0;
    w.frameGeometryChanged.connect(function () {{
        if (before === null) before = last;
        check.start();
    }});
    check.timeout.connect(function () {{
        const old = before, now = rect(w.frameGeometry);
        before = null;
        // last keeps the frame from before KWin took over, so a restore compares with it
        if (w.deleted || held(w, now)) return;
        last = now;
        if (w.minimized || w.move || w.resize) return;
        if (now.width === old.width && now.height === old.height) return;
        const area = areaAround(old, w);
        if (!inside(old, area) || inside(now, area)) return;
        last = {{
            x: Math.max(area.x, Math.min(now.x, area.x + area.width - now.width)),
            y: Math.max(area.y, Math.min(now.y, area.y + area.height - now.height)),
            width: now.width,
            height: now.height,
        }};
        w.frameGeometry = last;
    }});
}}

(workspace.windowList ? workspace.windowList() : workspace.clientList()).forEach(track);
(workspace.windowAdded || workspace.clientAdded).connect(track);
(workspace.windowRemoved || workspace.clientRemoved).connect(function (w) {{
    if (w === tracked) unload();
}});
const giveUp = new QTimer(); // a viewer that never shows its window
giveUp.singleShot = true;
giveUp.interval = 60000;
giveUp.timeout.connect(function () {{ if (tracked === null) unload(); }});
giveUp.start();
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_id_is_the_last_word_of_the_reply() {
        let reply = "method return time=1.2 sender=:1.5 -> destination=:1.9 serial=7 reply_serial=2\n   int32 3\n";
        assert_eq!(script_id(reply), Some(3));
        assert_eq!(script_id("   int32 -1\n"), None, "KWin refused it");
        assert_eq!(script_id(""), None);
    }

    #[test]
    fn the_script_names_the_process_and_its_plugin() {
        let script = script(4242, "nsxiv-ng-4242");
        assert!(script.contains(r#"const pid = 4242, plugin = "nsxiv-ng-4242";"#));
        assert!(
            script.contains("function track(w) {\n"),
            "braces written once"
        );
    }
}
