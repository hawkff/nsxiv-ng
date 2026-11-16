#!/bin/sh
# Runs the viewer in a pseudo-terminal with no display, as over SSH on a headless machine, in
# mode $2.
#
# In "halfblocks", a terminal that answers no query, the red first image must come as red
# half-block cells, and a smaller terminal must move the status line to its new last row. In
# "sixel", a terminal with sixels and no kitty graphics, the images must come as sixels of
# the picture alone, written from the cell at or before its corner, in whole bands short
# of the status line, each on a cleared screen, also in a smaller terminal, and past the
# probe's queries no kitty command may go, as such a terminal may print it.
#
# In "kitty" and "kitty-files", this script answers the viewer's queries as a terminal with
# kitty graphics would, one that cannot read the viewer's files and one that can: the large red
# first image must come at the terminal's pixels, inline as PNG or as raw RGB in a file, placed
# over the rows above the status line; the small blue second one, shown whole at 100%, alone
# in a box of whole cells, at its own pixels, which the terminal scales up, sent ahead while
# the viewer waits for a key. j and k must then only place them, the image going off the
# screen keeping its pixels in the terminal, an image the terminal says it dropped must go
# again, and w must show the blue image whole again by placement. Zoomed in, the view goes at
# the image's own scale, clear above and below the image; with the bar hidden, the status
# line's row is erased before the frame, so no text shows through it. In "kitty-files", the
# folder of the frames' files goes with the viewer.
#
# In "kitty-budget", a large terminal and three images whose frames, as RGB files, fit two at a
# time in what the terminal keeps. The viewer starts on the second, and must send only the
# third ahead, as the first would push it out. j and k then only place the third and the
# second, the first goes ahead from the second, pushing the third out, and k only places it.
#
# In "herdr", a herdr pane, which sends an image on to its client only once placed, the image
# sent ahead must get a hidden placement of one cell in the status bar's last cell, j must then
# only place it, and hiding the bar must take the hidden placement away.
#
# In "tmux", the viewer runs in tmux with allow-passthrough, and this script is the terminal
# around tmux: the viewer must ask it through tmux, show each image as placeholders tmux draws,
# of a virtual placement it passes on, send the next image ahead, which j then only places,
# take the image before off once the placeholders of the next show, and k must only place the
# first image again.
#
# In halfblocks, kitty, kitty-files, sixel and tmux, the first image rewritten while the second
# shows must come back as it is now. Then m marks the image, which redraws the status line
# alone, and SIGTERM must end the viewer as q does: the terminal comes back in the mode it was
# found, without images, before -o prints the marked file.
#
# Left out: a helper process making frames ahead (in "kitty-files"), as a thread of the
# viewer's own makes them; and in "herdr", the first image sent ahead again once the bar is
# back, as the viewer keeps the first image's frame and places it. chafa's symbols are half
# blocks. The tmux mode
# skips -o, which prints in a pane that closes with the viewer.
#
# In "ctrl-c", a terminal that answers no query, C-c, which no binding takes, as the terminal
# passes it on with isig off, must end the viewer as q does: status 0, the terminal's mode as it
# was, and -o printing the mark. In "beep", once the first image's frame is up, k and then q
# must write one BEL and then what q writes on its way out, nothing else, and leave the
# terminal's mode as it was. In "hangup", a terminal with kitty graphics that reads the viewer's
# files, once a frame went by file, goes away, the side that holds the pseudo-terminal's master
# killed: the viewer must end, its folder of frames gone, and nothing of it left running.
#
# packages: imagemagick tmux procps fonts-dejavu-core
# modes: halfblocks kitty kitty-files kitty-budget herdr sixel tmux ctrl-c beep hangup
set -eu
bin=$1
mode=${2:-halfblocks}
pics=$(mktemp -d)
session=
# in "hangup", the viewer and its folder of frames once its terminal is taken away, as
# nothing else ends a viewer that outlives it
hung=
frames=
# whether process $1 runs: neither gone nor ended and waiting to be reaped
running() {
    state=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -c1)
    [ -n "$state" ] && [ "$state" != Z ]
}
# this run's own job, tmux server and folder go on every way out, each step whatever the one
# before it came to
stop() {
    set +e
    [ -z "$session" ] || kill "$session" 2>/dev/null
    [ "$mode" != tmux ] || tmux -L "nsxiv-smoke-$$" kill-server 2>/dev/null
    if [ -n "$hung" ]; then
        kill -TERM "$hung" 2>/dev/null
        for _ in $(seq 40); do
            running "$hung" || break
            sleep 0.1
        done
        ! running "$hung" || kill -KILL "$hung" 2>/dev/null
    fi
    [ -z "$frames" ] || rm -rf "$frames"
    rm -rf "$pics"
}
trap stop EXIT
trap 'exit 1' HUP INT TERM
out=$pics/out
keys=$pics/keys
if [ "$mode" = kitty-budget ]; then
    # 4000x1980 pixels, 23.8 MB as RGB: two such frames fit in the 56 MiB the viewer keeps,
    # three do not
    for color in 1-red 2-lime 3-blue; do magick -size 4000x1980 "xc:${color#*-}" "$pics/$color.png"; done
    winsize=100,400,4000,2000 limit=120 start='-n 2'
