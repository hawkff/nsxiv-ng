#!/bin/sh
# Runs the viewer in a headless mutter, GNOME's compositor, on $2 side-by-side 1280x800
# monitors, and checks that a window that grows for a larger image stays on screen. mutter puts
# the small first window in the middle of a monitor, and the slideshow's second image needs most
# of it: grown from its top-left corner, the window would run past the monitor's bottom-right
# edge, and mutter must move it back. mutter keeps it on the screens, not on its first monitor:
# with two, it may spread onto the second. The viewer probes no room here, as mutter keeps the
# window on screen itself. mutter decorates no window, so the viewer draws its own title bar,
# whose rows its window takes beside the status bar's, for the small image and the large one
# alike. It runs in a D-Bus session of its own, which mutter needs.
# In mode "pointer", on one monitor, a pointer through mutter's remote desktop interface, which
# a small client of this script's keeps for the run: a click on the right of the image shows the
# next one, a double click on the title bar maximizes the window and another restores it, a
# drag on the title bar moves it, and a click on the close button ends the viewer as q does.
#
# packages: mutter dbus-daemon imagemagick python3-dbus
# modes: 1 2 pointer
set -eu
# a bus of this run's own, ended with it
[ "${MUTTER_SMOKE_BUS:-}" = 1 ] || MUTTER_SMOKE_BUS=1 exec dbus-run-session -- sh "$0" "$@"
bin=$1
mode=mutter-${2:-1}
case ${2:-1} in
pointer) monitors=1 ;;
*) monitors=${2:-1} ;;
esac
pics=$(mktemp -d)
compositor=
pid=
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
# this run's own processes, the viewer then the compositor, and its folder go on every way
# out: TERM, then KILL after a grace
stop() {
    for process in $pid $compositor; do
        kill -TERM "$process" 2>/dev/null || true
        ended "$process" 4 || { kill -KILL "$process" 2>/dev/null; wait "$process" 2>/dev/null; } || true
    done
    rm -rf "$pics"
}
trap stop EXIT
# a write to the pointer's client after it ended ends the run too, with its cleanup
trap 'exit 1' HUP INT TERM PIPE
log=$pics/compositor.log
fail() {
    echo "$mode: $1" >&2
    tail -40 "$log" >&2 || true
    if [ -s "$pics/pointer.log" ]; then
        echo "$mode: the pointer's client logged:" >&2
        cat "$pics/pointer.log" >&2
    fi
    exit 1
}
images=$pics/images
mkdir -p "$images"
magick -size 200x200 xc:green "$images/1-small.png"
magick -size 1200x700 gradient:red-blue "$images/2-large.png"

export XDG_RUNTIME_DIR="$pics/runtime" XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_CURRENT_DESKTOP=GNOME
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK
set --
for _ in $(seq "$monitors"); do set -- "$@" --virtual-monitor 1280x800; done
# mutter logs where a window asks to go, then where each constraint leaves it
MUTTER_DEBUG=geometry mutter --headless "$@" --no-x11 --wayland-display=wayland-m >"$log" 2>&1 &
compositor=$!
for _ in $(seq 100); do
    [ -S "$XDG_RUNTIME_DIR/wayland-m" ] && break
    running "$compositor" || fail "mutter did not start"
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/wayland-m" ] || fail "mutter opened no socket"

# mutter names each window it constrains, as W1 (title); the viewer's own comes first, or it is
# the one $window names. For that window, from the small image's size on, which mutter may
# constrain once before the window shows it: where it asked to go, and where each constraint
# left it
viewer() {
    seen "$1" | awk '$3 == 200 { small = 1 } small'
}
window=
seen() {
    awk -v kind="$1" -v first="$window" '
        / Constraining / {
            for (i = 1; i < NF; i++) if ($i == "Constraining") desc = $(i + 1)
            if (first == "") first = desc
            if (kind == "asked" && desc == first) { split($(NF - 1), p, ","); split($NF, z, "x"); print p[1], p[2], z[1], z[2] }
        }
        /info->current is/ && desc == first && kind == "placed" {
            for (i = 1; i < NF; i++) if ($i == "is") { split($(i + 1), p, ","); split($(i + 2), z, "x"); print p[1], p[2], z[1], z[2] }
        }' "$log"
}

