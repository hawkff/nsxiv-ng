#!/bin/sh
# Edit mode in a Wayland window, in a headless sway at scale 1 with the window floating at its
# image's size: x on a 640x400 image, a filled rectangle by a drag, and three texts by t and a
# click, each ended by Return: ASCII words, letters past ASCII, and an é held long enough to
# repeat; then Control-c, the edit's copy: the selection must offer image/png, which wl-paste
# reads back as a PNG of the image's size, the rectangle red in it, the held é's line long,
# and a change where each text was typed, which a copy after Control-z lacks (a check of the
# region, not of the words); then w must save the edit beside the image, its pixels those of
# the copy with the texts undone. A 3200x2400 image of noise, whose PNG is some 26 MB,
# must come through the selection whole, its pixels the image's, after a reader that closed
# its pipe early after 1000 bytes, the PNG's start, which the viewer must outlive without a
# word; while a reader that took the copy's first 1000 bytes reads no more, its pipe full,
# words typed into the line must show in a new copy, and sway's close must quit the viewer.
# The bar, which starts hidden, is shown with b before the edit of the 640x400 image, whose
# coordinates are the image's. Every wait is bounded.
#
# The keys come from wtype, each run a new virtual keyboard. The pointer is a virtual pointer
# that a small client of the script's own plugs in, holds for the whole run, and moves and
# presses: sway's own seat cursor (`seat - cursor set|press|release`) has no device to reach a
# client through where none is plugged in, and with one moves nothing while a button is down,
# so no drag; and wlrctl unplugs its pointer after each command.
#
# runs on: debian arch
# packages: sway wtype wl-clipboard jq imagemagick python3 fonts-dejavu-core
# arch packages: sway wtype wl-clipboard jq imagemagick python ttf-dejavu
set -eu
bin=$1
pics=$(mktemp -d)
compositor=
holder=
viewer=
stalled=
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
trap 'exec 3>&-; for p in $stalled $viewer $holder $compositor; do stop "$p"; done; rm -rf "$pics"' EXIT
trap 'exit 1' HUP INT TERM
fail() {
    echo "wayland-edit: $1" >&2
    [ ! -s "$pics/stderr" ] || sed 's/^/  viewer: /' "$pics/stderr" >&2
    [ ! -s "$pics/compositor.log" ] || tail -5 "$pics/compositor.log" | sed 's/^/  sway: /' >&2
    exit 1
}
. "$(dirname "$0")/lib/edit.sh"
edit_images

