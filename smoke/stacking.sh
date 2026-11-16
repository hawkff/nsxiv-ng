#!/bin/sh
# Runs the viewer in a headless labwc, Wayfire or river, compositors that keep a window's
# top-left corner when it grows and give a client no way to move itself, in mode $2: the
# compositor and the count of side-by-side 1280x720 screens, as "labwc-2". A window rule puts
# the viewer near the bottom-right corner of the last screen; the slideshow's second image needs
# most of a screen, and the viewer must grow to that screen's edge and no further, from the
# work area it probes with a popup. That image has four colored quarters: a screenshot shows all
# four at the same size only when no part of the window ran past the edge, and the status bar
# below them shows as many rows as below the small image, above the bottom edge. The small window
# must show first, so a window that never fits the image fails. Wayfire refuses to run as root,
# so there the script runs as the user nobody.
#
# runs on: debian arch
# packages: labwc grim imagemagick
# modes: labwc-1 labwc-2
# arch packages: wayfire river-classic grim imagemagick util-linux
# arch modes: wayfire-1 river-1
set -eu
bin=$1
mode=${2:-labwc-1}
compositor_name=${mode%-*}
if [ "$compositor_name" = wayfire ] && [ "$(id -u)" -eq 0 ]; then
    exec runuser -u nobody -- env HOME=/tmp sh "$0" "$@"
fi
outputs=${mode##*-}
right=$((outputs * 1280))
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
trap 'exit 1' HUP INT TERM
fail() {
    echo "$mode: $1" >&2
    tail -20 "$pics/compositor.log" >&2 || true
    exit 1
}
images=$pics/images
mkdir -p "$images"
magick -size 200x200 xc:green "$images/1-small.png"
magick \( -size 600x350 xc:magenta xc:yellow +append \) \
    \( -size 600x350 xc:cyan xc:blue +append \) -append "$images/2-large.png"

export XDG_RUNTIME_DIR="$pics/runtime" XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
mkdir -m 700 "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=$outputs
# what each compositor puts in the session it starts; a copy of it without the file
# capabilities a container cannot grant
mkdir -p "$pics/bin"
install -m755 "$(command -v "$compositor_name")" "$pics/bin/$compositor_name"
case $compositor_name in
labwc)
    export XDG_CURRENT_DESKTOP=labwc:wlroots
    mkdir -p "$pics/labwc"
    cat >"$pics/labwc/rc.xml" <<EOF
<?xml version="1.0"?>
<labwc_config>
  <windowRules>
    <windowRule identifier="nsxiv-ng">
      <action name="MoveTo" x="$((right - 280))" y="420" />
    </windowRule>
  </windowRules>
</labwc_config>
EOF
    "$pics/bin/labwc" -C "$pics/labwc" >"$pics/compositor.log" 2>&1 &
    ;;
wayfire)
    export XDG_CURRENT_DESKTOP=wayfire
    cat >"$pics/wayfire.ini" <<EOF
[core]
plugins = window-rules
xwayland = false

[window-rules]
park = on created if app_id is "nsxiv-ng" then move $((right - 280)) 420
EOF
    "$pics/bin/wayfire" -c "$pics/wayfire.ini" >"$pics/compositor.log" 2>&1 &
    ;;
river)
    export XDG_CURRENT_DESKTOP=river
    # the init command marks that its rules are in, as riverctl's listing of them may come
    # back empty
    "$pics/bin/river" -c "riverctl rule-add -app-id nsxiv-ng float && \
        riverctl rule-add -app-id nsxiv-ng position $((right - 280)) 420 && touch $pics/ruled" \
        >"$pics/compositor.log" 2>&1 &
    ;;
*) fail "no compositor $compositor_name" ;;
esac
compositor=$!
for _ in $(seq 100); do
    for candidate in "$XDG_RUNTIME_DIR"/wayland-[0-9]; do
        [ -S "$candidate" ] && WAYLAND_DISPLAY=$(basename "$candidate")
    done
    [ -n "${WAYLAND_DISPLAY:-}" ] && break
    running "$compositor" || fail "$compositor_name did not start"
    sleep 0.1
