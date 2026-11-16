#!/bin/sh
# Checks the decoding helpers' sandbox through the viewer, in X11 on Xvfb with no window manager,
# where the window sits at 0,0 and its pixels are the screen's. The status line `-v` prints must
# say the probe's helper locked down with seccomp, that open, exec, sockets and io_uring all
# failed in it, and name Landlock as the kernel offers it, which the script asks the kernel:
# a kernel that boots with Landlock off answers EOPNOTSUPP, and the line must then say none, so
# in a container on such a kernel Landlock's enforcement goes unchecked, and in a container
# whose own filter refuses io_uring, as Docker's does, the probe's io_uring check passes for
# that filter too. The run in a guest, on the distribution's own kernel, checks both.
# The line may add that a libheif plugin did not load, as one of Arch's whose library is an
# optional dependency. A HEIC, an SVG and an AVIF, which only a locked-down helper decodes, must
# show their pixels, with no warning, each unlike the one before it. Both again with a plugin
# that is no library beside the installed ones: the formats still show, and the line must say
# a plugin did not load where the installed libheif reports one. An SVG that names a PNG
# outside it must draw without it; the decoder's own resolver refuses such files, and the
# sandbox refuses them where it did not.
# A helper killed in a decode, and one that runs past the viewer's timeout of 30 seconds, must
# leave the viewer showing that the file did not load, alive, and showing the next image, and
# the reason on stderr when it quits. A viewer killed, or quit, while a helper decodes a file
# that never finishes must take that helper with it within three seconds.
#
# runs on: debian arch
# guest: also
# packages: imagemagick xvfb xdotool procps python3 fonts-dejavu-core
# packages: libheif1 libheif-plugin-libde265 libheif-plugin-dav1d
# arch packages: imagemagick xorg-server-xvfb xdotool procps-ng python libheif ttf-dejavu
set -eu
bin=$1
pics=$(mktemp -d)
xvfb=
viewer=
# this run's own server, viewer and folder go on every way out, and the viewer's helpers, which
# end with it unless a check failed on that; the server is waited for, as one still shutting
# down takes the socket of the next run's server on its display with it
trap '[ -z "$viewer" ] || { pkill -KILL -P "$viewer" 2>/dev/null; kill "$viewer" 2>/dev/null; } || true; [ -z "$xvfb" ] || { kill "$xvfb" 2>/dev/null; wait "$xvfb"; } || true; rm -rf "$pics"' EXIT
trap 'exit 1' HUP INT TERM
fail() {
    echo "sandbox: $1" >&2
    [ ! -s "$pics/stderr" ] || sed 's/^/  viewer: /' "$pics/stderr" >&2
    exit 1
}

# the status line, against Landlock as this kernel offers it: its ABI version, or why none;
# with $1, it must say that a libheif plugin did not load
landlock=$(python3 -c '
import ctypes, os
libc = ctypes.CDLL(None, use_errno=True)
abi = libc.syscall(444, None, 0, 1)  # landlock_create_ruleset(NULL, 0, VERSION)
print(abi if abi > 0 else os.strerror(ctypes.get_errno()))')
case $landlock in
# the helper asks for the access rights and scopes of ABI 9
[0-9]*) [ "$landlock" -ge 9 ] && expected=full || expected='\(partial\|full\)' ;;
*) expected=none ;;
esac
status_line() {
    note='\(, a plugin did not load\)'
    [ -n "${1:-}" ] || note="$note\\?"
    status=$("$bin" -v | sed -n 2p)
    echo "$status" | grep -qx "sandbox: seccomp, landlock $expected; libheif [0-9.]*$note" ||
        fail "-v says '$status', where the kernel's Landlock is $landlock"
    echo "sandbox: -v says '$status'; the kernel's Landlock: $landlock"
}
status_line

# a display of its own, on a number the server picks. -noreset: with no client left between two
# viewers the server would start over each time, and the one failure seen here, a viewer refused
# its display, came between two viewers
Xvfb -displayfd 3 -screen 0 1280x800x24 -nolisten tcp -noreset 3>"$pics/display" \
    2>"$pics/xvfb.log" &
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
export XDG_CONFIG_HOME="$pics/config" XDG_CACHE_HOME="$pics/cache"
# the status bar shown from the start, which the checks here read, as the config's show_bar
# asks; the viewer starts it hidden otherwise
mkdir -p "$XDG_CONFIG_HOME/nsxiv-ng"
echo 'show_bar = true' >"$XDG_CONFIG_HOME/nsxiv-ng/config.toml"

