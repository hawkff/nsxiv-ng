#!/bin/sh
# Measures image viewers under Xvfb with nsxiv-ng-speed. Starts Xvfb at 3840x2160, runs the
# tool's calibration, generates the fixtures into a temporary folder, then runs the cases and
# prints a table for each run. With one viewer it measures it once. With two it runs them in
# ROUNDS rounds (5), each the baseline then the candidate, which may reduce how much the
# machine's drift tells them apart. A round gives each case the ratio of the candidate's median
# time to the baseline's, and of its peak memory; the run exits 1 when the median of a case's
# ratios is above 1 by more than MARGIN percent (10), or when the candidate's keys draw nothing
# or late more often than the baseline's in a round. With no viewer it calibrates only.
# Why 5 rounds and one grid start a run: on a shared machine one round of a viewer against
# itself gave ratios from 0.68 to 1.49. Both such runs of 3 rounds failed 10% (worst medians
# 1.18 and 1.12), and both runs of 5 passed. In the runs of 5 the grid cases' medians were 0.97
# to 1.00 with three grid starts a run and 0.84 to 1.07 with one; one start was chosen for its
# time, 33 minutes a comparison against 68.
# A frame ends once its window draws nothing for the settle time (500 ms), and the window must
# then stay quiet for one more settle time before the next key; what it draws then is late. A
# viewer that draws in steps, as the bar at once and the image once decoded, must draw each step
# within the settle time of the one before, else its frame reads as the first step and the
# rest counts as late. A step later than twice the settle time after the one before reads as
# part of the next key's frame, so a viewer that slow needs a longer --settle-ms.
# A viewer is a command line, split at spaces. SPEED_FLAGS passes flags to the tool, as
# SPEED_FLAGS='--repeats 40'.
# Needs Xvfb and ImageMagick 7; installs them on Debian when run as root, with the font the
# viewers' bars draw with. Builds the tool where cargo is, else takes the one built before.
# Usage: ci/speed.sh [BASELINE] [CANDIDATE]
set -eu
cd "$(dirname "$0")/.."
rounds=${ROUNDS:-5}
margin=${MARGIN:-10}
case $rounds in '' | *[!0-9]* | 0*)
	echo "ROUNDS is no whole number of at least 1: $rounds" >&2
	exit 2
	;;
esac
printf '%s\n' "$margin" | grep -Eqx '[0-9]+(\.[0-9]+)?' || {
	echo "MARGIN is no percentage: $margin" >&2
	exit 2
}

packages="xvfb imagemagick fonts-dejavu-core"
if command -v dpkg >/dev/null && ! dpkg -s $packages >/dev/null 2>&1; then
	if [ "$(id -u)" -eq 0 ]; then
		apt-get update -qq >/dev/null
		DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $packages >/dev/null
	else
		echo "warning: the speed bar needs $packages" >&2
	fi
fi

if command -v cargo >/dev/null; then
	cargo build --release --locked -q -p nsxiv-ng-speed
fi
speed=${CARGO_TARGET_DIR:-$PWD/target}/release/nsxiv-ng-speed
flags=${SPEED_FLAGS:-}

work=$(mktemp -d)
xvfb=
# the kill is best-effort, and the folder always goes; a signal ends the run with 1
cleanup() {
	[ -z "$xvfb" ] || kill "$xvfb" 2>/dev/null || true
	rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

Xvfb -displayfd 3 -screen 0 3840x2160x24 -nolisten tcp 3>"$work/display" 2>"$work/xvfb.log" &
xvfb=$!
while [ ! -s "$work/display" ]; do
	kill -0 "$xvfb" 2>/dev/null || {
		cat "$work/xvfb.log" >&2
		exit 1
	}
	sleep 0.1
done
DISPLAY=:$(cat "$work/display")
export DISPLAY

echo "== calibration" >&2
"$speed" calibrate $flags
[ $# -gt 0 ] || exit 0

echo "== fixtures" >&2
# four photos of 12 megapixels, 1,000 small images of 640x480 cut from ten larger ones, and
# one photo of 50 megapixels, none of them committed; halved chroma, as cameras write JPEG
mkdir "$work/photos" "$work/grid"
for seed in 1 2 3 4; do
	magick -seed "$seed" -size 4000x3000 plasma: -sampling-factor 4:2:0 -quality 90 "$work/photos/$seed.jpg"
done
for seed in 0 1 2 3 4 5 6 7 8 9; do
	magick -seed "$seed" -size 6400x4800 plasma: -crop 640x480 +repage -sampling-factor 4:2:0 -quality 85 "$work/grid/$seed-%02d.jpg"
done
magick -seed 50 -size 8660x5773 plasma: -sampling-factor 4:2:0 -quality 90 "$work/big.jpg"

if [ $# -eq 1 ]; then
	echo "== $1" >&2
	"$speed" measure $flags "$work" $1
	exit 0
fi
baseline=$1
candidate=$2
set --
round=1
while [ "$round" -le "$rounds" ]; do
	echo "== round $round of $rounds, baseline: $baseline" >&2
	"$speed" measure $flags "$work" $baseline >"$work/baseline-$round.tsv"
	cat "$work/baseline-$round.tsv"
	echo "== round $round of $rounds, candidate: $candidate" >&2
	"$speed" measure $flags "$work" $candidate >"$work/candidate-$round.tsv"
	cat "$work/candidate-$round.tsv"
	set -- "$@" "$work/baseline-$round.tsv" "$work/candidate-$round.tsv"
	round=$((round + 1))
done
echo "== candidate against baseline over $rounds rounds, margin $margin%" >&2
"$speed" compare --margin "$margin" "$@"