done
[ -n "${WAYLAND_DISPLAY:-}" ] || fail "$compositor_name opened no socket"
export WAYLAND_DISPLAY
# river's socket comes before its init command has added the rules
if [ "$compositor_name" = river ]; then
    for _ in $(seq 50); do
        [ -e "$pics/ruled" ] && break
        sleep 0.1
    done
    [ -e "$pics/ruled" ] || fail "river did not add the rules"
fi

"$bin" -S 4 "$images" &
pid=$!
shot=$pics/shot.png

# the bounding box of each quarter's color, as width height x y, or 1 1 -1 -1 when the color
# is missing; a capture of an XRGB output may carry any alpha
box() {
    magick "$shot" -alpha off -fuzz 3% -fill black +opaque "$1" -fill white -opaque "$1" -trim \
        -format '%w %h %X %Y' info: 2>/dev/null | tr -d '+'
}
# the status bar's rows below the image from x $1, $2 wide, whose bottom is at y $3: the rows of
# the bar's color in that strip down to the screen's bottom, as height y
# the strip's border is black, so -trim takes it off and leaves the bar's box
bar() {
    magick "$shot" -crop "$2x$((720 - $3))+$1+$3" +repage -alpha off -fuzz 1% -fill black \
        +opaque '#eeeeee' -fill white -opaque '#eeeeee' -bordercolor black -border 1 -trim \
        -format '%h %Y' info: 2>/dev/null | tr -d '+' | awk -v top="$3" '{ print $1, $2 - 1 + top }'
}
# the small image first, whole at 200x200, with the status bar below it once the fonts are in
rows=0
for _ in $(seq 40); do
    sleep 0.1
    running "$pid" || fail "the viewer ended"
    grim "$shot" 2>/dev/null || continue
    set -- $(box '#008000')
    [ "${1:-0}" -eq 200 ] && [ "$2" -eq 200 ] || continue
    rows=$(bar "$3" 200 $(($4 + 200)) | cut -d' ' -f1)
    [ "$rows" -gt 10 ] && break
done
[ "${1:-0}" -eq 200 ] && [ "$2" -eq 200 ] && [ "$rows" -gt 10 ] ||
    fail "the small window never showed the small image whole with the status bar below it"
echo "$mode: small image at $3,$4, status bar $rows rows"
# the large image shows for four seconds once the viewer is up; wait for a screenshot of it in
# the grown window, as the small window shows each quarter at most 100 pixels wide
for _ in $(seq 60); do
    sleep 0.25
    running "$pid" || fail "the viewer ended"
    grim "$shot" 2>/dev/null || continue
    set -- $(box magenta)
    [ "${1:-0}" -gt 100 ] && break
done
for color in magenta yellow cyan blue; do
    set -- $(box "$color")
    echo "$mode: $color ${1}x$2 at $3,$4"
    eval "w_$color=$1 h_$color=$2 right_$color=$(($3 + $1)) bottom_$color=$(($4 + $2))"
done
[ "$w_magenta" -gt 100 ] || fail "the large image is not on screen"
# every quarter whole, so none was cut by the screen's edge, and the status bar below the image
# inside the screen too
for color in yellow cyan blue; do
    eval "dw=\$((w_$color - w_magenta)) dh=\$((h_$color - h_magenta))"
    [ "${dw#-}" -le 1 ] && [ "${dh#-}" -le 1 ] || fail "the $color quarter is cut: the window ran past the edge"
done
set -- $(bar $((right_blue - w_magenta - w_blue)) $((w_magenta + w_blue)) "$bottom_blue")
echo "$mode: status bar ${1:-0} rows at ${2:-none}"
[ "${1:-0}" -eq "$rows" ] && [ "$right_blue" -le "$right" ] ||
    fail "the status bar ran past the edge: ${1:-0} of its $rows rows show"
# the image is as wide as the window, which grew to the screen's right edge
[ "$right_blue" -ge $((right - 10)) ] || fail "the window did not grow to the screen's edge"
echo "$mode: the window grew to x $right_blue of $right, y $bottom_blue of 720, and no further"
