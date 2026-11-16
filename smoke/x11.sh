#!/bin/sh
# Drives the viewer in Xvfb with no window manager, in mode $2: "shm" puts frames through
# shared memory, "plain" runs a server without MIT-SHM, as over the network, where frames go
# in PutImage requests; the viewer must map the server's segment in the first and none in the
# second. Still, turned, alpha, animated and long-named images, the grid, the
# key-handler, then -o with two marks. The window must be first mapped at its final size, the
# image's, as the bar starts hidden, carry the app's class and the current
# file's name as its title, fit itself to the image, show the first image at 100% and the
# turned one fitted again, take AltGr and the key left of Z on Xvfb's pc105 layout as | and
# flip the image, the grid must take the starting 800x600 back, and the key-handler and -o
# must get the two marked files. The server must send the viewer no error. A viewer whose
# server then goes away must say so in one line and end with status 1. Mode "openbox" runs
# Openbox, which centers a new window in the free screen by the size it maps at: the first
# window must stay centered, leaving fullscreen must fit the window to the image shown then, or
# keep -g's size, and Openbox's close must end the viewer as q does.
# Mode "monitors" splits the screen into two RandR monitors, a large one at the origin and a
# small one beside it, and opens a large image twice under Openbox: with the pointer on the
# small one and Openbox placing there, the window is first mapped fitted to it; with the
# pointer on the large one and Openbox placing on the small one, it is fitted again to it.
#
# packages: imagemagick xvfb xdotool x11-utils x11-xserver-utils openbox wmctrl fonts-dejavu-core
# modes: shm plain openbox monitors
set -eu
bin=$1
mode=${2:-shm}
pics=$(mktemp -d)
xvfb=
viewer=
helpers=
# this run's own server, viewer, helpers and folder go on every way out; the server is waited
# for, as one still shutting down takes the socket of the next run's server on its display
trap '[ -z "$viewer" ] || kill "$viewer" 2>/dev/null || true; [ -z "$helpers" ] || kill $helpers 2>/dev/null || true; [ -z "$xvfb" ] || { kill "$xvfb" 2>/dev/null; wait "$xvfb"; } || true; rm -rf "$pics"' EXIT
trap 'exit 1' HUP INT TERM
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
fail() {
    echo "x11 $mode: $1" >&2
    [ ! -s "$pics/stderr" ] || sed 's/^/  viewer: /' "$pics/stderr" >&2
    exit 1
}

magick -size 640x400 gradient:red-blue "$pics/1-gradient.png"
magick -size 200x200 xc:none -fill green -draw 'circle 100,100 100,20' "$pics/2-alpha.png"
magick -delay 20 -size 120x80 xc:red xc:lime xc:blue -loop 0 "$pics/3-animated.gif"
long=4-a-name-long-enough-to-lose-its-start-in-the-status-bar.jpg
magick -size 300x300 plasma: "$pics/$long"
magick -size 640x400 plasma: "$pics/5-detail.png"

extensions=
[ "$mode" = plain ] && extensions="-extension MIT-SHM"
screen=1280x800
[ "$mode" = monitors ] && screen=2720x1080
# a display of its own, on a number the server picks
# $extensions unquoted, as the flag and its name are two words
Xvfb -displayfd 3 -screen 0 "${screen}x24" -nolisten tcp $extensions 3>"$pics/display" 2>"$pics/xvfb.log" &
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
    xdotool getdisplaygeometry >/dev/null 2>&1 && break
    sleep 0.1
done
xdotool getdisplaygeometry >/dev/null 2>&1 || fail "$DISPLAY takes no connection"
# the root's children as they are made, resized, mapped and framed, to see the size the window
# maps at; a property set on the root shows in the log once xev has the root's events
xev -root -event substructure -event property >"$pics/events" 2>&1 &
helpers=$!
for _ in $(seq 100); do
    xprop -root -f NSXIV_NG_SMOKE 8s -set NSXIV_NG_SMOKE ready
    grep -q NSXIV_NG_SMOKE "$pics/events" && break
    sleep 0.1
