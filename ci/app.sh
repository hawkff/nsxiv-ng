#!/bin/sh
# Checks nsxiv-ng.app on macOS, apart from ci/check.sh, as it makes release builds: that
# dist/macos/app.sh makes it universal, its binary holding an arm64 slice and an x86_64 one,
# each built for the minimum macOS its Info.plist gives; that the Info.plist passes plutil and
# gives Cargo.toml's version; that its ad-hoc signature verifies strictly; that its document
# types claim every extension the viewer decodes, each as an Alternate handler; that each slice
# this Mac runs, the Intel one under Rosetta on Apple silicon, prints its version and a working
# sandbox with -v; and that `open -a` starts it: with an image, a window of the viewer's comes,
# from each such slice, and without one the viewer ends having shown none. Which image the
# window shows, how long the viewer waits for files and the status it ends with are the window
# scenario's and the app's tests' to check. Usage: ci/app.sh
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = Darwin ] || { echo "ci/app.sh: runs on macOS" >&2; exit 2; }
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
work=$(mktemp -d)
app=$work/nsxiv-ng.app
# the bundle's viewers, its registration with LaunchServices and the folder go on every way out
trap 'pkill -f "/${work##*/}/nsxiv-ng.app/" || true; "$lsregister" -u "$app" 2>/dev/null || true; rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM
fail() {
	echo "app: $1" >&2
	exit 1
}

NSXIV_NG_UNIVERSAL=1 dist/macos/app.sh "$work" >/dev/null
plist=$app/Contents/Info.plist
plutil -lint "$plist"
codesign --verify --strict --verbose=2 "$app"
codesign --display --verbose=2 "$app" 2>&1 | grep -qx 'Signature=adhoc' || fail "the signature is not ad-hoc"
value() { plutil -extract "$1" raw -o - "$plist"; }
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ "$(value CFBundleShortVersionString)" = "$version" ] && [ "$(value CFBundleVersion)" = "$version" ] ||
	fail "the bundle's version is $(value CFBundleShortVersionString), not $version"
binary=$app/Contents/MacOS/nsxiv-ng
slices=$(lipo -archs "$binary")
for slice in arm64 x86_64; do
	echo " $slices " | grep -q " $slice " || fail "the binary has no $slice slice, only $slices"
	minos=$(vtool -arch "$slice" -show-build "$binary" | awk '$1 == "minos" { print $2 }')
	[ "$minos" = "$(value LSMinimumSystemVersion)" ] ||
		fail "its $slice slice is built for macOS $minos, the bundle says $(value LSMinimumSystemVersion)"
done
[ "$(value NSHighResolutionCapable)" = true ] || fail "the bundle is not high-resolution capable"
echo "app: $(value CFBundleIdentifier) $version, $slices, macOS $minos and later, signed ad hoc"

# the slices this Mac runs: its own, and Intel's under Rosetta on Apple silicon
runs=
for slice in arm64 x86_64; do
	if arch "-$slice" /usr/bin/true 2>/dev/null; then
		runs="$runs $slice"
	elif [ "$slice" = x86_64 ]; then
		fail "this Mac cannot run the Intel slice: softwareupdate --install-rosetta --agree-to-license"
	fi
done
for slice in $runs; do
	said=$(arch "-$slice" "$binary" -v)
	echo "$said" | head -n 1 | grep -qx "nsxiv-ng $version" ||
		fail "the $slice slice's -v says '$(echo "$said" | head -n 1)'"
	echo "$said" | grep -q '^sandbox: seatbelt' ||
		fail "the $slice slice's sandbox: $(echo "$said" | grep sandbox)"
	echo "app: the $slice slice: $(echo "$said" | tr '\n' ' ')"
done

# the extensions the decoders take, as the viewer lists them
extensions=$(sed -n '/^pub const EXTENSIONS/,/^];/p' crates/decode/src/lib.rs | grep -o '"[a-z0-9]*"' | tr -d '"')
# each extension's type, as the system knows it, among the bundle's document types, or the
# extension itself where the system has no type for it; every type Alternate
osascript -l JavaScript - "$plist" $extensions <<'JS' || fail "the document types do not claim every extension"
ObjC.import('CoreServices');
function run(argv) {
  var plist = ObjC.deepUnwrap($.NSDictionary.dictionaryWithContentsOfFile(argv[0]));
  var types = plist.CFBundleDocumentTypes, claimed = {}, missing = [];
  types.forEach(function (t) {
    if (t.LSHandlerRank !== 'Alternate') throw new Error(t.CFBundleTypeName + ' is ' + t.LSHandlerRank);
    (t.LSItemContentTypes || []).concat(t.CFBundleTypeExtensions || []).forEach(function (c) { claimed[c] = true; });
  });
  var lines = argv.slice(1).map(function (e) {
    var uti = ObjC.unwrap(ObjC.castRefToObject(
      $.UTTypeCreatePreferredIdentifierForTag($.kUTTagClassFilenameExtension, $(e), null)));
    var by = uti.indexOf('dyn.') === 0 ? e : uti;
    if (!claimed[by]) missing.push(e + ' (' + by + ')');
    return 'app: .' + e + ' as ' + by;
  });
  if (missing.length) throw new Error('unclaimed: ' + missing.join(', '));
  return lines.join('\n');
}
JS

# the viewer's process of this bundle, once it runs, by the folder's name, as the process
# names the folder by the path the system resolves, /private/var for /var
viewer() { pgrep -f "/${work##*/}/nsxiv-ng.app/Contents/MacOS/nsxiv-ng" || true; }
# the number of windows process $1 shows on screen
windows() {
	osascript -l JavaScript - "$1" <<'JS'
ObjC.import('CoreGraphics');
function run(argv) {
  var list = ObjC.deepUnwrap(ObjC.castRefToObject(
    $.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)));
  return list.filter(function (w) {
    return w.kCGWindowOwnerPID == argv[0] && w.kCGWindowLayer == 0;
  }).length;
}
JS
}
cp testdata/fixtures/gradient.png "$work/gradient.png"
for slice in $runs; do
	open -a "$app" --arch "$slice" "$work/gradient.png"
	ran= shown=
	for _ in $(seq 150); do
		pid=$(viewer)
		[ -z "$pid" ] || ran=1
		[ -n "$pid" ] && [ "$(windows "$pid")" -ge 1 ] && shown=1 && break
		sleep 0.1
	done
	[ -n "$ran" ] || fail "open -a with an image started no viewer from the $slice slice"
	[ -n "$shown" ] || fail "open -a with an image showed no window from the $slice slice in 15 seconds"
	echo "app: open -a with an image showed the viewer's window, from the $slice slice"
	kill "$pid"
	for _ in $(seq 50); do
		kill -0 "$pid" 2>/dev/null || break
		sleep 0.1
	done
done

open -a "$app"
for _ in $(seq 50); do
	pid=$(viewer)
	[ -n "$pid" ] && break
	sleep 0.1
done
[ -n "$pid" ] || fail "open -a alone started no viewer"
shown=0
for _ in $(seq 50); do
	kill -0 "$pid" 2>/dev/null || break
	[ "$(windows "$pid")" -eq 0 ] || shown=1
	sleep 0.1
done
! kill -0 "$pid" 2>/dev/null || fail "open -a alone left a viewer running after 5 seconds"
pid=
[ "$shown" -eq 0 ] || fail "open -a alone showed a window"
echo "app: open -a alone ended the viewer, which showed no window"