# A pointer that mutter moves and clicks for the client that made the remote desktop session,
# as long as that client is connected: commands on its input, one a line, "to X Y" moving
# to a place on the screen, "by DX DY" moving by so much, "down" and "up" with "left" or
# "right" pressing and releasing a button, "click" doing both, its press taken before its
# release comes, as from a hand, and "double" two clicks well within a double click's 400 ms,
# far enough apart that mutter ends the move the first one starts on a title bar. It answers
# each once it is done with a line.
cat >"$pics/pointer.py" <<'EOF'
import sys
import time
import dbus

bus = dbus.SessionBus()
name = "org.gnome.Mutter.RemoteDesktop"
path = bus.get_object(name, "/org/gnome/Mutter/RemoteDesktop").CreateSession(dbus_interface=name)
session = dbus.Interface(bus.get_object(name, path), name + ".Session")
session.Start()
for line in sys.stdin:
    command = line.split()
    if command[0] == "to":
        # the pointer stops at the screen's top-left corner, then goes to the place
        session.NotifyPointerMotionRelative(-10000.0, -10000.0)
        session.NotifyPointerMotionRelative(float(command[1]), float(command[2]))
    elif command[0] == "by":
        session.NotifyPointerMotionRelative(float(command[1]), float(command[2]))
    else:
        # BTN_LEFT and BTN_RIGHT
        button = 0x110 if command[1] == "left" else 0x111
        presses = {"down": [True], "up": [False]}.get(command[0], [True, False])
        if command[0] == "double":
            presses += presses
        for n, down in enumerate(presses):
            if n:
                time.sleep(0.15 if down else 0.05)
            session.NotifyPointerButton(button, down)
    print("done", line.strip(), flush=True)