done
grep -q NSXIV_NG_SMOKE "$pics/events" || fail "xev took none of the root's events"
# Openbox with the placement $1 in its config, which an Openbox running reads again
openbox_places() {
    printf '<?xml version="1.0"?>\n<openbox_config xmlns="http://openbox.org/3.4/rc">\n%s\n%s\n' \
        "<placement><policy>Smart</policy><center>yes</center>$1</placement>" \
        '</openbox_config>' >"$pics/rc.xml"
    if xprop -root _NET_SUPPORTING_WM_CHECK | grep -q "window id"; then
        openbox --reconfigure
        sleep 0.5
        return
    fi
    openbox --config-file "$pics/rc.xml" >"$pics/openbox.log" 2>&1 &
    helpers="$helpers $!"
    for _ in $(seq 100); do
        xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "window id" && break
        sleep 0.1
    done
    xprop -root _NET_SUPPORTING_WM_CHECK | grep -q "window id" ||
        fail "Openbox did not start: $(cat "$pics/openbox.log")"
}
[ "$mode" = openbox ] && openbox_places ''
# the config and the grid's thumbnails stay in this run's folder
export XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the window of class nsxiv-ng, once it came
find_window() {
    for _ in $(seq 100); do
        window=$(xdotool search --classname '^nsxiv-ng$' 2>/dev/null | head -1) &&
            [ -n "$window" ] && return
        sleep 0.1
    done
    fail "no window of class nsxiv-ng came"
}
# the size the window had when it was first mapped, the one it was made or last configured
# with before, once xev wrote the map down; a window manager that frames a window takes it in
# with a ReparentNotify before it maps it. xev prints each event as a paragraph, the window it
# is about after "window" on its second line. Only the lines after the first $since count, as
# the server gives a later viewer's window the id of an earlier one's
first_mapped() {
    id=$(printf '0x%x' "$window")
    for _ in $(seq 50); do
        first=$(tail -n "+$((since + 1))" "$pics/events" |
            awk -v id="$id" 'BEGIN { RS = "" } index($0, "window " id ",") {
                if ($1 == "MapNotify" || $1 == "ReparentNotify") {
                    print (size == "" ? "none" : size); exit
                }
                if (match($0, /width [0-9]+, height [0-9]+/)) {
                    split(substr($0, RSTART, RLENGTH), a, /[ ,]+/); size = a[2] "x" a[4]
                }
            }')
        [ -n "$first" ] && return
        sleep 0.1
    done
    fail "xev saw no map of the window"
}
# the window's size now
window_size() {
    xdotool getwindowgeometry --shell "$window" | sed -n 's/^WIDTH=//p; s/^HEIGHT=//p' |
        paste -sd x
}

if [ "$mode" = monitors ]; then
    # the output goes to the large monitor, which takes the place of the whole screen's
    xrandr --setmonitor large 1920/508x1080/286+0+0 screen >/dev/null
    xrandr --setmonitor small 800/212x600/159+1920+0 none
    magick -size 1200x900 gradient:red-blue "$pics/large.png"
    # fitted to the large monitor the window is 1200 wide; to the small one, within 800x600
    small() { [ "${1%x*}" -le 800 ] && [ "${1#*x}" -le 600 ]; }
    opens() {
        xdotool mousemove "$1" "$2"
        since=$(wc -l <"$pics/events")
        "$bin" "$pics/large.png" 2>"$pics/stderr" &
        viewer=$!
        find_window
        first_mapped
        sleep 1
        final=$(window_size)
        x=$(xwininfo -id "$window" | sed -n 's/^ *Absolute upper-left X: *//p')
        kill "$viewer"
        wait "$viewer" || true
        viewer=
    }
    openbox_places '<monitor>Mouse</monitor>'
    opens 2320 300
    [ "$first" = "$final" ] && small "$final" && [ "$x" -ge 1920 ] ||
        fail "with the pointer on the small monitor the window was first $first, then $final at x $x"
    echo "x11 $mode: with the pointer on the small monitor, first mapped at $first, at x $x"
    openbox_places '<monitor>Primary</monitor><primaryMonitor>2</primaryMonitor>'
    opens 960 540
    [ "${first%x*}" -eq 1200 ] && small "$final" && [ "$x" -ge 1920 ] ||
        fail "placed on the small monitor the window was first $first, then $final at x $x"
    echo "x11 $mode: placed on the small monitor, first mapped at $first, fitted again to $final"
    exit 0
