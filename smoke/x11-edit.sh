#!/bin/sh
# Edit mode in Xvfb with no window manager: x on a 640x400 image, a filled rectangle by a drag,
# t and a click, typed words in the viewer's own text line and Return, then Control-c, the
# edit's copy: CLIPBOARD must offer image/png, which xclip reads back as a PNG of the image's
# size, the rectangle red in it, and a change where the words were typed, which a copy after
# Control-z lacks (a check of the region, not of the words); then w must save the edit beside
# the image, its pixels those of the copy after Control-z. A 3200x2400 image of noise, whose
# PNG is larger than the server's largest request, must come through CLIPBOARD too, by INCR,
# its pixels those of the image. Space held while a rectangle is drawn, then let go while the
# focus is elsewhere, must count as up once the focus is back. The bar, which starts hidden,
# is shown with b before an edit whose coordinates are the image's. Every wait is bounded.
#
# runs on: debian arch
# packages: imagemagick xvfb xdotool xclip x11-utils fonts-dejavu-core
# arch packages: imagemagick xorg-server-xvfb xdotool xclip xorg-xwininfo xorg-xdpyinfo xorg-xev ttf-dejavu
set -eu
bin=$1
pics=$(mktemp -d)
xvfb=
viewer=
helper=
# process $1 ends within 5 seconds of a TERM, else is killed, and is waited for
stop() {
    kill "$1" 2>/dev/null || return 0
    for _ in $(seq 50); do
        kill -0 "$1" 2>/dev/null || break
        sleep 0.1
    done
    kill -9 "$1" 2>/dev/null || true
    wait "$1" 2>/dev/null || true
}
trap '[ -z "$viewer" ] || stop "$viewer"; [ -z "$helper" ] || stop "$helper"; [ -z "$xvfb" ] || stop "$xvfb"; rm -rf "$pics"' EXIT
trap 'exit 1' HUP INT TERM
# xdotool, ended where the server stops answering
xdo() { timeout 10 xdotool "$@"; }
fail() {
    echo "x11-edit: $1" >&2
    [ ! -s "$pics/stderr" ] || sed 's/^/  viewer: /' "$pics/stderr" >&2
    exit 1
}


Xvfb -displayfd 3 -screen 0 1280x800x24 -nolisten tcp 3>"$pics/display" 2>"$pics/xvfb.log" &
xvfb=$!
for _ in $(seq 100); do
    [ -s "$pics/display" ] && break
    sleep 0.1
done
[ -s "$pics/display" ] || fail "Xvfb did not start: $(cat "$pics/xvfb.log")"
DISPLAY=:$(cat "$pics/display")
export DISPLAY
unset WAYLAND_DISPLAY
for _ in $(seq 100); do
    xdo getdisplaygeometry >/dev/null 2>&1 && break
    sleep 0.1
done
export XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"

# opens image $1, and waits for its window to show; with no window manager it is at 0,0
open() {
    timeout -k 5 120 "$bin" "$1" 2>"$pics/stderr" &
    viewer=$!
    window=
    for _ in $(seq 100); do
        window=$(xdo search --classname '^nsxiv-ng$' 2>/dev/null | head -1) &&
            [ -n "$window" ] && timeout 5 xwininfo -id "$window" | grep -q IsViewable && break
        sleep 0.1
    done
    [ -n "$window" ] || fail "no window of class nsxiv-ng came for $1"
    # the keys go where the pointer is
    xdo mousemove 320 380
    sleep 0.5
}
# b shows the bar, which starts hidden, so the window takes the image's 400 rows and the
# bar's below them, and the edit's canvas is the image's size, as the coordinates here take it
bar_shown() {
    xdo key b
    for _ in $(seq 50); do
        [ "$(xdo getwindowgeometry --shell "$window" | sed -n 's/^HEIGHT=//p')" -gt 400 ] &&
            return
        sleep 0.1
    done
    fail "b did not show the bar below the image"
}
# reads CLIPBOARD as image/png into $1, once its targets name it, which may take up to two
# minutes: the large copy encodes a 26 MB PNG first, which a debug build on a slow machine takes
# most of a minute for; a read that does not end in 30 seconds fails, as one the owner never
# finishes
paste_png() {
    listed=
    for _ in $(seq 600); do
        timeout 2 xclip -selection clipboard -t TARGETS -o 2>/dev/null | grep -qx image/png &&
            listed=yes && break
        sleep 0.2
    done
    [ -n "$listed" ] || fail "CLIPBOARD's TARGETS never named image/png: \
$(timeout 2 xclip -selection clipboard -t TARGETS -o 2>&1 | paste -sd ' ')"
    timeout 30 xclip -selection clipboard -t image/png -o >"$1" 2>"$pics/xclip" && [ -s "$1" ] ||
        fail "CLIPBOARD gave no image/png: $(cat "$pics/xclip"), targets \
$(timeout 2 xclip -selection clipboard -t TARGETS -o 2>&1 | paste -sd ' ')"
}
# the viewer ends within 10 seconds, with status 0
quits() {
    for _ in $(seq 100); do
        kill -0 "$viewer" 2>/dev/null || break
        sleep 0.1
    done
    ! kill -0 "$viewer" 2>/dev/null || fail "the viewer did not quit on q"
    wait "$viewer" || fail "the viewer ended with status $?"
    viewer=
}
. "$(dirname "$0")/lib/edit.sh"
edit_images

