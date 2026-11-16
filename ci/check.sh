#!/bin/sh
# Checks the workspace on each Rust toolchain named (stable, beta and nightly
# by default): formatting, clippy with warnings as errors, and the tests. Then
# cargo-deny checks licenses, advisories and sources once. A nightly failure
# prints a warning and leaves the result to stable and beta.
# Needs rustup; installs missing toolchains and cargo-deny, and on Debian and Arch the libheif
# decoders that the HEIC and AVIF tests load, the fonts the text tests draw with (DejaVu, and
# Noto Color Emoji for annotations' emoji; macOS has Apple Color Emoji), and
# libxkbcommon, which the window systems' keys are named through; on Arch a C compiler, git,
# which cargo-deny fetches its advisories with, and rustup too, where they are missing, and an
# install there is a full upgrade, the only kind pacman supports. On macOS it installs DejaVu
# through Homebrew.
set -eu
cd "$(dirname "$0")/.."
[ $# -gt 0 ] || set -- stable beta nightly

packages="libheif1 libheif-plugin-libde265 libheif-plugin-dav1d fonts-dejavu-core fonts-noto-color-emoji libxkbcommon-dev pkg-config"
if command -v dpkg >/dev/null && ! dpkg -s $packages >/dev/null 2>&1; then
	if [ "$(id -u)" -eq 0 ]; then
		apt-get update -qq >/dev/null
		apt-get install -y -qq --no-install-recommends $packages >/dev/null
	else
		echo "warning: the build and the HEIC, AVIF and text tests need $packages" >&2
	fi
fi

if command -v pacman >/dev/null; then
	packages="gcc git pkgconf libxkbcommon libheif ttf-dejavu noto-fonts-emoji"
	command -v rustup >/dev/null || packages="$packages rustup"
	missing=
	for package in $packages; do
		pacman -Q "$package" >/dev/null 2>&1 || missing="$missing $package"
	done
	if [ -n "$missing" ]; then
		if [ "$(id -u)" -eq 0 ]; then
			pacman -Syu --noconfirm --needed --noprogressbar $missing >/dev/null
		else
			echo "warning: the build and the HEIC, AVIF and text tests need$missing" >&2
		fi
	fi
fi

# DejaVu on macOS: Homebrew's cask puts the faces in ~/Library/Fonts, which fontdb scans
if [ "$(uname -s)" = Darwin ] && [ ! -e "$HOME/Library/Fonts/DejaVuSansMono.ttf" ] &&
	[ ! -e /Library/Fonts/DejaVuSansMono.ttf ]; then
	brew=$(command -v brew || echo /opt/homebrew/bin/brew)
	if [ -x "$brew" ]; then
		"$brew" install --cask --quiet font-dejavu
	else
		echo "warning: the text tests need DejaVu: brew install --cask font-dejavu" >&2
	fi
fi

check() {
	rustup toolchain install "$1" --profile minimal --component rustfmt,clippy --no-self-update &&
		cargo "+$1" fmt --all --check &&
		cargo "+$1" clippy --workspace --all-targets --locked -- -D warnings &&
		cargo "+$1" test --workspace --locked
}

for toolchain; do
	echo "== $toolchain"
	if [ "$toolchain" = nightly ]; then
		check nightly || echo "warning: nightly failed; stable and beta decide the result" >&2
	else
		check "$toolchain"
	fi
done

echo "== cargo-deny"
rustup toolchain install stable --profile minimal --no-self-update
command -v cargo-deny >/dev/null || cargo +stable install --locked cargo-deny
cargo +stable deny --locked check
