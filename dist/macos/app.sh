#!/bin/sh
# Makes nsxiv-ng.app from a release build of the viewer: the same binary the command line runs,
# in Contents/MacOS, the Info.plist from dist/macos/Info.plist, and an ad-hoc signature, which
# Gatekeeper does not take for a known developer's: a downloaded copy is quarantined until
# `xattr -dr com.apple.quarantine nsxiv-ng.app` clears it. No icon. Usage:
#   [NSXIV_NG_UNIVERSAL=1] dist/macos/app.sh [DIR]
# makes DIR/nsxiv-ng.app, by default in the build's release folder, and prints its path. The
# binary is for this Mac's own architecture; with NSXIV_NG_UNIVERSAL=1 it is universal, built for
# Apple silicon and for Intel and joined with lipo, in the release folder of
# universal-apple-darwin.
set -eu
cd "$(dirname "$0")/../.."

identifier=${NSXIV_NG_BUNDLE_ID:-io.github.hawkff.nsxiv-ng}
# the oldest macOS the viewer runs on, which the binary is built for
minimum=13.0
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || { echo "app.sh: no version in Cargo.toml" >&2; exit 1; }

target=${CARGO_TARGET_DIR:-target}
if [ "${NSXIV_NG_UNIVERSAL:-}" = 1 ]; then
	for triple in aarch64-apple-darwin x86_64-apple-darwin; do
		# rustup's toolchain has the host's standard library alone until asked for another
		! command -v rustup >/dev/null || rustup target add "$triple" >/dev/null
		MACOSX_DEPLOYMENT_TARGET=$minimum cargo build --release --locked -p nsxiv-ng --target "$triple"
	done
	release=$target/universal-apple-darwin/release
	mkdir -p "$release"
	lipo -create -output "$release/nsxiv-ng" "$target/aarch64-apple-darwin/release/nsxiv-ng" \
		"$target/x86_64-apple-darwin/release/nsxiv-ng"
else
	MACOSX_DEPLOYMENT_TARGET=$minimum cargo build --release --locked -p nsxiv-ng
	# the build's folder, under the target's triple where one is set
	release=$target/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}release
fi
out=${1:-$release}
app=$out/nsxiv-ng.app

rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$release/nsxiv-ng" "$app/Contents/MacOS/nsxiv-ng"
sed -e "s/@IDENTIFIER@/$identifier/" -e "s/@VERSION@/$version/" -e "s/@MINIMUM@/$minimum/" \
	dist/macos/Info.plist >"$app/Contents/Info.plist"
plutil -lint -s "$app/Contents/Info.plist"
codesign --sign - --force "$app"
codesign --verify --strict "$app"
echo "$app"