else
    magick -size 2000x1160 xc:red "$pics/1-red.png"
    magick -size 700x232 xc:blue "$pics/2-blue.png"
    winsize=30,100,1000,600 limit=60 start=
fi
mkfifo "$keys"
unset DISPLAY WAYLAND_DISPLAY TMUX TERM_PROGRAM
# No config or cache of the user's: a config may bind the keys sent here to something else, or
# name a font that is not installed, of which the viewer warns once it has left the terminal.
# A font is among the packages, as with none the session asks for one more redraw once its
# fonts have finished loading, which "beep" could take for k's.
export XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"
server=nsxiv-smoke-$$
export TERM=xterm-256color COLORTERM=truecolor bin pics mode winsize server start
if [ "$mode" = herdr ]; then export TERM_PROGRAM=herdr; fi
fail() { echo "$mode: $1" >&2; tail -c 1500 "$out" | od -c | tail -30 >&2; exit 1; }
# the session in script's terminal of 100 by 30 cells, or with the rows, columns and pixels of
# winsize, which stty cannot set: it notes the terminal and the viewer's pid, and compares the
# terminal's mode before and after the viewer
cat >"$pics/session" <<'EOF'
if [ "$mode" = halfblocks ]; then
    stty cols 100 rows 30
else # rows, columns, then pixels; 0x5414 is TIOCSWINSZ
    perl -e 'ioctl(STDIN, 0x5414, pack("S4", split(/,/, $ENV{winsize}))) or die "TIOCSWINSZ: $!\n"'
fi
tty >"$pics/tty"
before=$(stty -g)
if [ "$mode" = tmux ]; then # -u for UTF-8, which the placeholders are, in a locale without it
    # the pane's variables named, as a tmux server still running would give it its own
    tmux -u -L "$server" -f "$pics/tmux.conf" new-session "env pics='$pics' bin='$bin' mode=tmux sh $pics/run"
else
    sh "$pics/run"
