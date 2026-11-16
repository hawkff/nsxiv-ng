#!/bin/sh
# Runs the viewer in a headless KWin, KDE's compositor, on $2 side-by-side 1280x800 screens,
# with two viewers whose slideshows go from a small image to one that needs most of a screen.
# A test-only KWin script places them and logs their frames. The "corner" window starts inside
# the first screen, near its bottom-right corner; KWin lets it run past that screen's edge as it
# grows, and the viewer's own KWin script must move it back inside the same screen. The "parked"
# window hangs past the right edge of the last screen, as a user may leave it; it must stay
# there as it grows, and come back to the same frame after a trip through fullscreen. Each
# viewer's script must unload itself when its viewer quits. It runs in a D-Bus session of its
# own, which KWin needs.
#
# packages: kwin-wayland dbus-daemon dbus-bin imagemagick
# modes: 1 2
set -eu
# a bus of this run's own, ended with it
[ "${KWIN_SMOKE_BUS:-}" = 1 ] || KWIN_SMOKE_BUS=1 exec dbus-run-session -- sh "$0" "$@"
bin=$1
outputs=${2:-1}
mode=kwin-$outputs
pics=$(mktemp -d)
compositor=
corner=
parked=
# whether process $1 runs: neither gone nor ended and waiting to be reaped
running() {
    state=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -c1)
    [ -n "$state" ] && [ "$state" != Z ]
}
# waits $2 seconds at most for process $1 to end; its exit status goes in $status
ended() {
    for _ in $(seq $(($2 * 10))); do
        if ! running "$1"; then
            status=0
            wait "$1" || status=$?
            return 0
        fi
        sleep 0.1
    done
    return 1
}
# this run's own processes, the viewers then the compositor, and its folder go on every way
# out: TERM, then KILL after a grace
stop() {
    for process in $corner $parked $compositor; do
        kill -TERM "$process" 2>/dev/null || true
        ended "$process" 4 || { kill -KILL "$process" 2>/dev/null; wait "$process" 2>/dev/null; } || true
    done
    rm -rf "$pics"
}
trap stop EXIT
trap 'exit 1' HUP INT TERM
log=$pics/compositor.log
fail() {
    echo "$mode: $1" >&2
    grep -v 'nsxiv-test' "$log" | tail -40 >&2 || true
    exit 1
}
images=$pics/images
mkdir -p "$images"
magick -size 200x200 xc:green "$images/1-small.png"
magick -size 1200x700 gradient:red-blue "$images/2-large.png"
observer=$pics/observer.js

export XDG_RUNTIME_DIR="$pics/runtime" XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_CURRENT_DESKTOP=KDE
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK
# a copy without the file capabilities a container cannot grant
install -m755 "$(command -v kwin_wayland)" "$pics/kwin_wayland"
# Qt logs to the journal when stderr is no terminal; a container has no journal
QT_FORCE_STDERR_LOGGING=1 QT_LOGGING_TO_CONSOLE=1 QT_LOGGING_RULES='js=true;kwin_scripting=true' \
    KWIN_COMPOSE=Q "$pics/kwin_wayland" --virtual --width=1280 --height=800 \
    --output-count="$outputs" --socket=wayland-k --no-lockscreen >"$log" 2>&1 &
compositor=$!
for _ in $(seq 150); do
    [ -S "$XDG_RUNTIME_DIR/wayland-k" ] && break
    running "$compositor" || fail "KWin did not start"
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/wayland-k" ] || fail "KWin opened no socket"

kwin() { dbus-send --session --print-reply --dest=org.kde.KWin "$@"; }
# KWin's D-Bus interfaces come up after its socket
for _ in $(seq 75); do
    kwin /Scripting org.kde.kwin.Scripting.isScriptLoaded string:none >/dev/null 2>&1 && break
    sleep 0.2
done

