#!/bin/sh
# Runs the viewer as a Wayland client of a headless compositor with no GPU, in mode $2.
# In "sway", at scale 1.5 through the fractional scale and a viewport, with the window
# floating: a gradient, committed in a buffer 1.5 times the window's width, as the protocol
# log shows, whose first frame already has the window's final size and which sway centers in
# its workspace, the same turned, which a narrower window fits and sway's IPC keeps inside the
# workspace, sway's title bar included, a small image with alpha, whose size the window takes,
# the checkerboard, an animation that plays, fullscreen and back where it was, two marks and
# the grid, also inside the workspace, the key-handler after C-x, then q, after which -o prints
# the marks; the grid must take the starting size back, as far as the workspace allows.
# Then a large image in a window that fits the first output's workspace, and grows to the
# second's, larger at the same scale, once sway moves it there. Then a mark, which the window
# commits as damage of the bar's rows alone, with the pixels a whole frame has. Then a viewer
# whose compositor goes away must say so in one line and end with status 1.
# Every key comes from a new virtual keyboard, so the seat's keyboard comes and goes. In
# "weston", at scale 2, which offers no fractional scale, so the window draws at the outputs'
# integer scale: stripes one pixel wide shown at 50% must keep their black and white pixels,
# which a frame drawn at scale 1 would average to gray before the compositor enlarges it; as
# Weston decorates no window, the window draws its own title bar above the image, which with
# the status bar below makes two bars of the bar's color; and SIGTERM must end the viewer as q
# does. Then a slideshow onto a photo whose decode the script holds, by stopping the helper that
# decodes it: its name shows in that title bar at once, and stays once the photo shows. In
# "weston-gl", that slideshow alone under Weston's GL renderer, which gives a buffer back while
# it shows it, so the bar is drawn in place.
#
# Left out as no tool here can drive it: the pointer, on the image and on the title bar and
# the edges the window draws itself, since wlrctl plugs in a new virtual pointer for each
# command and unplugs it at once, so a click is over before the viewer has bound the pointer.
#
# packages: sway weston grim wtype jq imagemagick libgl1-mesa-dri libegl-mesa0 python3
# modes: weston weston-gl sway
set -eu
bin=$1
mode=${2:-sway}
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
# out: TERM, then KILL after a grace longer than the viewer's timeout gives its child
between=
held=
stop() {
    # helpers held stopped first, which nothing else ends
    [ -z "$held" ] || kill -KILL $held 2>/dev/null || true
    for process in $pid $between $compositor; do
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
magick -size 640x400 gradient:red-blue "$images/1-gradient.png"
magick -size 200x200 xc:none -fill green -draw 'circle 100,100 100,20' "$images/2-alpha.png"
magick -delay 20 -size 120x80 xc:red xc:lime xc:blue -loop 0 "$images/3-animated.gif"
magick -size 300x300 plasma: "$images/4-a-name-long-enough-to-lose-its-start-in-the-status-bar.jpg"

# the config, the key-handler and the thumbnails all in this run's folder
export XDG_RUNTIME_DIR="$pics/runtime" XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
mkdir -m 700 "$XDG_RUNTIME_DIR"
handler=$XDG_CONFIG_HOME/nsxiv-ng/exec/key-handler
mkdir -p "$(dirname "$handler")"
printf '#!/bin/sh\n{ echo "key $1"; cat; } >"$0.ran"\n' >"$handler"
chmod +x "$handler"
# a Wayland window, not an X one, and none of a session running the test
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK

# waits for the compositor's socket
socket() {
    for _ in $(seq 100); do
        for candidate in "$XDG_RUNTIME_DIR"/wayland-[0-9]; do
            if [ -S "$candidate" ]; then
                WAYLAND_DISPLAY=$(basename "$candidate")
                export WAYLAND_DISPLAY
                return
            fi
        done
        kill -0 "$compositor" 2>/dev/null || fail "$mode did not start"
        sleep 0.1
    done
    fail "$mode opened no socket"
}

if [ "$mode" = weston ] || [ "$mode" = weston-gl ]; then
    renderer=pixman
    [ "$mode" = weston-gl ] && renderer=gl
    # --debug lets weston-screenshooter capture the output
    weston --backend=headless --renderer=$renderer --width=1280 --height=800 --scale=2 \
        --idle-time=0 --debug --socket=wayland-1 >"$pics/compositor.log" 2>&1 &
    compositor=$!
    socket
    # the output in $pics/$1.png
    screenshot() {
        rm -f "$pics"/wayland-screenshot*.png
        (cd "$pics" && XDG_PICTURES_DIR=$pics timeout 10 weston-screenshooter) \
            >>"$pics/compositor.log" 2>&1 || fail "weston-screenshooter failed"
        mv "$(ls "$pics"/wayland-screenshot*.png | head -1)" "$pics/$1.png"
    }
fi
if [ "$mode" = weston ]; then
    magick -size 1x1 xc:black xc:white +append "$pics/stripe.png"
    magick -size 400x200 tile:"$pics/stripe.png" "$pics/stripes.png"
    "$bin" -z 50 "$pics/stripes.png" &
    pid=$!
    # Weston places windows at random, so pixels are counted by their color: the stripes'
    # 20000 black ones, and the bars', about 40000 each for a window 800 pixels wide
    count() {
        magick "$pics/stripes-shot.png" -fill white +opaque "$1" -fill black -opaque "$1" \
            -precision 12 -format '%[fx:int((1-mean)*w*h)]' info:
    }
    black=0
    bar=0
    for _ in $(seq 20); do
        sleep 0.5
        running "$pid" || fail "the viewer ended, as on a protocol error"
        screenshot stripes-shot
        black=$(count '#000000')
        bar=$(count '#eeeeee')
        [ "$black" -gt 15000 ] && [ "$bar" -gt 65000 ] && break
    done
    [ "$black" -gt 15000 ] || fail "the stripes have $black black pixels, as drawn at scale 1"
    [ "$bar" -gt 65000 ] || fail "the bars have $bar pixels, too few for a title bar above the status bar"
    kill -TERM "$pid"
    ended "$pid" 10 || fail "the viewer did not end on SIGTERM"
    pid=
    [ "$status" -eq 0 ] || fail "SIGTERM ended the viewer with status $status"
    echo "weston: $black black pixels in the stripes at scale 2, $bar in the title bar and the status bar"
fi
if [ "$mode" = weston ] || [ "$mode" = weston-gl ]; then
    # A slideshow onto a photo whose decode is held: an SVG, which a decoding helper decodes.
    # The viewer starts on another SVG, so a helper is up and idle once that shows, and the
    # script stops it; the slideshow goes on to a gradient and wraps to the photo, which no
    # file shown before has next to it, so nothing decodes it ahead, and its decode goes to
    # the stopped helper. Its name must show in the window's own title bar at once, with the
    # status bar alone, while the image area still shows the gradient; the photo must come
    # once the helper goes on, and the name stay.
    magick -size 300x200 gradient:red-blue "$images/1-gradient.png"
    magick -size 300x200 gradient:red-blue "$images/3-gradient.png"
    printf '%s\n' '<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200">' \
        '<rect width="300" height="200" fill="#0f0"/></svg>' >"$images/0-photo.svg"
    printf '%s\n' '<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200">' \
        '<rect width="300" height="200" fill="#f00"/></svg>' >"$images/2-first.svg"
    # six seconds an image, so the gradient's screenshot is taken well before the photo's name
    WAYLAND_DEBUG=client timeout --kill-after=2 60 "$bin" -g 300x200 -S 6 -n 3 \
        "$images/0-photo.svg" "$images/1-gradient.png" "$images/2-first.svg" \
        "$images/3-gradient.png" 2>"$pics/wire" &
    pid=$!
    # the helpers, once the first SVG's frame came, all stopped
    for _ in $(seq 100); do
        sed -n '/set_title(Some("2-first.svg"))/,$p' "$pics/wire" 2>/dev/null |
            grep -q 'damage_buffer(0, 0, 2147483647' && break
        sleep 0.1
    done
    viewer=$(pgrep -P "$pid" | head -1)
    held=$(pgrep -P "${viewer:-0}" -f -- '--decode-helper ' | tr '\n' ' ')
    [ -n "$held" ] || fail "no decoding helper came for the first SVG"
    kill -STOP $held
    # the window by the bars' color, its title bar the top $1 rows of their bounding box, and
    # the middle of the image area between the bars
    window_box() {
        box=$(magick "$pics/$1.png" -fill black +opaque '#eeeeee' -format '%@' info:)
        set -- "${box%%x*}" "${box#*x}"
        width=$1 height=${2%%+*} left=${2#*+} top_edge=${2##*+}
        left=${left%%+*}
    }
    title() {
        magick "$pics/$1.png" -crop "${width}x$rows+$left+$top_edge" +repage "$pics/$1-title.png"
    }
    # the middle of the image area is green, the photo's
    green() {
        magick "$pics/$1.png" -crop "40x40+$((left + width / 2 - 20))+$((top_edge + height / 2 - 20))" \
            -format '%[fx:mean.g > 0.4 && mean.r < 0.3]' info:
    }
    differ() { magick compare -metric AE "$pics/$1-title.png" "$pics/$2-title.png" null: 2>&1 || true; }
    # the gradient before the photo, once its frame came
    for _ in $(seq 100); do
        sed -n '/set_title(Some("3-gradient.png"))/,$p' "$pics/wire" |
            grep -q 'damage_buffer(0, 0, 2147483647' && break
        sleep 0.1
    done
    for _ in $(seq 20); do
        screenshot first
        box=$(magick "$pics/first.png" -fill black +opaque '#eeeeee' -format '%@' info:)
        case $box in 0x0*) sleep 0.2 ;; *) break ;; esac
    done
    window_box first
    # a screenshot that took as long as the slideshow's step would hold the photo's name already
    ! grep -q 'set_title(Some("0-photo.svg"))' "$pics/wire" ||
        fail "the screenshot of the gradient came after the photo's name"
    [ "$(green first)" = 0 ] || fail "the gradient's frame already shows the photo"
    # the rows of the status bar damaged alone once the window took the photo's name, as many
    # as the title bar's
    rows=
    for _ in $(seq 100); do
        rows=$(sed -n '/set_title(Some("0-photo.svg"))/,$p' "$pics/wire" |
            grep -o 'damage_buffer(0, [1-9][0-9]*, [0-9]*, [0-9]*)' |
            sed -n 's/.*, \([0-9]*\))/\1/p' | head -1)
        [ -n "$rows" ] && break
        sleep 0.1
    done
    [ -n "$rows" ] || fail "the slideshow drew no status bar alone for the photo's name"
    title first
    named=
    for _ in $(seq 40); do
        screenshot named
        title named
        [ "$(differ first named)" != 0 ] && { named=1; break; }
        sleep 0.2
    done
    [ -n "$named" ] || fail "the title bar in ${width}x$rows+$left+$top_edge kept the first name"
    [ "$(green named)" = 0 ] || fail "the photo came while its decode was held"
    kill -CONT $held
    held=
    shown=
    for _ in $(seq 100); do
        screenshot shown
        [ "$(green shown)" = 1 ] && { shown=1; break; }
        sleep 0.2
    done
    [ -n "$shown" ] || fail "the photo never showed once its helper went on"
    title shown
    [ "$(differ named shown)" = 0 ] ||
        fail "the title bar differs in $(differ named shown) pixels once the photo came"
    kill -TERM "$pid"
    ended "$pid" 10 || fail "the viewer did not end on SIGTERM"
    pid=
    echo "$mode: the next file's name showed in the title bar before its photo"
    [ "$mode" = weston-gl ] || exit 0
    # the GL renderer copies a buffer into a texture and gives it back while it still shows,
    # so the bar is drawn in place: in the buffer of the last commit, recommitted with part of
    # it damaged
    counts=$(awk 'function id() { match($0, /wl_buffer@[0-9]+/); return substr($0, RSTART + 10, RLENGTH - 10) }
        /-> wl_surface@[0-9]*\.attach\(wl_buffer@/ { on = id(); whole = 0 }
        /-> wl_surface@[0-9]*\.damage_buffer\(0, 0, 2147483647/ { whole = 1 }
        /-> wl_surface@[0-9]*\.commit\(\)/ {
            if (on != "") { if (!whole) { bars++; if (on == shown) kept++ } shown = on; on = "" }
        }
        /<- wl_buffer@[0-9]*\.release/ { if (id() == shown) early++ }
        END { printf "%d %d %d", bars, kept, early }' "$pics/wire")
    set -- $counts
    [ "$2" -ge 1 ] || fail "none of $1 commits of the bar alone drew in place, with $3 buffers given back while shown"
    echo "$mode: $2 of $1 commits of the bar alone drew in place, $3 buffers given back while shown"
    exit 0
fi

cat >"$pics/sway.conf" <<'EOF'
output HEADLESS-1 resolution 1280x800 position 0 0 scale 1.5
output HEADLESS-2 resolution 1920x1200 position 1000 0 scale 1.5
workspace 1 output HEADLESS-1
workspace 2 output HEADLESS-2
focus output HEADLESS-1
for_window [app_id="nsxiv-ng"] floating enable
EOF
WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=2 WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    sway -c "$pics/sway.conf" >"$pics/compositor.log" 2>&1 &
compositor=$!
socket
for _ in $(seq 100); do
    for candidate in "$XDG_RUNTIME_DIR"/sway-ipc.*.sock; do
        [ -S "$candidate" ] && SWAYSOCK=$candidate
    done
    [ -n "${SWAYSOCK:-}" ] && break
    sleep 0.1
done
[ -n "${SWAYSOCK:-}" ] || fail "sway opened no IPC socket"
export SWAYSOCK

# the viewer's window in sway's tree
window() { swaymsg -t get_tree | jq -c '.. | objects | select(.app_id == "nsxiv-ng")'; }
field() { window | jq -r "$1"; }
# waits for the window's field $1 to be $2
expect() {
    for _ in $(seq 50); do
        [ "$(field "$1")" = "$2" ] && return
        sleep 0.1
    done
    fail "the window's $1 is $(field "$1"), not $2"
}
# whether the whole window is inside the workspace, sway's title bar included: like i3, sway
# reports that bar as deco_rect, above rect
inside() {
    swaymsg -t get_tree | jq -e '[.. | objects | select(.type == "workspace" and .name == "1") | .rect] as [$w]
        | .. | objects | select(.app_id == "nsxiv-ng") | .deco_rect.height as $t | .rect
        | .x >= $w.x and .y - $t >= $w.y and .x + .width <= $w.x + $w.width and .y + .height <= $w.y + $w.height' \
        >/dev/null
}
# waits for sway's IPC to place the window inside the workspace
expect_inside() {
    for _ in $(seq 50); do
        inside && return
        sleep 0.1
    done
    fail "the window left the workspace: $(swaymsg -t get_tree | jq -c '.. | objects |
        select(.app_id == "nsxiv-ng" or .type == "workspace") | [.rect, .deco_rect]' | tr '\n' ' ')"
}
# each wtype plugs in a new virtual keyboard; the pause lets the viewer bind it first
key() { wtype -s 300 "$@"; }

# a viewer that misses q fails the run; the protocol log tells the buffers' sizes
WAYLAND_DEBUG=client timeout --kill-after=2 60 "$bin" -o "$images" >"$pics/marked" \
    2>"$pics/wire" &
pid=$!
for _ in $(seq 100); do
    [ -n "$(window)" ] && break
    running "$pid" || fail "the viewer ended before its window showed"
    sleep 0.1
done
[ -n "$(window)" ] || fail "no window with the app id nsxiv-ng"
expect .name 1-gradient.png
# the floating window takes the image's width, the bar below it
expect .window_rect.width 640
# whether a surface was committed with a buffer 960 pixels wide, by the width argument of the
# request that made it, while the viewport's destination was 640 wide
committed() {
    awk '{
        at = index($0, "-> "); if (!at) next
        split(substr($0, at + 3), a, /[@.(), ]+/)
        if (a[3] == "create_buffer") width[a[5]] = a[7]
        else if (a[3] == "set_destination") destination = a[4]
        else if (a[3] == "attach") on[a[2]] = a[5]
        else if (a[3] == "commit" && width[on[a[2]]] == 960 && destination == 640) { found = 1; exit }
    } END { exit !found }' "$pics/wire"
}
for _ in $(seq 50); do
    committed && break
    sleep 0.1
done
committed || fail "no frame of the 640-wide window came in a buffer 960 pixels wide: $(grep -o \
    -e 'create_buffer([^)]*)' -e 'set_destination([^)]*)' "$pics/wire" | tail -4 | tr '\n' ' ')"
# the first frame the compositor saw has the window's final size, by the viewport's
# destination set for it, in logical units as sway's tree counts them: the gradient's with the
# status bar's rows, not the default size, nor the image's without the bar, which a first frame
# drawn before the image or the fonts has; it may come at scale 1, before sway tells the scale
first=$(grep -o -m 1 'wp_viewport@[0-9]*\.set_destination([0-9]*, [0-9]*)' "$pics/wire" |
    sed 's/.*(\([0-9]*\), \([0-9]*\))/\1x\2/')
# the final size once the bar's rows are in the window, below the gradient's 400
for _ in $(seq 50); do
    [ "$(field .window_rect.height)" -gt 400 ] && break
    sleep 0.1
done
final=$(field '"\(.window_rect.width)x\(.window_rect.height)"')
[ "${final#*x}" -gt 400 ] || fail "the window never took the bar's rows: $final"
[ "$first" = "$final" ] || fail "the first frame was $first, not the window's final $final"
# sway centers a new floating window in its workspace, the window with its borders, its title
# bar above, and the viewer's placement leaves it there, as it counts only from the first frame
sleep 0.5
centered() {
    swaymsg -t get_tree | jq -e '[.. | objects | select(.type == "workspace" and .name == "1")
        | .rect] as [$w] | .. | objects | select(.app_id == "nsxiv-ng") | .rect
        | (.x + .width / 2 - $w.x - $w.width / 2 | fabs) <= 1
            and (.y + .height / 2 - $w.y - $w.height / 2 | fabs) <= 1' >/dev/null
}
centered || fail "the first window is off the workspace's center: $(swaymsg -t get_tree | jq -c \
    '.. | objects | select(.app_id == "nsxiv-ng" or .type == "workspace") | [.rect, .deco_rect]' |
    tr '\n' ' ')"