fi
status=$?
[ "$(stty -g)" = "$before" ] || echo "the terminal mode changed"
exit "$status"
EOF
# the viewer in its terminal, which in tmux is the pane: its status, and whether it left that
# terminal's mode as it found it
cat >"$pics/run" <<'EOF'
before=$(stty -g)
if [ "$mode" = tmux ]; then output=; else output=-o; fi
sh -c 'echo $$ >"$pics/pid"; exec "$bin" $start $0 "$pics"' "$output"
status=$?
if [ "$(stty -g)" = "$before" ]; then left=same; else left=changed; fi
echo "$status $left" >"$pics/status"
exit "$status"
EOF
printf 'set -g allow-passthrough on\nset -g status off\nset -g escape-time 10\n' >"$pics/tmux.conf"
timeout "$limit" script -q -e -c "sh $pics/session" /dev/null <"$keys" >"$out" &
session=$!
exec 3>"$keys"
# waits for the viewer to write $2, a fixed string with -F or a pattern with -E as $1
drawn() {
    for _ in $(seq 100); do
        if grep -a -q "$1" -- "$2" "$out"; then return; fi
        sleep 0.1
    done
    fail "$3"
}
# waits for the status bar to show $1; in tmux, which redraws only the cells that changed, as
# tmux holds the pane
bar() {
    if [ "$mode" != tmux ]; then
        drawn -F "$1" "$2"
        return
    fi
    for _ in $(seq 100); do
        if tmux -L "$server" capture-pane -p 2>/dev/null | grep -q -F -- "$1"; then return; fi
        sleep 0.1
    done
    fail "$2"
}
# how many kitty commands of kind $1, as a=T, a=p or a=d, the viewer sent
sent() { grep -a -o -F "$(printf '\033_G')$1," "$out" | wc -l; }
# waits up to $3 seconds for the viewer to have sent $2 kitty commands of kind $1
sends() {
    for _ in $(seq $(($3 * 10))); do
        if [ "$(sent "$1")" -ge "$2" ]; then return; fi
        sleep 0.1
    done
    fail "$4"
}
# the id of the last kitty command of kind $1
last() { grep -a -o -E "$(printf '\033_G')$1,(U=1,)?i=[0-9]+" "$out" | tail -1 | sed 's/.*i=//'; }
# how many sixel images the viewer drew
sixels() { grep -a -o -F "$(printf '\033P')" "$out" | wc -l; }
# a pattern for placeholders tmux drew in the color of image $1: U+10EEEE in the foreground of
# its low byte, perhaps after a move of the cursor
placed() { printf '\033\\[38;5;%sm(\033\\[[0-9;]*H)?\364\216\273\256' "$(($1 & 255))"; }
# The image the viewer sent as id $1, written to $pics/image-$1.png, its kind in $pics/sent:
# PNG chunks up to the one with m=0, or raw RGB or RGBA, as f=24 or f=32 says, in the file
# whose path came base64 encoded
picture() {
    awk -v id="$1" 'BEGIN { RS = "\033\\\\" }
        {
            at = index($0, "\033_G"); if (!at) next
            body = substr($0, at + 3); semi = index(body, ";")
            keys = "," substr(body, 1, semi - 1) ","; data = substr(body, semi + 1)
            if (keys ~ /,a=[Tt],/ && index(keys, ",i=" id ",")) {
                if (keys ~ /,t=t,/) {
                    match(keys, /,s=[0-9]+,/); w = substr(keys, RSTART + 3, RLENGTH - 4)
                    match(keys, /,v=[0-9]+,/); h = substr(keys, RSTART + 3, RLENGTH - 4)
                    print (keys ~ /,f=32,/ ? "rgba" : "rgb"), w, h, data
                    exit
                }
                taking = 1
                payload = ""
            }
            if (taking) {
                payload = payload data
                if (keys ~ /,m=0,/) { print "png", payload; exit }
            }
        }' "$out" >"$pics/sent"
    read -r kind w h data <"$pics/sent" || return 1
    if [ "$kind" != png ]; then
        magick -size "${w}x$h" -depth 8 "$kind:$(echo "$data" | base64 -d)" "$pics/image-$1.png" 2>/dev/null
    else
        echo "$w" | base64 -d >"$pics/image-$1.png" 2>/dev/null
    fi
}
# pixel x,y of image $1 as r,g,b,a, from its hex, which names no alpha in an image without it
pixel() {
    hex=$(magick "$pics/image-$1.png" -format "%[hex:p{$2}]" info: 2>/dev/null) || return 1
    [ ${#hex} -eq 6 ] && hex=${hex}FF
    printf '%d,%d,%d,%d' "0x$(echo "$hex" | cut -c1-2)" "0x$(echo "$hex" | cut -c3-4)" \
        "0x$(echo "$hex" | cut -c5-6)" "0x$(echo "$hex" | cut -c7-8)"
}
size() { magick "$pics/image-$1.png" -format '%wx%h' info:; }
# rgba for image $1 with alpha, else rgb
alpha() {
    hex=$(magick "$pics/image-$1.png" -format '%[hex:p{0,0}]' info:)
    if [ ${#hex} -eq 8 ]; then echo rgba; else echo rgb; fi
}
# r,g,b,a as clear, red, lime, blue or white, within the few steps scaling may take off
hue() {
    echo "$1" | awk -F, '{ hi = 250; lo = 5
        if ($4 <= lo) print "clear"
        else if ($4 < hi) print $0
        else if ($1 >= hi && $2 <= lo && $3 <= lo) print "red"
        else if ($1 <= lo && $2 >= hi && $3 <= lo) print "lime"
        else if ($1 <= lo && $2 <= lo && $3 >= hi) print "blue"
        else if ($1 >= hi && $2 >= hi && $3 >= hi) print "white"
        else print $0 }'
}
# waits for the last kitty image to show $1 at pixel $2; the image's id goes in $shown
# waits for image $1 to have come whole, as its last chunk may follow the first
arrived() {
    for _ in $(seq 100); do
        if picture "$1" && pixel "$1" 0,0 >/dev/null; then return; fi
        sleep 0.1
    done
    fail "$2"
}
showing() {
    for _ in $(seq 100); do
        shown=$(last a=T)
        if [ -n "$shown" ] && picture "$shown" && [ "$(hue "$(pixel "$shown" "$2")")" = "$1" ]; then return; fi
        sleep 0.1
    done
    fail "$3"
}
# the kitty commands of each kind the viewer sent so far
counts() { echo "$(sent a=T) $(sent a=p) $(sent a=d)"; }

if [ "$mode" = ctrl-c ]; then
    drawn -F 1-red.png "the status line never named the first image"
    printf m >&3
    drawn -F 2-blue.png "m did not mark the first image and go on"
    # the byte reaches the viewer's bindings only where the terminal makes no signal of it
    stty -a -F "$(cat "$pics/tty")" | grep -q -w -- -isig ||
        fail "the viewer's terminal has isig on, so C-c would come as SIGINT, not as a key"
    printf '\003' >&3
    for _ in $(seq 50); do
        running "$session" || break
        sleep 0.1
    done
    running "$session" && fail "C-c did not end the viewer"
    wait "$session" || fail "the viewer ended on C-c as '$(cat "$pics/status" 2>/dev/null)'"
    session=
    [ "$(cat "$pics/status")" = "0 same" ] ||
        fail "the viewer ended on C-c as '$(cat "$pics/status")', not with status 0 and its mode as it was"
    if grep -a -q 'the terminal mode changed' "$out"; then fail "the viewer left the terminal in another mode"; fi
    leave=$(printf '\033[?1049l')
    at=$(grep -a -b -o -F "$leave" "$out" | tail -1 | cut -d: -f1)
    [ -n "$at" ] || fail "the viewer did not leave the alternate screen"
    tail -c +$((at + ${#leave} + 1)) "$out" | grep -a -q -F "$pics/1-red.png" ||
        fail "-o did not print the marked file once the terminal was back"
    echo "$mode: C-c ended the viewer with status 0, the terminal as it was, then -o printed $pics/1-red.png"
    exit
fi
if [ "$mode" = beep ]; then
    drawn -F 1-red.png "the status line never named the first image"
    # the first image's frame up, its red half blocks and the synchronized output it ends
    drawn -E ';2;2[45][0-9];[0-9]{1,2};[0-9]{1,2}m' "no red half blocks came for the red image"
    red=$(grep -a -b -o -E ';2;2[45][0-9];[0-9]{1,2};[0-9]{1,2}m' "$out" | head -1 | cut -d: -f1)
    for _ in $(seq 100); do
        tail -c +$((red + 1)) "$out" | grep -a -q -F "$(printf '\033[?2026l')" && break
        sleep 0.1
    done
    tail -c +$((red + 1)) "$out" | grep -a -q -F "$(printf '\033[?2026l')" ||
        fail "the first image's frame never ended"
    before=$(wc -c <"$out")
    printf kq >&3
    wait "$session" || fail "the viewer ended on q as '$(cat "$pics/status" 2>/dev/null)'"
    session=
    [ "$(cat "$pics/status")" = "0 same" ] ||
        fail "the viewer ended on q as '$(cat "$pics/status")', not with status 0 and its mode as it was"
    # one BEL, then what q writes on its way out: the colors, line wrapping and cursor as they
    # were, and the main screen again
    tail -c +$((before + 1)) "$out" >"$pics/after-k"
    printf '\007\033[0m\033[?7h\033[?25h\033[?1049l' | cmp -s - "$pics/after-k" ||
        fail "k and q on the first image wrote '$(od -An -c "$pics/after-k" | tr -s ' \n' ' ')', not one BEL and q's way out"
    echo "$mode: k on the first image wrote one BEL and nothing else before q's way out"
    exit
fi
if [ "$mode" = hangup ]; then
    drawn -F "$(printf '\033_Gi=31,s=1,v=1,a=q')" "the viewer did not ask whether the terminal shows kitty graphics"
    printf '\033_Gi=31;OK\033\\\033_Gi=32;OK\033\\\033[?62;22c' >&3
    drawn -F "$(printf '\033_Ga=T,')" "the viewer sent no kitty image"
    folder=$(dirname "$(grep -a -o "$(printf '\033')_Gi=32,[^;]*;[A-Za-z0-9+/=]*" "$out" | head -1 | sed 's/.*;//' | base64 -d)")
    [ -d "$folder" ] || fail "the probe named no folder for frames: $folder"
    # removed on the way out, so by its name it is the viewer's own and no folder above it
    case ${folder#/tmp/tty-graphics-protocol-nsxiv-} in
    "$folder" | */* | '') fail "the probe named $folder, no folder of the viewer's own" ;;
    esac
    frames=$folder
    # a frame that went by file, a=T with t=t and the file's path, which is in that folder
    drawn -E "$(printf '\033')_Ga=T,[^;]*t=t[^;]*;[A-Za-z0-9+/=]+" "no frame went by file"
    file=$(grep -a -o -E "$(printf '\033')_Ga=T,[^;]*t=t[^;]*;[A-Za-z0-9+/=]+" "$out" | head -1 | sed 's/.*;//' | base64 -d)
    [ "$(dirname "$file")" = "$folder" ] && [ -f "$file" ] ||
        fail "the frame's file, $file, is not in the folder of frames, $folder"
    viewer=$(cat "$pics/pid")
    hung=$viewer
    # script, timeout's child, holds the pseudo-terminal's master
    terminal=$(pgrep -P "$session" | head -1)
    [ -n "$terminal" ] || fail "no terminal to take away"
    kill -KILL "$terminal"
    for _ in $(seq 50); do
        ! running "$viewer" && [ ! -d "$folder" ] && [ -z "$(pgrep -f -- "$pics/" || true)" ] && break
        sleep 0.1
    done
    running "$viewer" && fail "the viewer outlived its terminal"
    [ ! -d "$folder" ] || fail "the viewer left the folder of its frames, $folder"
    left=$(pgrep -a -f -- "$pics/" || true)
    [ -z "$left" ] || fail "left running without the terminal: $left"
    wait "$session" 2>/dev/null || true
    session=
    hung=
    echo "$mode: once a frame went by file and the terminal went away, the viewer ended, its folder of frames gone, nothing left running"
    exit
fi

if [ "$mode" = halfblocks ]; then
    drawn -F 1-red.png "the status line never named the first image"
    # the cells' colors, as 38;2;R;G;B or 48;2;R;G;B
    drawn -E ';2;2[45][0-9];[0-9]{1,2};[0-9]{1,2}m' "no red half blocks came for the red image"
    printf j >&3
    drawn -F 2-blue.png "j did not name the second image"
    drawn -E ';2;[0-9]{1,2};[0-9]{1,2};2[45][0-9]m' "j did not draw the blue image"
    magick -size 2000x1160 xc:lime "$pics/1-red.png"
    printf k >&3
    drawn -E ';2;[0-9]{1,2};2[45][0-9];[0-9]{1,2}m' "k drew the first image as it was before it changed"
    printf j >&3
    stty -F "$(cat "$pics/tty")" cols 80 rows 20
    drawn -F "$(printf '\033[20;1H')" "a smaller terminal did not get the status line on its last row"
elif [ "$mode" = sixel ]; then
    drawn -F "$(printf '\033_Gi=31,s=1,v=1,a=q')" "the viewer did not ask what the terminal shows"
    printf '\033[?62;4;22c' >&3 # foot's answer: sixels, and no kitty graphics
    # 100 columns of 10 pixels, and 29 rows of 20 above the bar in whole bands of 6: 1000x576,
    # where the red picture lies over 3 to 996: 993x576 from column 1, 3 pixels to the left
    sixel_places() { grep -a -o -E "$(printf '\033')\[[0-9]+;[0-9]+H$(printf '\033')P[0-9;]*q\"1;1;[0-9]+;[0-9]+" "$out" |
        sed "s/$(printf '\033')P[0-9;]*q\"1;1;/ /; s/$(printf '\033')\[//; s/H / /"; }
    sixel_at() {
        for _ in $(seq 100); do sixel_places | grep -q -x -F "$1" && return; sleep 0.1; done
        fail "$2, but at row;column and of width;height: $(sixel_places | tr '\n' ' ')"
    }
    sixel_at '1;1 993;576' "the viewer drew the red picture's 993x576 pixels from no cell 1;1"
    drawn -F 1-red.png "the status line never named the first image"
    # the colors, in percent
    drawn -E '#[0-9]+;2;(9[5-9]|100);[0-4];[0-4]([^0-9]|$)' "the first sixel image has no red"
    printf j >&3
    drawn -F 2-blue.png "j did not name the second image"
    drawn -E '#[0-9]+;2;[0-4];[0-4];(9[5-9]|100)([^0-9]|$)' "j drew no blue sixel image"
    # at 875x290 over 63 to 938 and 143 to 433: from the cell at 60,140
    sixel_at '8;7 875;290' "j drew the blue picture's 875x290 pixels from no cell 8;7"
    magick -size 2000x1160 xc:lime "$pics/1-red.png"
    printf k >&3
    drawn -E '#[0-9]+;2;[0-4];(9[5-9]|100);[0-4]([^0-9]|$)' "k drew the first image as it was before it changed"
    printf j >&3
    # 80 columns of 12 pixels, and 19 rows of 30 above the bar: 960x570, the frames kept for
    # the larger terminal forgotten, the pictures now as wide; then k and j in it
    stty -F "$(cat "$pics/tty")" cols 80 rows 20
    # at 960x318 from 126 in cells of 30: from the cell at 0,120
    sixel_at '5;1 960;318' "a smaller terminal got no 960x318 pixels of the blue picture from cell 5;1"
    smaller() { sixel_places | grep -c -E '^[0-9]+;1 960;'; }
    printf k >&3
    for _ in $(seq 100); do [ "$(smaller)" -ge 2 ] && break; sleep 0.1; done
    printf j >&3
    for _ in $(seq 100); do [ "$(smaller)" -ge 3 ] && break; sleep 0.1; done
    [ "$(smaller)" -ge 3 ] || fail "k and j drew $(smaller) sixel images in the smaller terminal, not 3"
elif [ "$mode" = tmux ]; then
    # tmux asks the terminal's attributes as it starts, then passes on what the viewer asks
    drawn -F "$(printf '\033[c')" "tmux did not ask the terminal's attributes"
    printf '\033[?62;22c' >&3
    drawn -F "$(printf '\033_Gi=31,s=1,v=1,a=q')" "the viewer did not ask through tmux whether the terminal shows kitty graphics"
    # a terminal that cannot read the viewer's files, so the images go as PNG, in chunks tmux passes on
    printf '\033_Gi=31;OK\033\\\033_Gi=32;EBADF:Failed to read image file\033\\\033[?62;22c' >&3
    drawn -F "$(printf '\033_Ga=T,U=1,')" "the viewer sent no image for placeholders through tmux"
    bar 1-red.png "the status bar never named the first image"
    showing red 500,290 "the viewer sent no red image through tmux"
    red=$shown
    drawn -E "$(placed "$red")" "tmux drew no placeholders for the red image"
    drawn -F "$(printf '\033_Ga=t,')" "the viewer did not send the next image ahead"
    blue=$(last a=t)
    arrived "$blue" "the image sent ahead came as no image"
    # through tmux, an image goes with its view, whose box the placeholders fill
    [ "$(size "$red") $(size "$blue") $(hue "$(pixel "$blue" 400,232)")" = "1000x580 800x464 blue" ] ||
        fail "the images went through tmux as $(size "$red") and $(size "$blue"), not as the red and blue views"
    printf j >&3
    drawn -F "$(printf '\033_Ga=p,U=1,i=%s,' "$blue")" "j did not place the blue image sent ahead"
    bar 2-blue.png "j did not name the second image"
    drawn -E "$(placed "$blue")" "tmux drew no placeholders for the blue image"
    # the red image's placement goes once tmux drew the blue placeholders over it, not while it shows
    drawn -F "$(printf '\033_Ga=d,d=i,i=%s,p=1,' "$red")" "j did not take the red image's placement away"
    hidden=$(grep -a -b -o -F "$(printf '\033_Ga=d,d=i,i=%s,p=1,' "$red")" "$out" | head -1 | cut -d: -f1)
    shown=$(grep -a -b -o -E "$(placed "$blue")" "$out" | head -1 | cut -d: -f1)
    [ "$shown" -lt "$hidden" ] || fail "the red image's placement went at byte $hidden, before tmux drew the blue one at $shown"
    printf k >&3
    drawn -F "$(printf '\033_Ga=p,U=1,i=%s,' "$red")" "k did not place the red image again"
    [ "$(sent a=T) $(sent a=t)" = "1 1" ] || fail "j or k sent an image again"
    magick -size 2000x1160 xc:lime "$pics/1-red.png"
    printf j >&3
    drawn -F "$(printf '\033_Ga=p,U=1,i=%s,' "$blue")" "j did not place the blue image again"
    printf k >&3
    showing lime 500,290 "k sent the first image as it was before it changed"
    lime=$shown
    printf j >&3
    bar 2-blue.png "j did not name the second image again"
    sleep 1
    before=$(counts)
    printf m >&3
    bar '*1 |' "m did not mark the image"
    sleep 1
    [ "$(counts)" = "$before" ] || fail "the mark sent or placed an image: kitty commands went from $before to $(counts)"
    kill -TERM "$(cat "$pics/pid")"
    wait "$session" || fail "SIGTERM did not end the viewer cleanly"
    session=
    [ "$(cat "$pics/status" 2>/dev/null)" = "0 same" ] ||
        fail "the viewer in tmux ended as '$(cat "$pics/status" 2>/dev/null)', not with status 0 and its pane's mode as it was"
    for image in "$red" "$blue" "$lime"; do
        grep -a -q -F "$(printf '\033_Ga=d,d=I,i=%s,' "$image")" "$out" ||
            fail "the viewer left image $image in the terminal around tmux"
    done
    echo "$mode: drew $(wc -c <"$out") bytes through tmux"
    exit
else
    drawn -F "$(printf '\033_Gi=31,s=1,v=1,a=q')" "the viewer did not ask whether the terminal shows kitty graphics"
    # the red image covers its canvas, so a file holds it as RGB, without alpha
    if [ "$mode" = kitty ]; then files='EBADF:Failed to read image file' medium=png; else files=OK medium=rgb; fi
    printf '\033_Gi=31;OK\033\\\033_Gi=32;%s\033\\\033[?62;22c' "$files" >&3 # herdr's answers
    if [ "$mode" = kitty-budget ]; then
        # started on the second image, of two neighbors that do not both fit beside it, the
        # third goes ahead, and the first does not
        sends a=t 1 60 "the viewer did not send the next image ahead"
        second=$(last a=T)
        third=$(last a=t)
        sleep 2
        [ "$(sent a=T) $(sent a=t)" = "1 1" ] ||
            fail "the viewer sent $(sent a=t) images ahead, of two neighbors that do not both fit beside the one shown"
        printf j >&3
        drawn -F "$(printf '\033_Ga=p,i=%s,' "$third")" "j did not place the third image sent ahead"
        printf k >&3
        drawn -F "$(printf '\033_Ga=p,i=%s,' "$second")" "k did not place the second image again"
        # from the second, going back, the first goes ahead, pushing the third out
        sends a=t 2 60 "the first image did not go ahead from the second"
        first=$(last a=t)
        drawn -F "$(printf '\033_Ga=d,d=I,i=%s,' "$third")" "the first image sent ahead did not push the third out of what the terminal keeps"
        printf k >&3
        drawn -F "$(printf '\033_Ga=p,i=%s,' "$first")" "k did not place the first image sent ahead"
        [ "$(sent a=T)" -eq 1 ] || fail "j or k sent an image, where they had only to place them"
        kill -TERM "$(cat "$pics/pid")"
        wait "$session" || fail "SIGTERM did not end the viewer cleanly"
        session=
        echo "$mode: sent one neighbor ahead of two, and placed every image after the first"
        exit
    fi
    if [ "$mode" = herdr ]; then
        drawn -F "$(printf '\033_Ga=T,')" "the viewer sent no kitty image"
        # the next image goes ahead with a placement of one cell, at the bar's last cell, under
        # its background
        ahead=$(printf '\033[30;100H\033_Ga=T,i=')
        drawn -F "$ahead" "the viewer did not place the image it sent ahead in the bar's last cell"
        blue=$(grep -a -o -E "$(printf '\033')\[30;100H$(printf '\033')_Ga=T,i=[0-9]+" "$out" | tail -1 | sed 's/.*i=//')
        drawn -F "$(printf '\033_Ga=T,i=%s,p=2,c=1,r=1,z=-1073741825,C=1,' "$blue")" "the image sent ahead did not go as one hidden cell"
        transmissions=$(sent a=T)
        printf j >&3
        drawn -F "$(printf '\033_Ga=p,i=%s,p=1,' "$blue")" "j did not place the image sent ahead"
        drawn -F 2-blue.png "j did not name the second image"
        [ "$(sent a=T)" = "$transmissions" ] || fail "j sent the image sent ahead again"
        # with the bar hidden, nothing covers that cell, so the hidden placement goes, alone or
        # with its image
        printf b >&3
        drawn -E "$(printf '\033')_Ga=d,d=(i,i=$blue,p=2|I,i=$blue)," "hiding the bar left the hidden placement where it would show"
        printf b >&3
    else
        drawn -F "$(printf '\033_Ga=T,')" "the viewer sent no kitty image"
        drawn -F 1-red.png "the status line never named the first image"
        showing red 500,290 "the viewer sent no red image"
        red=$shown
        read -r kind _ <"$pics/sent"
        # the view at the terminal's pixels, opaque, placed over the 100 columns and 29 rows
        [ "$kind $(size "$red") $(hue "$(pixel "$red" 995,575)") $(alpha "$red")" = "$medium 1000x580 red rgb" ] ||
            fail "the red image went as $kind $(alpha "$red") $(size "$red") $(pixel "$red" 995,575), not as $medium filling 1000x580 pixels with red, without alpha"
        drawn -F "$(printf '\033_Ga=T,i=%s,p=1,C=1,c=100,r=29,' "$red")" "the red image was not placed over the rows above the bar"
        if [ "$mode" = kitty-files ]; then
            # the folder the frames go in, from the file the probe named
            folder=$(dirname "$(grep -a -o "$(printf '\033')_Gi=32,[^;]*;[A-Za-z0-9+/=]*" "$out" | head -1 | sed 's/.*;//' | base64 -d)")
            [ -d "$folder" ] || fail "the probe named no folder for frames: $folder"
        fi
        drawn -F "$(printf '\033_Ga=t,')" "the viewer did not send the next image ahead"
        blue=$(last a=t)
        arrived "$blue" "the image sent ahead came as no image"
        # shown whole, the image goes alone, opaque, without the rest of the view
        read -r kind _ <"$pics/sent"
        [ "$kind $(size "$blue") $(hue "$(pixel "$blue" 350,116)") $(hue "$(pixel "$blue" 5,5)")" = "$medium 700x232 blue blue" ] ||
            fail "the blue image went ahead as $kind $(size "$blue") $(pixel "$blue" 350,116) $(pixel "$blue" 5,5), not alone as 700x232 blue pixels"
        printf j >&3
        # in a box of 88 columns by 15 rows from row 8 and column 7, 875x290 pixels at offsets 5 and 10
        drawn -F "$(printf '\033[8;7H\033_Ga=p,i=%s,p=1,C=1,c=88,r=15,X=5,Y=10,' "$blue")" "j did not place the blue image sent ahead in its box"
        drawn -F 2-blue.png "j did not name the second image"
        drawn -F "$(printf '\033_Ga=d,d=i,i=%s,p=1,' "$red")" "j did not take the red image off the screen, keeping it in the terminal"
        printf k >&3
        drawn -F "$(printf '\033_Ga=p,i=%s,' "$red")" "k did not show the red image again"
        printf j >&3
        drawn -E "$(printf '\033_Ga=p,i=%s,.*\033_Ga=p,i=%s,' "$red" "$blue")" "j did not show the blue image again"
        [ "$(sent a=T) $(sent a=t)" = "1 1" ] || fail "j or k sent an image again"
        # the terminal dropped the blue image for room: it goes again, with the red one still kept
        printf '\033_Gi=%s,p=1;ENOENT:No such image\033\\' "$blue" >&3
        sends a=T 2 10 "the viewer did not send the image the terminal dropped again"
        showing blue 350,116 "the viewer did not send the blue image alone again"
        blue=$shown
        [ "$(size "$blue")" = 700x232 ] || fail "the viewer sent $(size "$blue") in place of the blue image alone"
        [ "$(sent a=d,d=A)" -eq 1 ] ||
            fail "the viewer deleted every image on the screen after the first frame, freeing those it kept"
        magick -size 2000x1160 xc:lime "$pics/1-red.png"
        printf k >&3
        sends a=T 3 10 "k did not send the first image as it is now"
        showing lime 500,290 "k showed the first image as it was before it changed"
        printf j >&3
        drawn -F "$(printf '\033_Ga=p,i=%s,' "$blue")" "j did not show the blue image sent again"
        # zoomed in, the image runs past the sides, with room above and below: the frame is the
        # view, 533x309 pixels at the image's own scale, the terminal's background around the image
        printf + >&3
        sends a=T 4 10 "+ did not send the zoomed view"
        showing blue 266,154 "the zoomed view came as no image with blue amid it"
        zoomed=$shown
        [ "$(size "$zoomed") $(hue "$(pixel "$zoomed" 266,154)") $(hue "$(pixel "$zoomed" 5,5)")" = "533x309 blue clear" ] ||
            fail "the zoomed view went as $(size "$zoomed") $(pixel "$zoomed" 266,154) $(pixel "$zoomed" 5,5), not as 533x309 pixels with blue amid clear ones"
        whole=$(printf '\033[8;7H\033_Ga=p,i=%s,p=1,' "$blue")
        placements=$(grep -a -o -F "$whole" "$out" | wc -l)
        printf w >&3
        for _ in $(seq 100); do [ "$(grep -a -o -F "$whole" "$out" | wc -l)" -gt "$placements" ] && break; sleep 0.1; done
        [ "$(grep -a -o -F "$whole" "$out" | wc -l)" -gt "$placements" ] || fail "w did not show the blue image whole again"
        # with the bar hidden, the frame covers the last row too, whose status line must not show
        # through the frame's clear parts: the row is erased before the image comes
        from=$(wc -c <"$out")
        printf b >&3
        for _ in $(seq 100); do
            tail -c +$((from + 1)) "$out" >"$pics/after-b"
            grep -a -q -F "$(printf '\033_Ga=T,')" "$pics/after-b" && break
            sleep 0.1
        done
        erased=$(grep -a -b -o -F "$(printf '\033[30;1H\033[2K')" "$pics/after-b" | head -1 | cut -d: -f1)
        image=$(grep -a -b -o -F "$(printf '\033_Ga=T,')" "$pics/after-b" | head -1 | cut -d: -f1)
        [ -n "$image" ] || fail "b drew no frame"
        [ -n "$erased" ] && [ "$erased" -lt "$image" ] ||
            fail "with the bar hidden, the status line's row was not erased before the frame"
        printf b >&3
    fi
fi
printf m >&3
drawn -F '*1 |' "m did not mark the image"
# the mark, on the last image, changed only the status line, which goes without the image:
# the output that first shows it, from its synchronized output's start, holds no frame, which
# starts at the top left; a frame still due from before may come ahead of it
at=$(grep -a -b -o -F '*1 |' "$out" | head -1 | cut -d: -f1)
head -c "$at" "$out" >"$pics/before-mark"
start=$(grep -a -b -o -F "$(printf '\033[?2026h')" "$pics/before-mark" | tail -1 | cut -d: -f1)
if tail -c +$((start + 1)) "$pics/before-mark" | grep -a -q -F "$(printf '\033[?2026h\033[H')"; then
    fail "the mark drew the frame again, not the status line alone"
fi
kill -TERM "$(cat "$pics/pid")"
wait "$session" || fail "SIGTERM did not end the viewer cleanly"
session=
if [ "$mode" = sixel ]; then
    # each on a cleared screen, as the rows it does not cover and its own clear pixels would
    # keep the image before
    [ "$(grep -a -o -E "$(printf '\033')\[2J$(printf '\033')\[[0-9]+;[0-9]+H$(printf '\033')P" "$out" | wc -l)" -eq "$(sixels)" ] ||
        fail "a sixel image came without the screen cleared first, then the cursor moved"
    commands=$(grep -a -o -F "$(printf '\033_G')" "$out" | wc -l)
    queries=$(grep -a -o -E "$(printf '\033')_Gi=3[12],s=1,v=1,a=q," "$out" | wc -l)
    [ "$commands" -eq "$queries" ] ||
        fail "past the probe's $queries queries, $((commands - queries)) kitty commands went to a terminal without kitty graphics"
fi
if grep -a -q 'the terminal mode changed' "$out"; then fail "the viewer left the terminal in another mode"; fi
if [ "$mode" = kitty-files ]; then
    [ ! -d "$folder" ] || fail "the viewer left the folder of its frames, $folder"
fi
leave=$(printf '\033[?1049l')
at=$(grep -a -b -o -F "$leave" "$out" | tail -1 | cut -d: -f1)
[ -n "$at" ] || fail "the viewer did not leave the alternate screen"
case $mode in kitty* | herdr)
    grep -a -q -F "$(printf '\033_Ga=d,d=A,q=2\033\\\033[0m\033[?7h\033[?25h\033[?1049l')" "$out" ||
        fail "the viewer left its image on the screen"
esac
tail -c +$((at + ${#leave} + 1)) "$out" | grep -a -q -F "$pics/2-blue.png" ||
    fail "-o did not print the marked file once the terminal was back"
echo "$mode: drew $(wc -c <"$out") bytes, then printed the marked $pics/2-blue.png"