open "$pics/plain.png"
bar_shown
xdo key x
sleep 0.5
# r and f twice: rectangles filled with the edit's red
xdo key r f f
sleep 0.3
xdo mousemove 100 100 mousedown 1 mousemove 150 130 mousemove 200 150 mouseup 1
sleep 0.3
# t, a click, and words typed into the line, which Return ends
xdo key t
xdo mousemove 300 300 click 1
sleep 0.3
xdo type --delay 50 'Hello'
xdo key Return
sleep 0.3
xdo key ctrl+c
paste_png "$pics/copy.png"
size=$(magick identify -format '%wx%h' "$pics/copy.png")
[ "$size" = 640x400 ] || fail "the copy is $size, not the image's 640x400"
# the rectangle's middle
holds "$pics/copy.png" 'p{150,125}.r > 0.9 && p{150,125}.g < 0.4' ||
    fail "the copy has no red rectangle at 150,125"
# the words: Control-z takes them away, and the copy then differs where they were typed
xdo key ctrl+z
sleep 0.3
xdo key ctrl+c
sleep 0.5
paste_png "$pics/undone.png"
words=$(differ "$pics/copy.png[120x40+300+262]" "$pics/undone.png[120x40+300+262]")
[ "$words" -gt 50 ] 2>/dev/null ||
    fail "the copy differs from one without the words in $words pixels where they were typed"
echo "x11-edit: the copy is a PNG of $size with the rectangle, and the words in $words pixels"
# w saves the edit beside the image, and shows it next
xdo key w
for _ in $(seq 50); do
    [ -s "$pics/plain-edit.png" ] && break
    sleep 0.1
done
[ -s "$pics/plain-edit.png" ] || fail "w saved no plain-edit.png"
saved=$(magick identify -format '%wx%h' "$pics/plain-edit.png")
[ "$saved" = 640x400 ] || fail "the saved edit is $saved, not 640x400"
off=$(differ "$pics/plain-edit.png" "$pics/undone.png")
[ "$off" = 0 ] || fail "the saved edit differs from the copy in $off pixels"
echo "x11-edit: w saved plain-edit.png at $saved"
xdo key q
quits

# space held to move a rectangle being drawn, then the focus elsewhere, where space comes up:
# back in the window, the drag resizes the rectangle again, as the copy shows, from where space
# moved it to, 150,120, to 250,200, and does not move it on to 200,170
rm -f "$pics/plain-edit.png"
# another window, xev's, away from the viewer's, to give the focus to
timeout 120 xev -name nsxiv-ng-other -geometry 50x50+1200+700 >/dev/null 2>&1 &
helper=$!
other=
for _ in $(seq 100); do
    other=$(xdo search --name '^nsxiv-ng-other$' 2>/dev/null | head -1) && [ -n "$other" ] && break
    sleep 0.1
done
[ -n "$other" ] || fail "xev opened no window"
open "$pics/plain.png"
bar_shown
xdo key x
sleep 0.5
xdo key r f f
sleep 0.3
xdo windowfocus --sync "$window"
xdo mousemove 100 100 mousedown 1 mousemove 150 130
sleep 0.2
xdo keydown space
sleep 0.2
xdo mousemove 200 150
sleep 0.2
xdo windowfocus --sync "$other"
xdo keyup space
xdo windowfocus --sync "$window"
sleep 0.2
xdo mousemove 250 200
sleep 0.2
xdo mouseup 1
sleep 0.3
xdo key ctrl+c
paste_png "$pics/space.png"
holds "$pics/space.png" 'p{170,140}.r > 0.9 && p{170,140}.g < 0.4' ||
    fail "after space came up with the focus elsewhere, the drag moved the rectangle on"
echo "x11-edit: space up with the focus elsewhere, the drag resized the rectangle again"
xdo key q
sleep 0.3
xdo key q
quits
stop "$helper"
helper=

# the noise's PNG, larger than a request the server takes, comes by INCR
open "$pics/noise.png"
xdo key x
sleep 1
xdo key ctrl+c
paste_png "$pics/big.png"
bytes=$(wc -c <"$pics/big.png")
largest=$(timeout 5 xdpyinfo | sed -n 's/^ *maximum request size: *\([0-9]*\) bytes.*/\1/p')
case $largest in
'' | *[!0-9]*) fail "xdpyinfo gave no largest request: '$largest'" ;;
esac
[ "$bytes" -gt "$largest" ] ||
    fail "the copy of $bytes bytes is no larger than the server's largest request, $largest"
size=$(magick identify -format '%wx%h' "$pics/big.png")
[ "$size" = 3200x2400 ] || fail "the large copy is $size, not 3200x2400"
off=$(differ "$pics/big.png" "$pics/noise.png")
[ "$off" = 0 ] || fail "the large copy differs from the image in $off pixels"
echo "x11-edit: the copy of $bytes bytes, over the server's $largest, came at $size"
# q leaves the edit, and q again quits
xdo key q
sleep 0.3
xdo key q
quits