key '>'
for _ in $(seq 50); do
    [ "$(field .window_rect.width)" -lt 640 ] && break
    sleep 0.1
done
[ "$(field .window_rect.width)" -lt 640 ] || fail "the turned image kept the window 640 wide"
expect_inside
key j
expect .name 2-alpha.png
expect .window_rect.width 200
key A
key j
expect .name 3-animated.gif
# the frames show red, lime and blue for 0.2 s each, so over a second the middle of the image,
# above the status bar, takes more than one color; sway counts logical pixels, grim physical
# ones, of the first output alone, which is quicker than both
sleep 1
at=$(field '"\((.rect.x + .rect.width / 2) * 1.5 | floor),\((.rect.y + .rect.height / 3) * 1.5 | floor)"')
colors=$(for n in 1 2 3 4; do
    sleep 0.25
    grim -o HEADLESS-1 "$pics/frame-$n.png"
    magick "$pics/frame-$n.png" -format "%[hex:p{$at}]\n" info:
done | sort -u | wc -l)
[ "$colors" -ge 2 ] || fail "the animation does not play at $at"
frame='"\(.rect.x) \(.rect.y) \(.rect.width) \(.rect.height)"'
before=$(field "$frame")
key f
expect .fullscreen_mode 1
key f
expect .fullscreen_mode 0
# sway puts the window back where it was, and the viewer's placement, which comes within a
# second, must leave it there
for _ in $(seq 50); do
    [ "$(field "$frame")" = "$before" ] && break
    sleep 0.1