fixtures=$(cd "$(dirname "$0")/../testdata/fixtures" && pwd)
# red on the left, green on the right
cat >"$pics/shapes.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">
<rect width="100" height="100" fill="#f00"/><rect x="100" width="100" height="100" fill="#0f0"/>
</svg>
EOF
# a billion octaves of noise, which no decode finishes
cat >"$pics/noise.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><filter id="f">
<feTurbulence baseFrequency="0.05" numOctaves="1000000000"/></filter>
<rect width="64" height="64" filter="url(#f)"/></svg>
EOF
magick -size 100x100 xc:blue "$pics/outside.png"
# green, and over its middle the blue PNG outside it, named by its path
cat >"$pics/outside.svg" <<EOF
<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">
<rect width="200" height="100" fill="#0f0"/>
<image href="$pics/outside.png" x="50" width="100" height="100"/></svg>
EOF
magick -size 200x100 xc:gray "$pics/gray.png"

# whether process $1 runs: neither gone nor ended and waiting to be reaped
running() {
    state=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -c1)
    [ -n "$state" ] && [ "$state" != Z ]
}
# starts the viewer on the files given, its stderr kept
open() {
    "$bin" "$@" 2>"$pics/stderr" &
    viewer=$!
}
# waits $2 seconds at most for the window to show with the title $1; without a window manager,
# keys go to the window the pointer enters
titled() {
    for _ in $(seq $(($2 * 10))); do
        window=$(xdotool search --onlyvisible --classname '^nsxiv-ng$' 2>/dev/null | head -1) &&
            [ -n "$window" ] && [ "$(xdotool getwindowname "$window")" = "$1" ] &&
            xdotool mousemove 1 1 mousemove 40 40 && return
        if ! running "$viewer"; then
            status=0
            wait "$viewer" || status=$?
            viewer=
            fail "the viewer ended with status $status on $1"
        fi
        sleep 0.1
    done
    fail "no window showed $1"
}
# waits for the screen to pass the fx test $2, as $1, within ten seconds
shows() {
    for _ in $(seq 50); do
        magick import -window root "$pics/$1.png"
        [ "$(magick "$pics/$1.png" -format "%[fx:$2]" info:)" = 1 ] && return
        sleep 0.2
    done
    fail "$1 does not show: $(magick "$pics/$1.png" -crop 320x200+0+0 -format '%c' histogram:info: |
        sort -rn | head -3 | tr -s ' \n' ' ')"
}
# the screen's tests: the shapes, a gradient red at the top and blue at the bottom, and the
# text of what went wrong, dark on white, in the window's first 200 rows
shapes='p{50,50}.r > 0.9 && p{50,50}.g < 0.1 && p{150,50}.g > 0.9 && p{150,50}.r < 0.1'
gradient='p{160,5}.r > 0.8 && p{160,5}.b < 0.2 && p{160,194}.b > 0.8 && p{160,194}.r < 0.2'
quits() {
    xdotool key q
    for _ in $(seq 50); do
        running "$viewer" || break
        sleep 0.1
    done
    running "$viewer" && fail "the viewer did not end on q"
    wait "$viewer" || fail "the viewer did not end with status 0 on q"
    viewer=
}

# each format decodes in a helper, with nothing on stderr; each image differs from the one
# before it, as the title changes before the picture does
formats() {
    open "$fixtures/gradient.heic" "$pics/shapes.svg" "$fixtures/gradient.avif"
    titled gradient.heic 10
    shows heic "$gradient"
    xdotool key j
    titled shapes.svg 10
    shows svg "$shapes"
    xdotool key j
    titled gradient.avif 10
    shows avif "$gradient"
    quits
    [ ! -s "$pics/stderr" ] || fail "the decodes warned"
    echo "sandbox: the HEIC, the SVG and the AVIF showed their pixels from locked-down helpers"
}
formats

# again with a plugin that cannot load, a file that is no library, after the installed ones:
# libheif must stay, and say so
plugins=$(python3 -c '
import ctypes
heif = ctypes.CDLL("libheif.so.1")
heif.heif_get_plugin_directories.restype = ctypes.POINTER(ctypes.c_char_p)
found = heif.heif_get_plugin_directories()
directories = []
while found[len(directories)]:
    directories.append(found[len(directories)].decode())
print(":".join(directories))')
mkdir "$pics/plugins"
echo 'no library' >"$pics/plugins/libheif-broken.so"
export LIBHEIF_PLUGIN_PATH="$plugins:$pics/plugins"
# the note where the installed libheif's own heif_init fails for that plugin with error 11, as
# 1.23 does; 1.17 and 1.19 keep a plugin's failure inside heif_load_plugins and give 0
init=$(python3 -c '
import ctypes
class Error(ctypes.Structure):
    _fields_ = [("code", ctypes.c_int), ("subcode", ctypes.c_int), ("message", ctypes.c_char_p)]
heif = ctypes.CDLL("libheif.so.1")
heif.heif_init.restype = Error
print(heif.heif_init(None).code)' 2>/dev/null)
echo "sandbox: libheif's heif_init with the broken plugin gives $init"
case $init in
11) status_line broken ;;
0) status_line ;;
*) fail "libheif's heif_init gives $init with the broken plugin" ;;
esac
formats
unset LIBHEIF_PLUGIN_PATH

