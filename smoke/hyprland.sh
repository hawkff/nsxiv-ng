#!/bin/sh
# Runs the viewer in Hyprland on a virtual 1024x768 display, vkms, which a guest's kernel gives,
# as root. Hyprland sizes a window when it maps it and keeps that size when the client resizes
# itself later, cutting off what lies past it; it marks every window tiled and maximized, floating
# ones too, so for the viewer every size it gives binds. A window rule opens the viewer floating.
# The viewer must map at its first image's size, 200x200 with the bar's rows, and when the
# slideshow's second image needs most of the screen, once its frame came, fit it into the size
# Hyprland gave and never grow, nor ask for the room to: no buffer, viewport destination,
# minimum, maximum or window geometry of another size after the first frame, and no popup that
# would probe the room. Hyprland draws no frames on such a display, so the check reads hyprctl
# and the viewer's protocol log, whole once the viewer ended.
#
# runs on: arch
# guest: only
# arch packages: hyprland mesa jq imagemagick
set -eu
bin=$1
pics=$(mktemp -d)
# short, as Hyprland's socket lies two long names below it, and a socket's path is short
runtime=$(mktemp -d /tmp/h.XXXXXX)
hypr=
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
# this run's own processes, the viewer then Hyprland, so the display is free for the next run,
# then its folders, go on every way out: TERM, then KILL after a grace
stop() {
    for process in $pid $hypr; do
        kill -TERM "$process" 2>/dev/null || true
        ended "$process" 4 || { kill -KILL "$process" 2>/dev/null; wait "$process" 2>/dev/null; } || true
    done
    rm -rf "$pics" "$runtime"
}
trap stop EXIT
trap 'exit 1' HUP INT TERM
fail() {
    echo "hyprland: $1" >&2
    tail -30 "$pics/hyprland.log" "$XDG_RUNTIME_DIR"/hypr/*/hyprland.log >&2 2>/dev/null || true
    exit 1
}

modprobe vkms 2>/dev/null || true
[ -d /sys/module/vkms ] || fail "the kernel has no vkms, so no display for Hyprland"
magick -size 200x200 xc:green "$pics/1-small.png"
magick -size 1200x700 gradient:red-blue "$pics/2-large.png"

# all of it in this run's folder, as the guest's root is read-only; no seat manager runs here,
# so Hyprland, as root, opens the display itself
export HOME="$pics/home" XDG_RUNTIME_DIR="$runtime" XDG_CURRENT_DESKTOP=Hyprland
export XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache" LIBSEAT_BACKEND=noop
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
mkdir -p "$HOME"
chmod 700 "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY SWAYSOCK HYPRLAND_INSTANCE_SIGNATURE
cat >"$pics/hyprland.lua" <<'EOF'
hl.monitor({ output = "", mode = "preferred", position = "auto", scale = 1 })
hl.config({
    misc = { disable_hyprland_logo = true, disable_splash_rendering = true },
    debug = { disable_logs = false },
})
hl.window_rule({ name = "viewer", match = { class = "^(nsxiv-ng)$" }, float = true, move = "500 400" })
EOF
Hyprland --i-am-really-stupid --config "$pics/hyprland.lua" >"$pics/hyprland.log" 2>&1 &
hypr=$!
for _ in $(seq 150); do
    for dir in "$XDG_RUNTIME_DIR"/hypr/*; do
        [ -S "$dir/.socket.sock" ] && HYPRLAND_INSTANCE_SIGNATURE=$(basename "$dir")
    done
    [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] && break
    kill -0 "$hypr" 2>/dev/null || fail "Hyprland ended"
    sleep 0.2
done
[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] || fail "Hyprland did not start"
export HYPRLAND_INSTANCE_SIGNATURE
socket=
for candidate in "$XDG_RUNTIME_DIR"/wayland-[0-9]; do
    [ -S "$candidate" ] && socket=$(basename "$candidate")
done
[ -n "$socket" ] || fail "Hyprland opened no Wayland socket"

# a slideshow slow enough not to wrap back to the small image while the script looks
WAYLAND_DEBUG=client WAYLAND_DISPLAY=$socket timeout --kill-after=2 60 "$bin" -S 6 "$pics" \
    2>"$pics/wire" &
pid=$!
# the viewer's window, by the pid of the viewer, timeout's child
window() {
    viewer=$(pgrep -P "$pid" | head -1)
    hyprctl -j clients | jq -c --argjson pid "${viewer:-0}" '.[] | select(.pid == $pid)'
}
# Hyprland's size of the window
size() { window | jq -r '"\(.size[0])x\(.size[1])"'; }
# whether a whole frame of the window came after the viewer named its window $1: a frame that
# shows the image, not the bar alone, which comes with the name while the image decodes
framed() {
    sed -n "/set_title(Some(\"$1\"))/,\$p" "$pics/wire" |
        grep -q 'damage_buffer(0, 0, 2147483647, 2147483647)'
}
waits() {
    for _ in $(seq 100); do
        framed "$1" && return
        sleep 0.1
    done
    fail "no frame of $1 came: $(window)"
}
waits 1-small.png
sleep 1
small=$(size)
waits 2-large.png
# a probe for room, or a frame of another size, would have come by now
sleep 1
large=$(window | jq -r '"\(.floating) \(.size[0])x\(.size[1])"')
# the log whole, from a viewer that ended
kill -TERM "$pid"
ended "$pid" 5 || fail "the viewer did not end on TERM"
pid=
# 0 as on q; one that had to be killed after timeout's grace gives another
[ "$status" -eq 0 ] || fail "the viewer ended with status $status on TERM"
echo "hyprland: the window, floating and its size, $small at the small image, then $large"
[ "${large%% *}" = true ] || fail "the window is not floating"
[ "${large#* }" = "$small" ] || fail "Hyprland let the window grow from $small to ${large#* }"
grep -q 'set_title(Some("1-small.png"))' "$pics/wire" &&
    ! sed -n '/set_title(Some("2-large.png"))/,$p' "$pics/wire" | grep -q 'set_title(Some("1-small' ||
    fail "the slideshow went back to the small image before the look"
# The window's surface, from the xdg_surface made for it, and each of its commits that came
# with a buffer, by the size the buffer was made at; every destination of its viewport, and
# every size the toplevel or its geometry asked for. At scale 1 a buffer is the window's size.
asked=$(awk '{
    at = index($0, "-> "); if (!at) next
    split(substr($0, at + 3), a, /[@.(), ]+/)
    if (a[3] == "get_xdg_surface") surface = a[7]
    else if (a[3] == "create_buffer") size[a[5]] = a[7] "x" a[8]
    else if (a[1] == "wl_surface" && a[2] == surface && a[3] == "attach") on = a[5]
    else if (a[1] == "wl_surface" && a[2] == surface && a[3] == "commit" && on != "") {
        print "buffer " size[on]; on = ""
    }
    else if (a[3] == "set_destination") print "destination " a[4] "x" a[5]
    else if (a[3] == "set_min_size" || a[3] == "set_max_size") print a[3] " " a[4] "x" a[5]
    else if (a[3] == "set_window_geometry") print a[3] " " a[6] "x" a[7]
}' "$pics/wire")
first=$(echo "$asked" | sed -n 's/^buffer //p' | head -1)
# the first frame is the small image's, 200 wide at scale 1, and 200 high with the bar's rows,
# which the frame of the bar alone damaged, as the large image's name came
rows=$(sed -n '/set_title(Some("2-large.png"))/,$p' "$pics/wire" |
    grep -o 'damage_buffer(0, [1-9][0-9]*, [0-9]*, [0-9]*)' | sed -n '1s/.*, \([0-9]*\))/\1/p')
[ -n "$rows" ] || fail "no frame of the bar alone came with the large image's name"
[ "$first" = "200x$((200 + rows))" ] ||
    fail "the first frame was $first, not the small image's 200x200 with the bar's $rows rows"
[ "$first" = "$small" ] || fail "Hyprland mapped the window at $small, the viewer drew $first"
# then nothing of another size: every buffer, destination and size asked for is the first's,
# or none at all, as a minimum or maximum of 0x0
other=$(echo "$asked" | grep -v -e " $first\$" -e ' 0x0$' || true)
[ -z "$other" ] || fail "after the first frame, the viewer sent sizes other than $first: $(echo "$other" |
    sort | uniq -c | tr -s ' \n' ' ')"
if grep -q 'get_popup' "$pics/wire"; then
    fail "the viewer asked for room to grow: $(grep -c get_popup "$pics/wire") popups"
fi
echo "hyprland: $(echo "$asked" | grep -c '^buffer') frames, all $first as Hyprland gave, with no probe for room"