done
sleep 1
[ "$(field "$frame")" = "$before" ] || fail "out of fullscreen the window went from $before to $(field "$frame")"
key g m m -k Return
# the grid takes the starting size back from the image's the marks went on to: the config's
# 800x600, as tall as the workspace lets it, 853x533 at scale 1.5 with sway's title bar
expect .window_rect.width 800
expect_inside
[ "$(field .window_rect.height)" -gt 480 ] ||
    fail "the grid's window is $(field .window_rect.height) high, not the starting 600 in the room"
key -M ctrl x -m ctrl c
for _ in $(seq 50); do
    [ -s "$handler.ran" ] && break
    sleep 0.1
done
key q
ended "$pid" 10 || fail "the viewer did not end on q"
pid=
[ "$status" -eq 0 ] || fail "the viewer ended with status $status"
[ "$(wc -l <"$pics/marked")" -eq 2 ] || fail "-o printed $(cat "$pics/marked"), not two marks"
[ "$(cat "$handler.ran" 2>/dev/null)" = "key c
$(cat "$pics/marked")" ] || fail "the key-handler got $(cat "$handler.ran" 2>/dev/null)"
echo "sway: marked and passed to the key-handler: $(tr '\n' ' ' <"$pics/marked")"

# sway's room follows the window to another output at the same scale: fitted to the first
# output's workspace, 853 wide, under 800 once it took the image's size and no longer the
# default's, the large image grows past that in the second's, 1280 wide
magick -size 1600x1000 gradient:green-blue "$pics/large.png"
timeout --kill-after=2 30 "$bin" "$pics/large.png" &
pid=$!
expect .name large.png
for _ in $(seq 50); do
    inside && [ "$(field .window_rect.width)" -lt 800 ] && break
    sleep 0.1
done
small=$(field .window_rect.width)
[ "$small" -lt 800 ] || fail "the large image's window is $small wide, not fitted to the first workspace"
swaymsg '[app_id="nsxiv-ng"] move container to output HEADLESS-2' >/dev/null
for _ in $(seq 50); do
    [ "$(field .window_rect.width)" -gt 853 ] && break
    sleep 0.1
done
large=$(field .window_rect.width)
[ "$large" -gt 853 ] || fail "on the larger output the window stayed $large wide, in the first's room"
# the window keeps the focus it had, which the move may take
swaymsg '[app_id="nsxiv-ng"] focus' >/dev/null
key q
ended "$pid" 10 || fail "the viewer did not end on q"
pid=
echo "sway: the large image's window grew from $small to $large wide on the larger output"

# a mark on the only file changes the bar alone: the viewer commits the bar's rows alone as
# damage, at the bottom of a buffer of the window's size, and the outputs then show what a
# whole frame of the same state shows, as after fullscreen and back; the window opens on the
# second output, which has the focus
WAYLAND_DEBUG=client timeout --kill-after=2 30 "$bin" "$images/1-gradient.png" 2>"$pics/wire" &
pid=$!
expect .name 1-gradient.png
for _ in $(seq 50); do
    [ "$(field .window_rect.height)" -gt 400 ] && break
    sleep 0.1
done
# the rows of the commit after line $1 of the protocol log whose one damage is the bottom rows
# of its buffer, the whole width and less than its height, as "rows of height"
bar_alone() {
    awk -v since="$1" '{
        at = index($0, "-> "); if (!at) next
        split(substr($0, at + 3), a, /[@.(), ]+/)
        if (a[3] == "create_buffer") { w[a[5]] = a[7]; h[a[5]] = a[8] }
        else if (a[3] == "attach") { on = a[5]; n = 0 }
        else if (a[3] == "damage_buffer") { n++; x = a[4]; y = a[5]; dw = a[6]; rows = a[7] }
        else if (a[3] == "commit") {
            if (NR > since && n == 1 && x == 0 && dw == w[on] && y + rows == h[on] && rows < h[on]) {
                print rows " of " h[on]; exit
            }
            n = 0
        }
    }' "$pics/wire"
}
differ() { magick compare -metric AE "$pics/$1.png" "$pics/$2.png" null: 2>&1 || true; }
# the outputs in $pics/$1.png once they show what $3 says of them against $pics/$2.png, a
# change or none, within five seconds
shows() {
    for _ in $(seq 25); do
        grim "$pics/$1.png"
        case $3 in
        change) [ "$(differ "$1" "$2")" != 0 ] && return ;;
        none) [ "$(differ "$1" "$2")" = 0 ] && return ;;
        esac
        sleep 0.2
    done
    return 1
}
# the outputs before the mark, once two looks in a row agree
grim "$pics/settled.png"
for _ in $(seq 25); do
    sleep 0.2
    mv "$pics/settled.png" "$pics/unmarked.png"
    grim "$pics/settled.png"
    [ "$(differ settled unmarked)" = 0 ] && break