# the PNG the SVG names stays out: the middle is green, not blue
open "$pics/outside.svg"
titled outside.svg 10
shows outside 'p{100,50}.g > 0.9 && p{100,50}.b < 0.1 && p{20,50}.g > 0.9'
quits
echo "sandbox: the SVG drew without the PNG it names outside it"

# a helper killed in a decode, then one held past the timeout: the window comes with the text
# of what went wrong, and the next file, an SVG past a PNG, decodes in a new helper
for way in killed held; do
    open "$pics/noise.svg" "$pics/gray.png" "$pics/shapes.svg"
    if [ "$way" = killed ]; then
        # the helper that decodes the noise, a second into it
        for _ in $(seq 100); do
            helper=$(pgrep -P "$viewer" -f -- '--decode-helper ' | head -1) && [ -n "$helper" ] &&
                break
            sleep 0.1
        done
        [ -n "$helper" ] || fail "no helper started for the noise"
        sleep 1
        pkill -KILL -P "$viewer" -f -- '--decode-helper '
        titled noise.svg 10
    else
        titled noise.svg 45
    fi
    # the window's rows above the bar: white, with dark text
    for _ in $(seq 50); do
        magick import -window "$window" "$pics/$way.png"
        [ "$(magick "$pics/$way.png" -gravity north -crop 100%x90%+0+0 \
            -format '%[fx:mean > 0.8 && minima.r < 0.3]' info:)" = 1 ] && break
        sleep 0.2
    done
    [ "$(magick "$pics/$way.png" -gravity north -crop 100%x90%+0+0 \
        -format '%[fx:mean > 0.8 && minima.r < 0.3]' info:)" = 1 ] ||
        fail "the window shows no text of what went wrong with the noise"
    running "$viewer" || fail "the viewer ended with its helper $way"
    xdotool key j
    titled gray.png 10
    xdotool key j
    titled shapes.svg 10
    shows "$way-next" "$shapes"
    quits
    case $way in
    killed) why='the decoder crashed' ;;
    held) why='the decoder ran out of time' ;;
    esac
    grep -qx "nsxiv-ng: $pics/noise.svg: $why" "$pics/stderr" ||
        fail "stderr does not say $why: $(cat "$pics/stderr")"
    echo "sandbox: a helper $way in a decode left the viewer showing the next file; stderr: $why"
done

# a viewer that goes, killed or on q, takes its helpers with it, also one in a decode that
# never finishes: the noise, the second file, decoded ahead behind a PNG with q
helpers() { pgrep -P "$viewer" -f -- '--decode-helper ' | tr '\n' ' '; }
# whether every process of $1 has ended within three seconds; an ended one may stay a zombie
# of a parent that does not reap it
ended() {
    for _ in $(seq 30); do
        left=
        for helper in $1; do
            state=$(sed 's/.*) //' "/proc/$helper/stat" 2>/dev/null | cut -c1)
            [ -n "$state" ] && [ "$state" != Z ] && left="$left $helper"
        done
        [ -z "$left" ] && return
        sleep 0.1
    done
    return 1
}
for way in killed quit; do
    case $way in
    killed) open "$pics/noise.svg" ;;
    quit)
        open "$pics/gray.png" "$pics/noise.svg"
        titled gray.png 10
        ;;
    esac
    decoding=
    for _ in $(seq 100); do
        decoding=$(helpers)
        [ -n "$decoding" ] && break
        sleep 0.1
    done
    [ -n "$decoding" ] || fail "no helper started for the noise"
    sleep 1
    decoding=$(helpers)
    [ -n "$decoding" ] || fail "the helper for the noise ended before its viewer, $way"
    case $way in
    killed)
        kill -KILL "$viewer"
        wait "$viewer" 2>/dev/null || true
        viewer=
        ;;
    quit) quits ;;
    esac
    if ended "$decoding"; then
        echo "sandbox: the helpers ended with their viewer, $way in a decode"
    else
        outlived="${outlived:-}${outlived:+; }$way:$left"
        # so the next run has all the machine's cores
        kill -KILL $left 2>/dev/null || true
    fi
done
[ -z "${outlived:-}" ] || fail "helpers outlived their viewer: $outlived"
