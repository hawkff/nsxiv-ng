#!/bin/sh
# Runs the smoke scripts: builds the viewer, then runs every script in smoke/ that runs on
# this distribution with it, once for each mode the script names there. Usage:
#   ci/smoke.sh [debug|release]
# The viewer is the debug build, or with `release` the release build, which a user runs. A
# script says what it needs in comment lines:
#   # runs on: the distributions it runs on, by the ID of /etc/os-release, as debian or arch;
#     debian where it names none
#   # packages: the Debian packages it needs
#   # arch packages: the Arch packages it needs
#   # modes: the modes to run it in, each passed as its second argument, where it has any
#   # arch modes: the modes to run it in on Arch, where they differ
#   # guest: "only" to run it in a guest and nowhere else, "also" to run it as the others,
#     then once more, with the same arguments, in a guest: the distribution's own kernel under
#     KVM, with this system as its root, read-only over 9p, for what a container's kernel
#     cannot give, as Landlock
# A script that does not run here is skipped with a line that says so, as is a run in a guest
# where /dev/kvm cannot be opened, and a run in which no script runs fails. Where cargo is, builds
# the viewer in that profile with rustup's stable toolchain and notes a hash of the sources beside
# that profile's binary; else takes the one built before, as on a distribution that runs the
# scripts with a viewer built elsewhere, and fails unless it was built from these sources.
# Installs the packages named that are missing, and what the viewer links, when run as root on
# Debian or Arch; on Arch an install is a full upgrade, the only kind pacman supports.
# smoke/stacking.sh runs Wayfire, which refuses root, as the user nobody. smoke/hyprland.sh runs
# Hyprland in a guest alone, as it needs a display device from the kernel (vkms), which only a
# guest's kernel gives.
set -eu
cd "$(dirname "$0")/.."

profile=${1:-debug}
case $profile in
debug) release= ;;
release) release=yes ;;
*)
	echo "usage: ci/smoke.sh [debug|release]" >&2
	exit 2
	;;
esac

distro=$(. /etc/os-release && echo "$ID")
# the header line `# $1:` of script $2, or `# <distro> $1:` where the script has one; packages
# of no distribution named are Debian's
field() {
	own=$(sed -n "s/^# $distro $1: //p" "$2")
	if [ -n "$own" ]; then
		echo "$own"
	elif [ "$1" != packages ] || [ "$distro" = debian ]; then
		sed -n "s/^# $1: //p" "$2"
	fi
}
# the distributions script $1 runs on
runs_on() {
	sed -n 's/^# runs on: //p' "$1" | grep . || echo debian
}
runs_here() {
	for place in $(runs_on "$1"); do
		[ "$place" = "$distro" ] && return 0
	done
	return 1
}

# a guest runs only under KVM: emulated, it boots and runs some four times slower, and the
# scripts' timings would fail for that
# the device must be there before it is opened: <> would make a missing one, as a plain file
kvm=no
if [ -c /dev/kvm ] && (: 3<>/dev/kvm) 2>/dev/null; then
	kvm=yes
