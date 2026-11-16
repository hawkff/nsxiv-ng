#!/bin/sh
# Runs the viewer as a Wayland client of niri, nested in a headless sway, in mode $2. In
# "tiled", at scale 1.5, in a column whose width niri leaves to the window: the window hugs the
# 640x400 gradient at 100% in the tile's height, then again after fullscreen and back, j names
# the window after the next image, and niri's close ends the viewer. In "floating", a window
# rule opens the window 300 pixels from the output's bottom-right corner, and the slideshow's
# second image needs most of the output: the window must grow into the room it probes and stay
# inside niri's output, which niri's IPC tells. As the window's first frame has the small
# image's size, the rule places it the same every time, so it grows to the same room.
#
# runs on: arch
# arch packages: niri sway grim wtype jq imagemagick mesa
# modes: tiled floating
set -eu
bin=$1
mode=${2:-floating}
pics=$(mktemp -d)
host_compositor=
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
# this run's own processes, the viewer then the compositors, and its folder go on every way
# out: TERM, then KILL after a grace
stop() {
    for process in $pid $compositor $host_compositor; do
        kill -TERM "$process" 2>/dev/null || true
        ended "$process" 4 || { kill -KILL "$process" 2>/dev/null; wait "$process" 2>/dev/null; } || true
    done
    rm -rf "$pics"
}
trap stop EXIT
trap 'exit 1' HUP INT TERM
fail() {
    echo "niri $mode: $1" >&2
    tail -20 "$pics/compositor.log" >&2 || true
    exit 1
}
images=$pics/images
mkdir -p "$images"
case $mode in
tiled)
    magick -size 640x400 gradient:red-blue "$images/1-gradient.png"
    magick -size 300x300 plasma: "$images/2-plasma.jpg"
    ;;
floating)
    magick -size 200x200 xc:green "$images/1-small.png"
    magick -size 1200x700 gradient:red-blue "$images/2-large.png"
    ;;
*) fail "no mode $mode" ;;
esac

export XDG_RUNTIME_DIR="$pics/runtime" XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_CURRENT_DESKTOP=niri
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK NIRI_SOCKET
# copies without the file capabilities a container cannot grant
mkdir -p "$pics/bin"
for program in sway niri; do
    install -m755 "$(command -v "$program")" "$pics/bin/$program"
done

# the newest wayland-N socket in XDG_RUNTIME_DIR other than $1
next_socket() {
    for _ in $(seq 200); do
        for candidate in "$XDG_RUNTIME_DIR"/wayland-[0-9]; do
            if [ -S "$candidate" ] && [ "$(basename "$candidate")" != "${1:-}" ]; then
                basename "$candidate"
                return
            fi
        done
        sleep 0.1
    done
    fail "no Wayland socket appeared"
}

echo 'output HEADLESS-1 resolution 1600x1000' >"$pics/sway.conf"
WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    "$pics/bin/sway" -c "$pics/sway.conf" >"$pics/sway.log" 2>&1 &
host_compositor=$!
host=$(next_socket)
case $mode in
tiled)
    cat >"$pics/niri.kdl" <<'EOF'
output "winit" {
    scale 1.5
}
window-rule {
    match app-id="nsxiv-ng"
    default-column-width {}
}
hotkey-overlay {
    skip-at-startup
}
EOF
    ;;
floating)
    cat >"$pics/niri.kdl" <<'EOF'
window-rule {
    match app-id="nsxiv-ng"
    open-floating true
    default-floating-position x=300 y=300 relative-to="bottom-right"
}
hotkey-overlay {
    skip-at-startup
}
EOF
    ;;
esac
WAYLAND_DISPLAY=$host LIBGL_ALWAYS_SOFTWARE=1 "$pics/bin/niri" -c "$pics/niri.kdl" >"$pics/compositor.log" 2>&1 &
compositor=$!
nested=$(next_socket "$host")
for _ in $(seq 100); do
    for candidate in "$XDG_RUNTIME_DIR"/niri.*.sock; do [ -S "$candidate" ] && NIRI_SOCKET=$candidate; done
    [ -n "${NIRI_SOCKET:-}" ] && break
    sleep 0.1
done
[ -n "${NIRI_SOCKET:-}" ] || fail "niri opened no IPC socket"
export NIRI_SOCKET

