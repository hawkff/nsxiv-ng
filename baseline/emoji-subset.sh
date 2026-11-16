#!/bin/sh
# Makes testdata/fixtures/stamp-emoji.ttf, the color emoji the edit tests draw the stamps in on
# Linux: the 21 stamps of crates/edit/src/annotation.rs's EMOJIS, cut with fonttools'
# pyftsubset from Noto Color Emoji 2.051 as Debian 13's fonts-noto-color-emoji
# 2.051-0+deb13u1 installs it, the font the stamps' golden frames were drawn with. Its names
# say "nsxiv-ng stamp emoji": "Noto" is Google's trademark. Under the SIL Open Font License
# 1.1, whose text goes beside it. Run on Debian 13, as root, where it installs what it needs:
#   sh baseline/emoji-subset.sh
set -eu
out=$(dirname "$0")/../testdata/fixtures
apt-get update -qq >/dev/null
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
	fonts-noto-color-emoji=2.051-0+deb13u1 fonttools >/dev/null
source=/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf
cut=$(mktemp --suffix=.ttf)
# every code point of the stamps, the variation selector and the joiner of the pirate flag
# among them, with the layout that joins it
pyftsubset "$source" --output-file="$cut" --layout-features='*' --no-recalc-timestamp \
	--text='✅❌⚠️❗⭐🔥👍👎💡🔒🚫📌🎯🐛💯🤔😂🎉❤️🏴‍☠️🍆'
python3 - "$cut" "$out/stamp-emoji.ttf" <<'PY'
import sys
from fontTools.ttLib import TTFont
font = TTFont(sys.argv[1], recalcTimestamp=False)
names = {1: "nsxiv-ng stamp emoji", 3: "nsxiv-ng stamp emoji 2.051", 4: "nsxiv-ng stamp emoji",
         6: "nsxiv-ng-stamp-emoji", 16: "nsxiv-ng stamp emoji"}
for record in font["name"].names:
    if record.nameID in names:
        record.string = names[record.nameID]
font.save(sys.argv[2])
PY
rm "$cut"
# the font's copyright, then the license's text, from the font's Debian copyright file
{
	echo "stamp-emoji.ttf: a subset of Noto Color Emoji 2.051, renamed."
	echo "Copyright 2013-2022 Google Inc., Arjen Nienhuis <a.g.nienhuis@gmail.com>."
	echo "Licensed under the SIL Open Font License, Version 1.1, which follows."
	echo
	sed -n '/^ SIL OPEN FONT LICENSE Version 1.1/,$p' /usr/share/doc/fonts-noto-color-emoji/copyright |
		sed 's/^ //; s/^\.$//'
} >"$out/stamp-emoji-OFL.txt"
ls -l "$out/stamp-emoji.ttf" "$out/stamp-emoji-OFL.txt"