WAYLAND_DISPLAY=wayland-k "$bin" -S 4 "$images" &
corner=$!
WAYLAND_DISPLAY=wayland-k "$bin" -S 4 "$images" &
parked=$!
echo "const corner = $corner, parked = $parked, outputs = $outputs, width = 1280, height = 800;" >"$observer"
cat >>"$observer" <<'EOF'
function show(r) { return [r.x, r.y, r.width, r.height].map(Math.round).join(" "); }
function later(ms, f) {
    const t = new QTimer();
    t.singleShot = true;
    t.interval = ms;
    t.timeout.connect(f);
    t.start();
}
function watch(w) {
    if (w.pid !== corner && w.pid !== parked) return;
    const name = w.pid === corner ? "corner" : "parked";
    // the viewer's window first shows at the small image's size with the bar below it, taller
    // than wide, and is placed once it does
    let placed = false;
    function place() {
        const f = w.frameGeometry;
        if (placed || f.width >= 300 || f.height <= f.width) return;
        placed = true;
        const x = name === "corner" ? width - f.width - 10 : outputs * width - f.width / 2;
        w.frameGeometry = { x: x, y: height - f.height - 10, width: f.width, height: f.height };
    }
    // a change may be KWin's first placement, which goes on after its signal and would undo a
    // move made inside it
    w.frameGeometryChanged.connect(function () {
        console.info("nsxiv-test " + name + " " + show(w.frameGeometry));
        later(100, place);
    });
    place();
    if (name !== "parked") return;
    let trip = false;
    w.frameGeometryChanged.connect(function () {
        if (!placed || trip || w.fullScreen || w.frameGeometry.width < 1000) return;
        trip = true;
        later(500, function () {
            console.info("nsxiv-test before-fullscreen " + show(w.frameGeometry));
            w.fullScreen = true;
            later(500, function () {
                w.fullScreen = false;
                later(500, function () { console.info("nsxiv-test after-fullscreen " + show(w.frameGeometry)); });
            });
        });
    });
}
(workspace.windowList ? workspace.windowList() : workspace.clientList()).forEach(watch);
(workspace.windowAdded || workspace.clientAdded).connect(watch);
EOF
id=$(kwin /Scripting org.kde.kwin.Scripting.loadScript string:"$observer" string:observer | awk 'END { print $2 }')
case $id in '' | *[!0-9]*) fail "KWin did not load the observer: '$id'" ;; esac
# Plasma 6 puts a script at /Scripting/Script<id>, Plasma 5 at /<id>
kwin "/Scripting/Script$id" org.kde.kwin.Script.run >/dev/null 2>&1 || kwin "/$id" org.kde.kwin.Script.run >/dev/null

logged() { sed -n "s/.*nsxiv-test $1 //p" "$log"; }
# the large image shows from 4 to 8 seconds; the fullscreen trip ends about 1.5 seconds later
sleep 4
for _ in $(seq 40); do
    [ -n "$(logged after-fullscreen)" ] && break
    sleep 0.2
done
for viewer in $corner $parked; do
    running "$viewer" || fail "viewer $viewer ended"
    kwin /Scripting org.kde.kwin.Scripting.isScriptLoaded string:"nsxiv-ng-$viewer" | grep -q 'boolean true' ||
        fail "viewer $viewer loaded no KWin script"
done
echo "$mode: corner: $(logged corner | tr '\n' ';')"
echo "$mode: parked: $(logged parked | tr '\n' ';')"

# the corner window ran past the first screen's edge as it grew, then came back inside it
logged corner | awk '$3 >= 1000 && ($1 + $3 > 1280 || $2 + $4 > 800) { found = 1 } END { exit !found }' ||
    fail "the corner window never ran past the edge, so nothing was tested"
grown=$(logged corner | awk '$3 >= 1000' | tail -1) # the slideshow returns to the small image at 8 seconds
# shellcheck disable=SC2086
set -- $grown
[ $# -eq 4 ] && [ "$1" -ge 0 ] && [ "$2" -ge 0 ] && [ $(($1 + $3)) -le 1280 ] && [ $(($2 + $4)) -le 800 ] ||
    fail "the grown corner window is not inside its screen: $grown"

# the parked window grew where the user left it, and fullscreen brought it back there
before=$(logged before-fullscreen | tail -1)
after=$(logged after-fullscreen | tail -1)
echo "$mode: parked before fullscreen: $before; after: $after"
# shellcheck disable=SC2086
set -- $before
[ $# -eq 4 ] && [ "$3" -ge 1000 ] && [ $(($1 + $3)) -gt $((outputs * 1280)) ] ||
    fail "the parked window was pulled in: $before"
[ "$after" = "$before" ] || fail "fullscreen moved the parked window from $before to $after"

kill -TERM "$corner" "$parked"
for viewer in $corner $parked; do
    ended "$viewer" 10 || fail "viewer $viewer did not end on SIGTERM"
done
# KWin unloads a script once it has removed the window, which a busy KWin takes seconds for
for viewer in $corner $parked; do
    for _ in $(seq 50); do
        kwin /Scripting org.kde.kwin.Scripting.isScriptLoaded string:"nsxiv-ng-$viewer" | grep -q 'boolean false' && continue 2
        sleep 0.2
    done
    fail "the KWin script of viewer $viewer outlived its window"
done
corner=
parked=
echo "$mode: the corner window came back inside its screen, the parked one stayed, and both scripts unloaded"