fi

handler="$XDG_CONFIG_HOME/nsxiv-ng/exec/key-handler"
mkdir -p "$(dirname "$handler")"
printf '#!/bin/sh\n{ echo "key $1"; cat; } >"$0.ran"\n' >"$handler"
chmod +x "$handler"

shot() { sleep 1; magick import -window root "$pics/$1.png"; }
# whether the screenshot $1 passes the fx test $2
holds() { [ "$(magick "$pics/$1.png" -format "%[fx:$2]" info:)" = 1 ]; }

since=0
timeout 60 "$bin" -o "$pics" >"$pics/marked" 2>"$pics/stderr" & # a viewer that misses q fails the run
viewer=$!
find_window
# the window fits the image: its final size is the image's 640x400, the bar hidden as it
# starts, once it shows; it was made at the default 800x600
for _ in $(seq 50); do
    final=$(window_size)
    width=${final%x*} height=${final#*x}
    [ "$width" -eq 640 ] && [ "$height" -eq 400 ] && xwininfo -id "$window" | grep -q IsViewable &&
        break
    sleep 0.1
done
[ "$width" -eq 640 ] && [ "$height" -eq 400 ] ||
    fail "the window is $final, not the image's 640x400"
first_mapped
[ "$first" = "$final" ] || fail "the window was first mapped at $first, not its final $final"
if [ "$mode" = openbox ]; then
    # Openbox placed the frame, the window with its borders and title bar, in the screen's
    # center by the size the window mapped at, and the window keeps its corner as it resizes;
    # Openbox may take a moment to settle the frame
    center_x=none center_y=none off_x=9 off_y=9
    for _ in $(seq 30); do
        set -- $(xwininfo -id "$window" | sed -n 's/^ *Absolute upper-left [XY]: *//p')
        set -- "$@" $(xprop -id "$window" _NET_FRAME_EXTENTS | sed -n 's/.*= //p' | tr -d ,)
        [ $# -eq 6 ] || { sleep 0.1; continue; }
        center_x=$(($1 - $3 + (width + $3 + $4) / 2))
        center_y=$(($2 - $5 + (height + $5 + $6) / 2))
        off_x=$((center_x - 640)) off_y=$((center_y - 400))
        [ "${off_x#-}" -le 1 ] && [ "${off_y#-}" -le 1 ] && break
        sleep 0.1
    done
    [ "${off_x#-}" -le 1 ] && [ "${off_y#-}" -le 1 ] ||
        fail "the first window's frame is centered at $center_x,$center_y, not 640,400"
    echo "x11 $mode: the first window, $final, centered at $center_x,$center_y"
    # leaving fullscreen fits the window to the image shown then; with -g it keeps -g's size.
    # Openbox gives the window the keys, and puts it back at its size before fullscreen
    full() {
        xdotool key f
        for _ in $(seq 50); do
            [ "$(window_size)" = 1280x800 ] && return
            sleep 0.1
        done
        fail "f gave the window $(window_size), not the screen's 1280x800"
    }
    # fullscreen, the alpha, then back; the window's size a second after it left fullscreen
    alpha_out_of_fullscreen() {
        full
        xdotool key j
        for _ in $(seq 50); do
            [ "$(xdotool getwindowname "$window")" = 2-alpha.png ] && break
            sleep 0.1
        done
        [ "$(xdotool getwindowname "$window")" = 2-alpha.png ] ||
            fail "j showed '$(xdotool getwindowname "$window")', not the alpha"
        xdotool key f
        # the size it settles at out of fullscreen: two looks a third of a second apart agree
        left=1280x800
        for _ in $(seq 15); do
            sleep 0.3
            now=$(window_size)
            [ "$now" != 1280x800 ] && [ "$now" = "$left" ] && break
            left=$now
        done
    }
    alpha_out_of_fullscreen
    [ "$left" = 200x200 ] || fail "out of fullscreen the window is $left, not the alpha's 200x200"
    xdotool key q
    wait "$viewer" || fail "the viewer did not end with status 0 on q"
    timeout --kill-after=2 60 "$bin" -g 500x400 "$pics/1-gradient.png" "$pics/2-alpha.png" \
        2>"$pics/stderr" &
    viewer=$!
    find_window
    for _ in $(seq 50); do
        [ "$(window_size)" = 500x400 ] && xwininfo -id "$window" | grep -q IsViewable && break
        sleep 0.1
    done
    [ "$(window_size)" = 500x400 ] && xwininfo -id "$window" | grep -q IsViewable ||
        fail "the -g 500x400 window showed at $(window_size)"
    alpha_out_of_fullscreen
    [ "$left" = 500x400 ] || fail "out of fullscreen the -g 500x400 window is $left"
    xdotool key q
    wait "$viewer" || fail "the viewer did not end with status 0 on q"
    viewer=
    echo "x11 $mode: out of fullscreen the window took the alpha's size, and with -g kept 500x400"
    # Openbox's close, which sends the window WM_DELETE_WINDOW as it names that protocol, ends
    # the viewer as q does: status 0, and -o prints the mark, the gradient's, which m made
    timeout --kill-after=2 30 "$bin" -o "$pics/1-gradient.png" "$pics/2-alpha.png" \
        >"$pics/marked" 2>"$pics/stderr" &
    viewer=$!
    find_window
    for _ in $(seq 50); do
        xwininfo -id "$window" | grep -q IsViewable && break
        sleep 0.1
    done
    xdotool key m
    for _ in $(seq 50); do
        [ "$(xdotool getwindowname "$window")" = 2-alpha.png ] && break
        sleep 0.1
    done
    [ "$(xdotool getwindowname "$window")" = 2-alpha.png ] || fail "m did not mark and go on"
    wmctrl -i -c "$(printf 0x%x "$window")"
    ended "$viewer" 5 || fail "Openbox's close did not end the viewer"
    viewer=
    [ "$status" -eq 0 ] || fail "Openbox's close ended the viewer with status $status"
    [ "$(cat "$pics/marked")" = "$pics/1-gradient.png" ] ||
        fail "after Openbox's close -o printed '$(cat "$pics/marked")', not the gradient"
    echo "x11 $mode: Openbox's close ended the viewer with status 0, and -o printed $(cat "$pics/marked")"
    exit 0
fi
sleep 2
xdotool mousemove 40 40 # without a window manager, keys go to the window under the pointer
[ "$(xdotool getwindowname "$window")" = 1-gradient.png ] ||
    fail "the title is '$(xdotool getwindowname "$window")', not the first file's name"
shot 1-image
# with no window manager the window sits at 0,0, and the 640x400 gradient at 100% fills its
# top: red at the top, blue at the bottom
holds 1-image 'p{320,5}.r > 0.8 && p{320,5}.b < 0.2 && p{320,394}.b > 0.8 && p{320,394}.r < 0.2' ||
    fail "the first image is not drawn"
# the viewer, timeout's child, maps the segment Xvfb makes as a memfd, shared and writable
child=$(cat "/proc/$viewer/task/$viewer/children")
segments=$(grep -c ' rw-s .*/memfd:xorg' "/proc/${child%% *}/maps" || true)
if [ "$mode" = shm ]; then
    [ "$segments" -ge 1 ] || fail "the viewer maps no shared memory of the server's"
else
    [ "$segments" -eq 0 ] || fail "the viewer maps shared memory of a server without MIT-SHM"
fi
xdotool key greater
shot 2-turned
# turned clockwise and fitted again, 400 wide: red on the right, blue on the left
holds 2-turned 'p{395,320}.r > 0.8 && p{395,320}.b < 0.2 && p{5,320}.b > 0.8 && p{5,320}.r < 0.2' ||
    fail "the turned image is not drawn in a window fitted to it"
# level 3 of keycode 94, <LSGT>, whose core mapping is less greater less greater bar brokenbar
# bar; xdotool would take less from another key, so the keycode goes as it is
xdotool key ISO_Level3_Shift+94
shot 2-flipped
holds 2-flipped 'p{5,320}.r > 0.8 && p{5,320}.b < 0.2 && p{395,320}.b > 0.8 && p{395,320}.r < 0.2' ||
    fail "AltGr and <LSGT> did not flip the image as | does"
xdotool key j
shot 3-alpha
xdotool key A
shot 4-checkerboard
xdotool key j
shot 5-animated
xdotool key j
shot 6-long-name
[ "$(xdotool getwindowname "$window")" = "$long" ] || fail "the title did not follow the file"
xdotool key g m m Return
# the grid takes the starting size back, from the image's the marks went on to
for _ in $(seq 50); do
    [ "$(window_size)" = 800x600 ] && break
    sleep 0.1
done
[ "$(window_size)" = 800x600 ] || fail "the grid's window is $(window_size), not the starting 800x600"
sleep 1
shot 7-grid
xdotool key ctrl+x c
sleep 1
xdotool key q
wait "$viewer" || fail "the viewer did not end with status 0 on q"
viewer=
if grep -q 'X error' "$pics/stderr"; then fail "the server sent the viewer errors"; fi
[ "$(wc -l <"$pics/marked")" -eq 2 ] || fail "-o printed $(wc -l <"$pics/marked") files, not the two marked"
[ "$(cat "$handler.ran")" = "key c
$(cat "$pics/marked")" ] || fail "the key-handler got '$(cat "$handler.ran")'"
echo "x11 $mode: marked and passed to the key-handler: $(tr '\n' ' ' <"$pics/marked")"

# pans of an image at 200%, each frame moved from the one before in the buffer that holds it,
# end on the very frame drawn whole, which antialiasing off and on again draws
timeout 60 "$bin" "$pics/5-detail.png" 2>"$pics/stderr" &
viewer=$!
find_window
sleep 2
xdotool mousemove 40 40
xdotool key plus plus
sleep 1
for key in Down Right Down Left Up Right Right Down; do
    xdotool key "$key"
    sleep 0.3
done
shot 8-moved
xdotool key a
sleep 0.5
xdotool key a
shot 8-whole
differ=$(magick compare -metric AE "$pics/8-moved.png" "$pics/8-whole.png" null: 2>&1 | cut -d' ' -f1) ||
    true
[ "$differ" = 0 ] || fail "after the pans $differ pixels differ from the frame drawn whole"
xdotool key q
wait "$viewer" || fail "the viewer of the pans did not end with status 0 on q"
viewer=
echo "x11 $mode: the frame the pans moved is the frame drawn whole"

# a viewer whose server goes away says so, and ends with status 1; one that stays ends with
# its timeout, after the trap
timeout --kill-after=2 30 "$bin" "$pics/1-gradient.png" 2>"$pics/stderr" &
viewer=$!
for _ in $(seq 100); do
    window=$(xdotool search --onlyvisible --classname '^nsxiv-ng$' 2>/dev/null | head -1) &&
        [ -n "$window" ] && break
    sleep 0.1
done
[ -n "$window" ] || fail "no window came for the server to take with it"
kill "$xvfb"
wait "$xvfb" || true
xvfb=
ended "$viewer" 5 || fail "the viewer outlived its server"
viewer=
said=$(cat "$pics/stderr")
# one line, none of a library's own beside it
[ "$status" -eq 1 ] && [ "$(wc -l <"$pics/stderr")" -eq 1 ] &&
    echo "$said" | grep -qx 'nsxiv-ng: lost the connection to the X server: .*' ||
    fail "without its server the viewer ended with status $status, saying '$said'"
echo "x11 $mode: without its server the viewer said '$said' and ended with status 1"
