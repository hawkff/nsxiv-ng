#!/bin/sh
# Runs the viewer in COSMIC's compositor, nested in a headless sway. COSMIC puts the small first
# window in the middle of its screen; the slideshow's second image is larger than the screen, so
# a window that kept its top-left corner and grew to fit it would run past the bottom-right
# edge. That image has four colored quarters: a screenshot shows all four at the same size only
# when the image did not run past an edge, and the status bar below it shows whole above the
# bottom edge; the window grows to the screen's right edge. The small window must show first,
# so a window that never fits the image fails. It runs in a D-Bus session of its own, which
# COSMIC needs.
#
# runs on: arch
# arch packages: cosmic-comp sway grim imagemagick mesa dbus
set -eu
# a bus of this run's own, ended with it
[ "${COSMIC_SMOKE_BUS:-}" = 1 ] || COSMIC_SMOKE_BUS=1 exec dbus-run-session -- sh "$0" "$@"
bin=$1
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
    echo "cosmic: $1" >&2
    tail -20 "$pics/compositor.log" >&2 || true
    exit 1
}
images=$pics/images
mkdir -p "$images"
magick -size 200x200 xc:green "$images/1-small.png"
magick \( -size 800x500 xc:magenta xc:yellow +append \) \
    \( -size 800x500 xc:cyan xc:blue +append \) -append "$images/2-large.png"

export XDG_RUNTIME_DIR="$pics/runtime" XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_CURRENT_DESKTOP=COSMIC
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK
# copies without the file capabilities a container cannot grant
mkdir -p "$pics/bin"
for program in sway cosmic-comp; do
    install -m755 "$(command -v "$program")" "$pics/bin/$program"
done

# the newest wayland-N socket in XDG_RUNTIME_DIR other than $1
next_socket() {
    for _ in $(seq 300); do
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

# with no title bar or border, COSMIC's screen is the whole of sway's
printf 'output HEADLESS-1 resolution 1280x800\ndefault_border none\n' >"$pics/sway.conf"
WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    "$pics/bin/sway" -c "$pics/sway.conf" >"$pics/sway.log" 2>&1 &
host_compositor=$!
host=$(next_socket)
WAYLAND_DISPLAY=$host LIBGL_ALWAYS_SOFTWARE=1 "$pics/bin/cosmic-comp" >"$pics/compositor.log" 2>&1 &
compositor=$!
nested=$(next_socket "$host")

WAYLAND_DISPLAY=$nested "$bin" -S 4 "$images" &
pid=$!
shot=$pics/shot.png

# the bounding box of a color within fuzz $2 (3% where none), as width height x y; 1 1 -1 -1
# when the color is missing
box() {
    magick "$shot" -alpha off -fuzz "${2:-3%}" -fill black +opaque "$1" -fill white -opaque "$1" -trim \
        -format '%w %h %X %Y' info: 2>/dev/null | tr -d '+'
}
# the small image first, whole at 200x200, but for the column at each side that COSMIC's
# outline of the window covers, in a window of its size: its status bar as wide as the image,
# whose rows are then the bar's whole height; a tight fuzz leaves out the light title text in
# COSMIC's header.
whole() { case $small in 19[89]\ 200\ * | 200\ 200\ *) return 0 ;; esac; return 1; }
small=
bar=
shown=
for _ in $(seq 60); do
    sleep 0.1
    running "$pid" || fail "the viewer ended"
    WAYLAND_DISPLAY=$host grim "$shot" 2>/dev/null || continue
    small=$(box '#008000')
    whole || continue
    shown=$small
    bar=$(box '#eeeeee' 1%)
    [ "${bar%% *}" = "${small%% *}" ] && break
done
[ -n "$shown" ] || fail "the small window never showed the small image whole"
set -- $bar
[ "$1" = "${shown%% *}" ] || fail "the window did not take the small image's size: its bar is $1 wide"
small=$shown
rows=$2
echo "cosmic: small image at $(echo "$small" | cut -d' ' -f3,4), status bar ${1}x$2"
# the large image shows for four seconds once the viewer is up; wait for a screenshot of it in
# the grown window, as the small window shows each quarter at most 100 pixels wide
for _ in $(seq 60); do
    sleep 0.25
    running "$pid" || fail "the viewer ended"
    WAYLAND_DISPLAY=$host grim "$shot" 2>/dev/null || continue
    set -- $(box magenta)
    [ "${1:-0}" -gt 100 ] && break
done
for color in magenta yellow cyan blue; do
    set -- $(box "$color")
    echo "cosmic: $color ${1}x$2 at $3,$4"
    eval "w_$color=$1 h_$color=$2"
done
[ "$w_magenta" -gt 100 ] || fail "the window did not grow for the large image"
for color in yellow cyan blue; do
    eval "dw=\$((w_$color - w_magenta)) dh=\$((h_$color - h_magenta))"
    [ "${dw#-}" -le 1 ] && [ "${dh#-}" -le 1 ] || fail "the $color quarter is cut: the window ran past the edge"
done
# the bar below the image whole, as many rows as below the small one, inside the screen, and
# as wide as the window, which grew to the screen's right edge, but for the column COSMIC's
# outline covers
set -- $(box '#eeeeee' 1%)
echo "cosmic: status bar ${1}x$2 at $3,$4"
[ "$2" -eq "$rows" ] && [ $(($4 + $2)) -le 800 ] && [ $(($3 + $1)) -le 1280 ] ||
    fail "the status bar ran past an edge: $2 of its $rows rows show"
[ $(($3 + $1)) -ge 1278 ] || fail "the window did not grow to the screen's right edge"
echo "cosmic: the grown window reached the screen's right edge, its quarters whole and its status bar inside the screen"