done
[ "$(differ settled unmarked)" = 0 ] || fail "the outputs did not settle before the mark"
since=$(wc -l <"$pics/wire")
key m
for _ in $(seq 50); do
    rows=$(bar_alone "$since")
    [ -n "$rows" ] && break
    sleep 0.1
done
[ -n "$rows" ] || fail "no commit after the mark damaged the bar's rows alone: $(tail -n "+$since" \
    "$pics/wire" | grep -o 'damage_buffer([^)]*)' | tr '\n' ' ')"
shows marked unmarked change || fail "the mark changed nothing on the outputs"
key f
expect .fullscreen_mode 1
shows full marked change || fail "fullscreen changed nothing on the outputs"
key f
expect .fullscreen_mode 0
shows whole marked none ||
    fail "the bar drawn alone differs from a whole frame in $(differ marked whole) pixels"
key q
ended "$pid" 10 || fail "the viewer did not end on q"
pid=
echo "sway: the mark damaged the bar's $rows rows alone, as a whole frame draws them"

# a protocol error once the window is up, which no compositor sends on request: a go-between of
# the script's own passes the viewer's connection to sway's and back, bytes and descriptors,
# and on SIGUSR1 sends the viewer wl_display.error, after the last message it passed, and
# closes; the viewer must say so in one line, with the error's text, and end with status 1
cat >"$pics/between.py" <<'EOF'
import os, select, signal, socket, struct, sys
listening = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listening.bind(sys.argv[1])
listening.listen(1)
woken, wake = socket.socketpair()
wake.setblocking(False)
signal.set_wakeup_fd(wake.fileno())
signal.signal(signal.SIGUSR1, lambda *_: None)
viewer, _ = listening.accept()
compositor = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
compositor.connect(sys.argv[2])
other = {viewer: compositor, compositor: viewer}
while True:
    ready, _, _ = select.select([viewer, compositor, woken], [], [])
    if woken in ready:
        # wl_display@1.error(object wl_display@1, code 1, message), opcode 0
        text = b"the go-between closes the connection\0"
        text += b"\0" * (-len(text) % 4)
        args = struct.pack("<III", 1, 1, len(text.rstrip(b"\0")) + 1) + text
        viewer.sendall(struct.pack("<II", 1, (8 + len(args)) << 16) + args)
        sys.exit(0)
    for side in (viewer, compositor):
        if side in ready:
            data, fds, _, _ = socket.recv_fds(side, 1 << 16, 28)
            if not data:
                sys.exit(0)
            socket.send_fds(other[side], [data], fds)
            for fd in fds:
                os.close(fd)