# the viewer's window as niri lists it: title, floating, and its tile's place and size in
# niri's logical units, which may be fractional
window() {
    niri msg --json windows | jq -c '.[] | select(.app_id == "nsxiv-ng") | .layout as $l |
        {title, floating: .is_floating, x: $l.tile_pos_in_workspace_view[0], y: $l.tile_pos_in_workspace_view[1],
         width: $l.tile_size[0], height: $l.tile_size[1], window: $l.window_size}'
}
# waits for the window to satisfy jq's condition $1, which leaves its record in $record, else
# fails with $2
expect() {
    for _ in $(seq 100); do
        record=$(window)
        [ "$(echo "$record" | jq "$1" 2>/dev/null)" = true ] && return
        running "$pid" || fail "the viewer ended"
        sleep 0.1
    done
    fail "$2: $(window)"
}

if [ "$mode" = tiled ]; then
    WAYLAND_DISPLAY=$nested timeout --kill-after=2 60 "$bin" "$images" &
    pid=$!
    # the column is as wide as the 640x400 gradient at 100%, the zoom it gets in a 617-high
    # tile, and the window keeps the tile's height
    expect '.window == [640, 617]' "the window does not hug the gradient at 640x617"
    niri msg action fullscreen-window
    expect '.window != [640, 617]' "the window did not go fullscreen"
    niri msg action fullscreen-window
    expect '.window == [640, 617]' "out of fullscreen the window does not hug the gradient again"
    WAYLAND_DISPLAY=$nested wtype -s 300 j
    # the viewer names its window after the image it shows
    expect '.title == "2-plasma.jpg"' "j did not show the next image"
    niri msg action close-window
    ended "$pid" 10 || fail "niri's close did not end the viewer"
    pid=
    [ "$status" -eq 0 ] || fail "niri's close ended the viewer with status $status"
    echo "niri tiled: the window hugged the gradient at 640x617, before and after fullscreen, and niri's close ended it"
    exit 0
fi

WAYLAND_DISPLAY=$nested "$bin" -S 4 "$images" &
pid=$!
# the small image first, in a window as wide as it, so a window that never fits the image fails
expect '.title == "1-small.png" and .window[0] == 200' "the small window never showed"
small=$record
echo "niri floating: small window $small"
# then the large image in the grown window, which must be inside the output on the same record
expect '.title == "2-large.png" and .width > 400' "the window did not grow for the large image"
window=$record
output=$(niri msg --json outputs | jq -c '[.[] | .logical][0] | {width, height}')
echo "niri floating: output $output; window $window"
# where the rule put the small window, the large image at 100% would cross the output's edge,
# so a window that stays inside the output limited its growth
near=$(jq -n --argjson o "$output" --argjson w "$small" \
    '$w.x + 1200 > $o.width or $w.y + 700 > $o.height')
[ "$near" = true ] || fail "the rule left the small window room for the large image: $small"
problem=$(jq -rn --argjson o "$output" --argjson w "$window" '
    if $w.title != "2-large.png" then "the viewer did not show the large image"
    elif ($w.floating | not) then "the window is not floating"
    elif $w.width <= 400 then "the window did not grow into the room it had"
    elif $w.x < 0 or $w.y < 0 or $w.x + $w.width > $o.width or $w.y + $w.height > $o.height
    then "the grown window ran past the output"
    else empty end')
[ -z "$problem" ] || fail "$problem"
# the rule placed the small window by its first frame, of its final size with its title bar
# and the status bar, its bottom-right corner 300,300 from the output's; the grown window keeps
# its top-left corner and reaches the output's right and bottom edges, the room it probed
placed=$(jq -n --argjson o "$output" --argjson s "$small" --argjson w "$window" '
    $s.x + $s.width == $o.width - 300 and $s.y + $s.height == $o.height - 300
    and $w.x == $s.x and $w.y == $s.y
    and $w.x + $w.width == $o.width and $w.y + $w.height == $o.height')
[ "$placed" = true ] || fail "the rule placed the small window off its corner, or it grew short of the output's edges"
echo "niri floating: the window grew to $(echo "$window" | jq -r '"\(.width)x\(.height) at \(.x),\(.y)"') inside the output"