EOF
# the viewer's window as mutter last placed it, as x y width height
placed() { seen placed | tail -1; }
# waits for the window's placement to be $1, else fails with $2
expect_placed() {
    for _ in $(seq 50); do
        [ "$(placed)" = "$1" ] && return
        sleep 0.1
    done
    fail "$2: the window is at $(placed), not $1"
}
# waits for the window's placement to be $1, its place within a pixel each way and its size
# exactly, else fails with $2: mutter moves a window dragged by its title bar by its own
# pointer's motion, the viewer giving no place for it, and on arm64 that motion's arithmetic
# rounds a pixel apart from amd64's
expect_near() {
    for _ in $(seq 50); do
        echo "$(placed) $1" | awk '{ exit !($1 - $5 <= 1 && $5 - $1 <= 1 && $2 - $6 <= 1 && $6 - $2 <= 1 && $3 == $7 && $4 == $8) }' &&
            return
        sleep 0.1
    done
    fail "$2: the window is at $(placed), not within a pixel of $1"
}
# the pointer's commands, each a line, each returning once the client is done with it
point() {
    for command; do
        echo "$command" >&3
        read -r answer <&4 || fail "the pointer's client ended before it did $command"
    done
}
pointer() {
    WAYLAND_DISPLAY=wayland-m "$bin" "$images" &
    pid=$!
    small
    mkfifo "$pics/commands" "$pics/answers"
    python3 "$pics/pointer.py" <"$pics/commands" >"$pics/answers" 2>"$pics/pointer.log" &
    exec 3>"$pics/commands" 4<"$pics/answers"
    # the first motion plugs the pointer in; the viewer binds it before the first click
    point "to 1 1"
    sleep 1
    set -- $(placed)
    # the right of the image, below the title bar, goes on to the next image, whose name
    # mutter logs as the window grows for it
    point "to $(($1 + $3 - 20)) $(($2 + $4 / 2))" "click left"
    for _ in $(seq 50); do
        grep -q "Constraining $window (2-large.png)" "$log" && break
        sleep 0.1
    done
    grep -q "Constraining $window (2-large.png)" "$log" || fail "a click on the right of the image did not show the next one"
    sleep 1
    set -- $(placed)
    # up and left, where the close button stays on the screen
    # the viewer asks mutter to move the window on the press, before the pointer moves
    point "to $(($1 + 60)) $(($2 + 10))" "down left"
    sleep 0.2
    point "by -10 -5" "by -20 -10" "by -30 -15"
    sleep 0.2
    point "up left"
    expect_near "$(($1 - 60)) $(($2 - 30)) $3 $4" "a drag on the title bar did not move the window"
    sleep 0.5
    # the close button, at the title bar's right end
    point "to $(($1 - 60 + $3 - 12)) $(($2 - 30 + 12))" "click left"
    ended "$pid" 10 || fail "a click on the close button did not end the viewer"
    pid=
    [ "$status" -eq 0 ] || fail "the close button ended the viewer with status $status"

    # mutter logs no placement for a window it restores, so the viewer's protocol log tells
    WAYLAND_DISPLAY=wayland-m WAYLAND_DEBUG=client "$bin" "$images" 2>"$pics/wire" 3>&- 4<&- &
    pid=$!
    small
    point "to 1 1"
    sleep 1
    set -- $(placed)
    point "to $(($1 + 60)) $(($2 + 10))" "double left"
    expect_placed "0 0 1280 800" "a double click on the title bar did not maximize the window"
    point "to 60 10" "double left"
    # mutter's answer: a configure that sizes the window smaller than the screen
    restored() {
        awk '/unset_maximized/ { on = 1 }
            on && /xdg_toplevel@[0-9]+\.configure, \(/ { sub(/.*configure, \(/, ""); if ($1 + 0 < 1280) found = 1 }
            END { exit !found }' "$pics/wire"
    }
    for _ in $(seq 50); do
        restored && break
        sleep 0.1
    done
    restored || fail "a second double click on the title bar did not restore the window"
    sleep 0.5
    running "$pid" || fail "the viewer ended after the restore"
    echo "$mode: the pointer went on to the next image, moved the window and closed it, and maximized and restored another"
}

# waits for a viewer's window to take the small image's size, and names it in $window
small() {
    window=
    for _ in $(seq 50); do
        window=$(grep -o 'Constraining W[0-9]* (1-small.png)' "$log" | tail -1 | cut -d' ' -f2)
        [ -n "$window" ] && [ "$(placed | cut -d' ' -f3)" = 200 ] && return
        running "$pid" || fail "the viewer ended"
        sleep 0.1
    done
    fail "no window took the small image's size"
}

if [ "$mode" = mutter-pointer ]; then
    pointer
    exit 0
fi

WAYLAND_DISPLAY=wayland-m "$bin" -S 4 "$images" &
pid=$!
# the rows the latest request $1 wide had beside an image $2 high: the title bar's and the
# status bar's, about 27 each, once the fonts are in
rows() { viewer asked | awk -v w="$1" -v h="$2" '$3 == w { rows = $4 - h } END { print rows }'; }
# the large image shows for four seconds after the small one; the fonts may come after the first
# small window, so the slideshow may go round again until a small window, a grown one and its
# placement all have the rows
for _ in $(seq 100); do
    sleep 0.25
    running "$pid" || fail "the viewer ended"
    small=$(rows 200 200)
    grown=$(rows 1200 700)
    final=$(viewer placed | awk '$3 > 400' | tail -1)
    [ "${small:-0}" -gt 40 ] && [ "$grown" = "$small" ] &&
        [ -n "$final" ] && [ "$(echo "$final" | awk '{ print $4 - 700 }')" = "$small" ] && break
done

past=$(viewer asked | awk -v w=1280 '$3 > 400 && ($1 % w + $3 > w || $2 + $4 > 800)' | head -1)
echo "$mode: the viewer asked for: $(viewer asked | tr '\n' ';')"
echo "$mode: asked past an edge: ${past:-never}; placed at: ${final:-nothing}"
echo "$mode: rows beside the image: ${small:-none} small, ${grown:-none} grown"
[ -n "$final" ] || fail "the window did not grow for the large image"
[ -n "$past" ] || fail "the window never asked to run past an edge, so mutter was not tested"
[ "${small:-0}" -gt 40 ] ||
    fail "the window has ${small:-no} rows beside the image, too few for a title bar and the status bar"
[ "$grown" = "$small" ] || fail "the grown window has $grown rows beside the image, the small one $small"
# shellcheck disable=SC2086
set -- $final
[ "$1" -ge 0 ] && [ "$2" -ge 0 ] && [ $(($1 + $3)) -le $((monitors * 1280)) ] && [ $(($2 + $4)) -le 800 ] ||
    fail "the grown window ran past the screen's edge"