EOF
python3 "$pics/between.py" "$XDG_RUNTIME_DIR/between-0" "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" &
between=$!
for _ in $(seq 50); do
    [ -S "$XDG_RUNTIME_DIR/between-0" ] && break
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/between-0" ] || fail "the go-between opened no socket"
WAYLAND_DISPLAY=between-0 timeout --kill-after=2 30 "$bin" "$images/1-gradient.png" \
    2>"$pics/broken" &
pid=$!
expect .name 1-gradient.png
# once the window has its size and its frames are done, so the viewer reads the error before
# it writes to a connection the go-between closed
for _ in $(seq 50); do
    [ "$(field .window_rect.height)" -gt 400 ] && break
    sleep 0.1
done
sleep 1
started=$(date +%s%N)
kill -USR1 "$between"
if ! ended "$pid" 5; then
    # the viewer itself, timeout's child, and the CPU it takes in a second, in clock ticks
    viewer=$(pgrep -P "$pid" | head -1)
    ticks() { awk '{ print $14 + $15 }' "/proc/$viewer/stat"; }
    before=$(ticks)
    sleep 1
    fail "the viewer outlived the protocol error, taking $(($(ticks) - before)) ticks of CPU a second"
fi
took=$((($(date +%s%N) - started) / 1000000))
pid=
wait "$between" 2>/dev/null || true
between=
said=$(cat "$pics/broken")
[ "$status" -eq 1 ] && [ "$(wc -l <"$pics/broken")" -eq 1 ] && echo "$said" | grep -qx \
    'nsxiv-ng: lost the connection to the Wayland compositor: Protocol error 1 on object wl_display@1: the go-between closes the connection' ||
    fail "after a protocol error the viewer ended with status $status, saying '$said'"
echo "sway: after a protocol error the viewer said '$said' and ended with status 1 in $took ms"

# a viewer whose compositor goes away says so, and ends with status 1
timeout --kill-after=2 30 "$bin" "$images/1-gradient.png" 2>"$pics/lost" &
pid=$!
expect .name 1-gradient.png
kill -KILL "$compositor"
wait "$compositor" 2>/dev/null || true
compositor=
ended "$pid" 10 || fail "the viewer did not end without its compositor"
pid=
said=$(cat "$pics/lost")
# one line, none of a library's own beside it
[ "$status" -eq 1 ] && [ "$(wc -l <"$pics/lost")" -eq 1 ] &&
    echo "$said" | grep -qx 'nsxiv-ng: lost the connection to the Wayland compositor: .*' ||
    fail "without its compositor the viewer ended with status $status, saying '$said'"
echo "sway: without its compositor the viewer said '$said' and ended with status 1"