export XDG_RUNTIME_DIR="$pics/runtime" XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
mkdir -m 700 "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK
# a copy without the file capabilities a container cannot grant
mkdir -p "$pics/bin"
install -m755 "$(command -v sway)" "$pics/bin/sway"
cat >"$pics/sway.conf" <<'EOF'
output HEADLESS-1 resolution 1280x800 position 0 0 scale 1
for_window [app_id="nsxiv-ng"] floating enable
EOF
WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    "$pics/bin/sway" -c "$pics/sway.conf" >"$pics/compositor.log" 2>&1 &
compositor=$!
for _ in $(seq 100); do
    for candidate in "$XDG_RUNTIME_DIR"/wayland-[0-9]; do
        [ -S "$candidate" ] && WAYLAND_DISPLAY=$(basename "$candidate")
    done
    for candidate in "$XDG_RUNTIME_DIR"/sway-ipc.*.sock; do
        [ -S "$candidate" ] && SWAYSOCK=$candidate
    done
    [ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${SWAYSOCK:-}" ] && break
    kill -0 "$compositor" 2>/dev/null || fail "sway did not start"
    sleep 0.1
done
[ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${SWAYSOCK:-}" ] || fail "sway opened no sockets"
export WAYLAND_DISPLAY SWAYSOCK
msg() { timeout 10 swaymsg "$@"; }

# a virtual pointer on the seat, held for the whole run and moved and pressed by the lines it
# reads, each written to pointer.done once the compositor took it: the Wayland wire by hand, as
# no Python binding of it is packaged everywhere
cat >"$pics/pointer.py" <<'PY'
import os, socket, struct, sys, time

wire = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
wire.connect(os.path.join(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"]))

def send(target, opcode, payload=b""):
    wire.sendall(struct.pack("<IHH", target, opcode, 8 + len(payload)) + payload)

def string(text):
    data = text.encode() + b"\0"
    return struct.pack("<I", len(data)) + data + b"\0" * (-len(data) % 4)

pending = b""
def event():
    global pending
    while True:
        if len(pending) >= 8:
            target, opsize = struct.unpack("<II", pending[:8])
            size = opsize >> 16
            if len(pending) >= size:
                body, pending = pending[8:size], pending[size:]
                if target == 1 and opsize & 0xFFFF == 0:
                    sys.exit("a protocol error")
                return target, opsize & 0xFFFF, body
        data = wire.recv(65536)
        if not data:
            sys.exit("the compositor closed the connection")
        pending += data

ids = iter(range(3, 1 << 30))
def sync():
    callback = next(ids)
    send(1, 0, struct.pack("<I", callback))
    while event()[0] != callback:
        pass

# the globals the registry lists before a sync's done
send(1, 1, struct.pack("<I", 2))
globals_ = {}
callback = next(ids)
send(1, 0, struct.pack("<I", callback))
while True:
    target, opcode, body = event()
    if target == 2 and opcode == 0:
        name, length = struct.unpack("<II", body[:8])
        globals_[body[8:8 + length - 1].decode()] = name
    elif target == callback:
        break
seat, manager, pointer = next(ids), next(ids), next(ids)
for interface, new_id in (("wl_seat", seat), ("zwlr_virtual_pointer_manager_v1", manager)):
    if interface not in globals_:
        sys.exit("no " + interface)
    send(2, 0, struct.pack("<I", globals_[interface]) + string(interface)
         + struct.pack("<II", 1, new_id))
send(manager, 0, struct.pack("<II", seat, pointer))
sync()
width, height = int(sys.argv[1]), int(sys.argv[2])
done = open(sys.argv[3], "a", buffering=1)
done.write("ready\n")
# move x y, press or release: motion_absolute, or the left button, and a frame
for line in sys.stdin:
    now = int(time.monotonic() * 1000) & 0xFFFFFFFF
    words = line.split()
    if words[0] == "move":
        x, y = int(words[1]), int(words[2])
        send(pointer, 1, struct.pack("<IIIII", now, x, y, width, height))
    else:
        send(pointer, 2, struct.pack("<III", now, 0x110, int(words[0] == "press")))
    send(pointer, 4)
    sync()
    done.write(line)
PY
mkfifo "$pics/pointer.in"
timeout 300 python3 "$pics/pointer.py" 1280 800 "$pics/pointer.done" <"$pics/pointer.in" \
    2>"$pics/pointer.err" &
holder=$!
exec 3>"$pics/pointer.in"
# waits for the pointer's answer to its line $1
answered() {
    for _ in $(seq 50); do
        [ "$(cat "$pics/pointer.done" 2>/dev/null | wc -l)" -ge "$1" ] && return
        sleep 0.1
    done
    fail "the virtual pointer did not answer: $(cat "$pics/pointer.err")"
}
lines=1
answered 1
pointer() {
    echo "$*" >&3
    lines=$((lines + 1))
    answered "$lines"
}

# the viewer's window in sway's tree
window() { msg -t get_tree | jq -c '.. | objects | select(.app_id == "nsxiv-ng")'; }
# each wtype plugs in a new virtual keyboard; the pause lets the viewer bind it first; wtype
# reads its arguments in the locale's encoding, which must be UTF-8
key() { LC_ALL=C.UTF-8 timeout 10 wtype -s 300 "$@"; }
# the pointer to $1, $2 of the window's content, whose corner is at $left, $top
point() { pointer move $((left + $1)) $((top + $2)); }
press() { pointer press; }
release() { pointer release; }

# the window's content's corner now, which sway may move as it places the window
corner() {
    left=$(window | jq -r '.rect.x + .window_rect.x')
    top=$(window | jq -r '.rect.y + .window_rect.y')
}
# b shows the bar, which starts hidden: the window takes the image's 400 rows and the bar's
# below them, and the edit's canvas is the image's size, as the coordinates here take it
bar_shown() {
    key b
    for _ in $(seq 50); do
        [ "$(window | jq -r '.window_rect.height')" -gt 400 ] && corner && return
        sleep 0.1
    done
    fail "b did not show the bar below the image"
}
# opens image $1 and waits for its window, floating at the image's width
open() {
    timeout -k 5 120 "$bin" "$1" 2>"$pics/stderr" &
    viewer=$!
    width=
    for _ in $(seq 100); do
        width=$(window | jq -r '.window_rect.width // empty' 2>/dev/null) &&
            [ -n "$width" ] && [ "$width" -ge "$2" ] && break
        sleep 0.1
    done
    [ -n "$width" ] && [ "$width" -ge "$2" ] || fail "no window $2 wide came for $1"
    sleep 0.5
    corner
}
# reads the selection as image/png into $1, once its types name it, which may take up to two
# minutes: the large copy encodes a 26 MB PNG first, which a debug build on a slow machine takes
# most of a minute for; a read that does not end in 30 seconds fails, as one the viewer never
# finishes
paste_png() {
    listed=
    for _ in $(seq 600); do
        timeout 2 wl-paste -l 2>/dev/null | grep -qx image/png && listed=yes && break
        sleep 0.2
    done
    [ -n "$listed" ] || fail "the selection never named image/png: \
$(timeout 2 wl-paste -l 2>&1 | paste -sd ' ')"
    timeout 30 wl-paste -n -t image/png >"$1" 2>"$pics/paste" && [ -s "$1" ] ||
        fail "the selection gave no image/png: $(cat "$pics/paste")"
}
# the viewer ends within 10 seconds, with status 0
quits() {
    for _ in $(seq 100); do
        kill -0 "$viewer" 2>/dev/null || break
        sleep 0.1
    done
    ! kill -0 "$viewer" 2>/dev/null || fail "the viewer did not quit"
    wait "$viewer" || fail "the viewer ended with status $?"
    viewer=
}

open "$pics/plain.png" 640
bar_shown
key x
# r and f twice: rectangles filled with the edit's red
key r f f
point 100 100
press
point 150 130
point 200 150
release
sleep 0.3
# t and a click, then words typed into the line, which Return ends: ASCII, then letters past
# it, which come as the characters the keys type, then an é held 1.2 seconds, whose repeats
# type it again, as the character the key types
text() {
    key t
    point "$1" "$2"
    press
    release
    sleep 0.3
}
text 300 300
key Hello
key -k Return
text 300 200
key éàü
key -k Return
text 300 100
key -P eacute -s 1200 -p eacute
key -k Return
key -M ctrl c -m ctrl
paste_png "$pics/copy.png"
size=$(magick identify -format '%wx%h' "$pics/copy.png")
[ "$size" = 640x400 ] || fail "the copy is $size, not the image's 640x400"
holds "$pics/copy.png" 'p{150,125}.r > 0.9 && p{150,125}.g < 0.4' ||
    fail "the copy has no red rectangle at 150,125"
# the held é's line: red over 60 pixels and more, where one é is some 17 wide
held=$(magick "$pics/copy.png[320x40+300+62]" -fuzz 20% -fill white -opaque '#ff383c' \
    -fill black +opaque white -trim -format '%w' info:)
[ "$held" -ge 60 ] 2>/dev/null || fail "the held é typed $held pixels wide, as no repeat"
# Control-z twice takes the held é and the letters past ASCII away, and once more the ASCII
# words: each copy differs from the one before where they were typed
key -M ctrl z -m ctrl
key -M ctrl z -m ctrl
key -M ctrl c -m ctrl
sleep 0.5
paste_png "$pics/ascii.png"
letters=$(differ "$pics/copy.png[120x40+300+162]" "$pics/ascii.png[120x40+300+162]")
[ "$letters" -gt 50 ] 2>/dev/null ||
    fail "the copy differs from one without the letters past ASCII in $letters pixels"
key -M ctrl z -m ctrl
key -M ctrl c -m ctrl
sleep 0.5
paste_png "$pics/undone.png"
words=$(differ "$pics/ascii.png[120x40+300+262]" "$pics/undone.png[120x40+300+262]")
[ "$words" -gt 50 ] 2>/dev/null ||
    fail "the copy differs from one without the words in $words pixels where they were typed"
echo "wayland-edit: the copy is a PNG of $size with the rectangle, the words in $words pixels," \
    "the letters past ASCII in $letters, the held é $held wide"
# w saves the edit beside the image, and shows it next
key w
for _ in $(seq 50); do
    [ -s "$pics/plain-edit.png" ] && break
    sleep 0.1
done
[ -s "$pics/plain-edit.png" ] || fail "w saved no plain-edit.png"
off=$(differ "$pics/plain-edit.png" "$pics/undone.png")
[ "$off" = 0 ] || fail "the saved edit differs from the copy in $off pixels"
echo "wayland-edit: w saved plain-edit.png, its pixels the copy's"
key q
quits

# the noise's PNG, read whole
open "$pics/noise.png" 100
key x
sleep 1
key -M ctrl c -m ctrl
# a reader that closes its pipe early: the viewer goes on, and says nothing of it. The types
# wait as paste_png's do, for the 26 MB PNG the copy encodes first
listed=
for _ in $(seq 600); do
    timeout 2 wl-paste -l 2>/dev/null | grep -qx image/png && listed=yes && break
    sleep 0.2
done
[ -n "$listed" ] || fail "the selection never named image/png"
timeout 30 wl-paste -n -t image/png | head -c 1000 >"$pics/early"
signature=$(head -c 8 "$pics/early" | od -An -tx1 | tr -d ' \n')
[ "$(wc -c <"$pics/early")" = 1000 ] && [ "$signature" = 89504e470d0a1a0a ] ||
    fail "the early reader got $(wc -c <"$pics/early") bytes, starting $signature, not a PNG's 1000"
sleep 0.5
kill -0 "$viewer" 2>/dev/null || fail "the viewer ended as a reader closed its pipe early"
[ ! -s "$pics/stderr" ] || fail "the viewer spoke as a reader closed its pipe early"
paste_png "$pics/big.png"
bytes=$(wc -c <"$pics/big.png")
size=$(magick identify -format '%wx%h' "$pics/big.png")
[ "$size" = 3200x2400 ] || fail "the large copy is $size, not 3200x2400"
off=$(differ "$pics/big.png" "$pics/noise.png")
[ "$off" = 0 ] || fail "the large copy differs from the image in $off pixels"
echo "wayland-edit: the copy of $bytes bytes came whole at $size"
# a reader that takes the copy's first 1000 bytes, then stops reading and stays: its pipe full,
# words typed into the line go in, as a copy then shows, and sway's close quits, as q does
# outside an edit, the reader still stalled. wl-paste writes into a FIFO that a sleep holds
# open after head took its share; neither has a timeout, so nothing but the script's own end
# lets the viewer's writes on before the close is checked
mkfifo "$pics/stall"
wl-paste -n -t image/png >"$pics/stall" &
stalled=$!
sh -c 'head -c 1000 >"$1"; exec sleep 600' - "$pics/prefix" <"$pics/stall" &
stalled="$stalled $!"
for _ in $(seq 100); do
    [ "$(wc -c <"$pics/prefix" 2>/dev/null || echo 0)" -ge 1000 ] && break
    sleep 0.1
done
[ "$(wc -c <"$pics/prefix" 2>/dev/null || echo 0)" -ge 1000 ] ||
    fail "the stalled reader got no 1000 bytes of the copy"
corner
text 200 200
key Hi
key -k Return
key -M ctrl c -m ctrl
# the copy before stays the selection until the new one's PNG is made, and a read of it then
# may get nothing, as it goes before its data is sent; the new one encodes a 26 MB PNG too,
# which may take a slow debug build most of a minute, so this waits up to two minutes and more
new=
for _ in $(seq 240); do
    timeout 30 wl-paste -n -t image/png >"$pics/typed.png" 2>/dev/null || true
    [ -s "$pics/typed.png" ] && ! cmp -s "$pics/typed.png" "$pics/big.png" && new=yes && break
    sleep 0.5
done
[ -n "$new" ] || fail "no new copy came while a reader stalled"
typed=$(differ "$pics/typed.png" "$pics/big.png")
[ "$typed" -gt 50 ] 2>/dev/null ||
    fail "the words typed while a reader stalled changed $typed pixels of the copy"
msg '[app_id="nsxiv-ng"] kill' >/dev/null
quits
for p in $stalled; do
    kill -0 "$p" 2>/dev/null || fail "the stalled reader ended before the viewer did"
done
echo "wayland-edit: with a reader stalled, the words typed changed $typed pixels of a new copy, and sway's close quit the viewer"