fi
needed=
for script in smoke/*.sh; do
	runs_here "$script" || continue
	needed="$needed $(field packages "$script")"
	if [ -n "$(field guest "$script")" ] && [ "$kvm" = yes ]; then
		case $distro in
		debian)
			# the guest's kernel and its emulator for the machine's own architecture
			case $(dpkg --print-architecture) in
			arm64) guest_packages="qemu-system-arm linux-image-arm64" ;;
			*) guest_packages="qemu-system-x86 linux-image-amd64" ;;
			esac
			needed="$needed virtme-ng $guest_packages busybox-static"
			;;
		arch) needed="$needed virtme-ng qemu-base linux busybox" ;;
		esac
	fi
done
case $distro in
debian)
	packages=$(echo "libxkbcommon-dev pkg-config $needed" | tr ' ' '\n' | sort -u | tr '\n' ' ')
	if ! dpkg -s $packages >/dev/null 2>&1; then
		if [ "$(id -u)" -eq 0 ]; then
			apt-get update -qq >/dev/null
			DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $packages >/dev/null
		else
			echo "warning: the smoke scripts need $packages" >&2
		fi
	fi
	;;
arch)
	packages=$(echo "libxkbcommon ttf-dejavu $needed" | tr ' ' '\n' | sort -u | tr '\n' ' ')
	missing=
	for package in $packages; do
		pacman -Q "$package" >/dev/null 2>&1 || missing="$missing $package"
	done
	if [ -n "$missing" ]; then
		if [ "$(id -u)" -eq 0 ]; then
			pacman -Syu --noconfirm --needed --noprogressbar $missing >/dev/null
		else
			echo "warning: the smoke scripts need$missing" >&2
		fi
	fi
	;;
esac

bin=${CARGO_TARGET_DIR:-$PWD/target}/$profile/nsxiv-ng
# what the viewer is built from
sources=$(find Cargo.toml Cargo.lock crates -type f | LC_ALL=C sort | xargs cat | sha256sum | cut -d' ' -f1)
if command -v cargo >/dev/null; then
	rustup toolchain install stable --profile minimal --no-self-update
	cargo +stable build --locked -p nsxiv-ng ${release:+--release}
	echo "$sources" >"$bin.sources"
elif [ ! -x "$bin" ]; then
	echo "no cargo here and no viewer built before at $bin; build it first, where cargo is" >&2
	exit 1
elif [ "$(cat "$bin.sources" 2>/dev/null)" != "$sources" ]; then
	echo "the viewer at $bin was not built from these sources; build it first, where cargo is" >&2
	exit 1
fi
# under /tmp whatever TMPDIR says: virtme-ng 1.35 joins the guest's command with spaces, so the
# runner's path can hold none
work=$(mktemp -d /tmp/smoke.XXXXXX)
trap 'rm -rf "$work"' EXIT
# A modprobe that leaves module options off the insmod lines it shows: on a host whose kernel
# command line carries options for a module, modprobe appends them there, and virtme-ng 1.35,
# which builds the guest's initramfs from those lines, reads them as part of the module's path.
# shortcut: needed with virtme-ng 1.35 and 1.42; drop it once virtme-ng takes the options apart
mkdir "$work/bin"
cat >"$work/bin/modprobe" <<EOF
#!/bin/sh
$(PATH=/usr/sbin:/sbin:$PATH command -v modprobe || echo modprobe) "\$@" |
	sed -E 's/^(insmod [^ ]+) .*/\\1/'
EOF
chmod +x "$work/bin/modprobe"
# at least two of the cores the container has, and memory for the decodes of the scripts'
cpus=$(nproc)
[ "$cpus" -ge 2 ] || cpus=2
# $1 as one word of a shell command, whatever it holds
quoted() {
	printf "'%s'" "$(printf %s "$1" | sed "s/'/'\\\\''/g")"
}
# runs script $1 with the viewer and the mode $2 in a guest, which gives its status; a guest
# that does not come up fails too
in_guest() {
	kernel=$(ls /boot/vmlinuz-* | sort -V | tail -1)
	# a runner that could not be written would run as an empty one, and pass
	printf 'cd %s && exec sh %s %s %s\n' "$(quoted "$PWD")" "$(quoted "$1")" "$(quoted "$bin")" \
		"${2:+$(quoted "$2")}" >"$work/run" || return 1
	PATH="$work/bin:$PATH" vng --run "$kernel" --force-9p --disable-microvm --cpus "$cpus" \
		--memory 4G -- sh "$work/run"
}
status=0
ran=0
for script in smoke/*.sh; do
	name=$(basename "$script" .sh)
	if ! runs_here "$script"; then
		echo "== $name: skipped, runs on $(runs_on "$script") and not on $distro"
		continue
	fi
	modes=$(field modes "$script")
	guest=$(field guest "$script")
	for place in here guest; do
		case $place-$guest in
		here-only | guest-) continue ;;
		guest-*)
			if [ "$kvm" = no ]; then
				echo "== $name (guest): skipped, no /dev/kvm"
				continue
			fi
			;;
		esac
		ran=$((ran + 1))
		for mode in ${modes:--}; do
			[ "$mode" = - ] && mode=
			label="$name${mode:+ $mode}"
			if [ "$place" = here ]; then
				echo "== $label"
				sh "$script" "$bin" ${mode:+"$mode"} || status=1
			else
				echo "== $label (guest)"
				in_guest "$script" "$mode" || status=1
			fi
		done
	done
done
if [ "$ran" -eq 0 ]; then
	echo "no smoke script runs on $distro" >&2
	exit 1
fi
exit "$status"
