#!/bin/sh
# Generates the fixture images in testdata/fixtures with ImageMagick 7, and the
# tools each section names. The outputs are committed; rerun this only to
# change the set, which leaves the golden frames, drawn over these bytes, behind.
# testdata/fixtures/stamp-emoji.ttf is no output of this script: baseline/emoji-subset.sh cuts
# it from Noto Color Emoji 2.051, under the SIL Open Font License 1.1, whose text is beside it
# in stamp-emoji-OFL.txt. The edit tests draw stamps in it; the viewer never loads it.
set -eu
out=$(dirname "$0")/../testdata/fixtures
mkdir -p "$out"
# -strip leaves out the PNG time chunks, so a rerun writes the same bytes
# opaque and smooth for scaling: red at the top, blue at the bottom
magick -size 320x200 gradient:red-blue -strip "$out/gradient.png"
# transparent outside a green disk, for the checkerboard
magick -size 160x160 xc:none -fill green -draw 'circle 80,80 80,10' -strip "$out/alpha.png"
# black and white pixels alternating, where filtered and nearest scaling differ most
magick -size 320x240 pattern:gray50 -strip "$out/checker.png"
# photo-like detail, large enough that the canvas and thumbnails scale it down
magick -seed 7 -size 640x480 plasma:fractal -quality 85 "$out/photo.jpg"
# a square crossing a gradient, stored as partial frames with offsets
magick -size 96x64 gradient:white-gray \
	\( -clone 0 -fill red -draw 'rectangle 8,16 39,47' \) \
	\( -clone 0 -fill red -draw 'rectangle 32,16 63,47' \) \
	\( -clone 0 -fill red -draw 'rectangle 56,16 87,47' \) \
	-delete 0 -set delay 20 -loop 0 -layers Optimize "$out/moving.gif"
# the gradient again, small, as JPEG 2000 in its JP2 box and as a bare codestream
magick -size 64x40 gradient:red-blue -alpha off -quality 50 "$out/gradient.jp2"
magick -size 64x40 gradient:red-blue -alpha off -quality 50 "$out/gradient.j2k"
# JPEG XL with libjxl's cjxl: the gradient lossy, which decodes from XYB, the disk lossless
# with its alpha, and a gray ramp lossless
cjxl --quiet -d 1 "$out/gradient.png" "$out/gradient.jxl"
cjxl --quiet -d 0 "$out/alpha.png" "$out/alpha.jxl"
gray=$(mktemp --suffix=.png)
magick -size 32x8 gradient:black-white -colorspace Gray -depth 8 -strip "$gray"
cjxl --quiet -d 0 "$gray" "$out/gray.jxl"
# the ramp again as gray PQ, which jxl-rs cannot tone map into a profile, and a muted red in
# Display P3, which sRGB shows more saturated
cjxl --quiet -d 0 -x color_space=Gra_D65_Rel_PeQ "$gray" "$out/gray-pq.jxl"
rm "$gray"
p3=$(mktemp --suffix=.png)
magick -size 8x8 'xc:rgb(200,100,100)' -depth 8 -strip "$p3"
cjxl --quiet -d 0 -x color_space=RGB_D65_DCI_Rel_SRG "$p3" "$out/p3.jxl"
rm "$p3"
# HEIC and AVIF with libheif's heif-enc: the gradient in each, the HEIC with a thumbnail of
# 64 pixels, and the disk with its alpha
heif-enc -q 70 -t 64 -o "$out/gradient.heic" "$out/gradient.png"
heif-enc -A -q 70 -o "$out/gradient.avif" "$out/gradient.png"
heif-enc -q 70 -o "$out/alpha.heic" "$out/alpha.png"
# the muted red in Display P3 again, lossless: as nclx code points in AVIF, and in HEIC as the
# ICC profile djxl writes
p3=$(mktemp --suffix=.png)
magick -size 8x8 'xc:rgb(200,100,100)' -depth 8 -strip "$p3"
heif-enc -A -L --colour_primaries 12 --transfer_characteristic 13 -o "$out/p3.avif" "$p3"
djxl --quiet "$out/p3.jxl" "$p3"
heif-enc -L -o "$out/p3.heic" "$p3"
rm "$p3"
# JPEG stored as YCbCr, which decodes straight into RGBA, as cameras and editors write it:
# chroma at full, half and quarter resolution, progressive, with restart markers through
# libjpeg-turbo's jpegtran, and with the ICC profile djxl writes for the muted red; and gray,
# which decodes as before. An odd size leaves partial blocks at the edges.
jpeg=$(mktemp --suffix=.png)
magick -seed 3 -size 37x23 plasma:fractal -strip "$jpeg"
for sampling in 444 422 420; do
	magick "$jpeg" -sampling-factor "$(echo $sampling | sed 's/./&:/g; s/:$//')" -quality 90 \
		"$out/jpeg-$sampling.jpg"
done
magick "$jpeg" -sampling-factor 4:2:0 -interlace JPEG -quality 90 "$out/jpeg-progressive.jpg"
jpegtran -restart 1 -outfile "$out/jpeg-restart.jpg" "$out/jpeg-420.jpg"
magick "$jpeg" -colorspace Gray -quality 90 "$out/jpeg-gray.jpg"
# and what the image crate decodes as before: stored as RGB and as CMYK, and arithmetic coding,
# which zune-jpeg refuses
magick "$jpeg" ppm:- | cjpeg -rgb -quality 90 >"$out/jpeg-rgb.jpg"
magick "$jpeg" -colorspace CMYK -quality 90 "$out/jpeg-cmyk.jpg"
magick "$jpeg" ppm:- | cjpeg -arithmetic -quality 90 >"$out/jpeg-arithmetic.jpg"
djxl --quiet "$out/p3.jxl" "$jpeg"
magick "$jpeg" -quality 90 "$out/jpeg-p3.jpg"
rm "$jpeg"
# EXIF orientations 1 to 8 with exiftool: each file stores the upright image (red, green over
# blue, white) turned back by its tag, so every one shows upright once its tag applies
upright=$(mktemp --suffix=.png)
magick -size 16x8 xc:red xc:lime +append \( -size 16x8 xc:blue xc:white +append \) -append "$upright"
for turn in '1' '2 -flop' '3 -rotate 180' '4 -flip' '5 -transpose' '6 -rotate 270' \
	'7 -transverse' '8 -rotate 90'; do
	set -- $turn
	n=$1
	shift
	magick "$upright" "$@" -strip -sampling-factor 1x1 -quality 90 "$out/orientation-$n.jpg"
	exiftool -q -overwrite_original "-Orientation#=$n" "$out/orientation-$n.jpg"
done
rm "$upright"
# Exact pixels from python3, as PNG writes them: color hidden under transparent pixels, and
# the frames of a 4x1 animation in each format, whose disposal and blend each change what a
# later frame shows. The APNG is written here whole, as no tool sets its dispose and blend
# ops; gifsicle and webpmux assemble the others.
frames=$(mktemp -d)
python3 - "$out" "$frames" <<'EOF'
import struct, sys, zlib

out, frames = sys.argv[1:]
R, G, B, W = (255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255), (255, 255, 255, 255)
HALF, CLEAR = (255, 255, 255, 128), (0, 0, 0, 0)
SIGNATURE = b"\x89PNG\r\n\x1a\n"

def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

def header(width):
    return SIGNATURE + chunk(b"IHDR", struct.pack(">IIBBBBB", width, 1, 8, 6, 0, 0, 0))

def row(pixels):
    return zlib.compress(b"\0" + bytes(c for p in pixels for c in p), 9)

def png(path, pixels):
    data = header(len(pixels)) + chunk(b"IDAT", row(pixels)) + chunk(b"IEND", b"")
    open(path, "wb").write(data)

# green and blue at alpha 0, and white at half alpha
png(f"{out}/hidden.png", [R, (0, 255, 0, 0), (0, 0, 255, 0), HALF])
# APNG: x, dispose (0 none, 1 background, 2 previous), blend (0 source, 1 over), pixels
apng = [(0, 0, 0, [R, R, R, R]), (0, 1, 1, [G, CLEAR]), (1, 2, 0, [B, CLEAR]), (3, 0, 1, [HALF])]
data = header(4) + chunk(b"acTL", struct.pack(">II", len(apng), 0))
sequence = 0
for x, dispose, blend, pixels in apng:
    control = struct.pack(">IIIIIHHBB", sequence, len(pixels), 1, x, 0, 1, 10, dispose, blend)
    data += chunk(b"fcTL", control)
    if sequence == 0:
        data += chunk(b"IDAT", row(pixels))
        sequence += 1
    else:
        data += chunk(b"fdAT", struct.pack(">I", sequence + 1) + row(pixels))
        sequence += 2
open(f"{out}/disposal.apng", "wb").write(data + chunk(b"IEND", b""))
# the GIF's and the WebP's frames, placed by the commands below
for name, pixels in [("red", [R, R, R, R]), ("green", [G, G]), ("blue", [B, CLEAR]),
                     ("white", [W]), ("green-clear", [G, CLEAR]),
                     ("half", [CLEAR, HALF, CLEAR, CLEAR])]:
    png(f"{frames}/{name}.png", pixels)
EOF
cwebp -quiet -lossless -exact "$out/hidden.png" -o "$out/hidden.webp"
# GIF, whose transparent pixels always show what lies under them
for name in red green blue white; do
	magick "$frames/$name.png" "$frames/$name.gif"
done
gifsicle --loopcount=forever --delay 10 --logical-screen 4x1 \
	--disposal none --position 0,0 "$frames/red.gif" \
	--disposal background --position 0,0 "$frames/green.gif" \
	--disposal previous --position 1,0 "$frames/blue.gif" \
	--disposal none --position 3,0 "$frames/white.gif" >"$out/disposal.gif"
# WebP, which has no previous disposal and keeps frame offsets even; +x+y+dispose and +b to
# blend or -b to replace, over an opaque white background color, which is only a hint
for name in red green-clear blue half; do
	cwebp -quiet -lossless -exact "$frames/$name.png" -o "$frames/$name.webp"
done
webpmux -frame "$frames/red.webp" +100+0+0+0-b -frame "$frames/green-clear.webp" +100+0+0+0+b \
	-frame "$frames/blue.webp" +100+2+0+1-b -frame "$frames/half.webp" +100+0+0+0+b \
	-loop 0 -bgcolor 255,255,255,255 -o "$out/disposal.webp"
rm -r "$frames"
# Camera RAW, synthetic: each container's layout around tiny JPEGs, so the preview search is
# tested without tens of megabytes of real files: TIFF kinds with the preview in a SubIFD
# (DNG), in the first directory and the next (CR2, two previews), behind a MakerNote (Nikon,
# Sony, Pentax, Epson, Olympus in ORF, and the other headers of OM System, Olympus and Pentax),
# RW2's JpgFromRaw, CR3's boxes, one of 64-bit size, RAF's header, and none.
raw=$(mktemp -d)
magick -size 64x48 gradient:red-blue -quality 80 "$raw/large.jpg"
magick -size 48x32 gradient:green-yellow -quality 80 "$raw/medium.jpg"
magick -size 24x16 xc:purple -quality 80 "$raw/tiny.jpg"
magick -size 16x12 xc:green -quality 80 "$raw/small.jpg"
# RAF's JPEG carries its own orientation, which the RAF has none of
cp "$raw/large.jpg" "$raw/oriented.jpg"
exiftool -q -overwrite_original "-Orientation#=6" "$raw/oriented.jpg"
python3 - "$out" "$raw" <<'EOF'
import struct, sys

out, raw = sys.argv[1:]
jpeg = {name: open(f"{raw}/{name}.jpg", "rb").read()
        for name in ["large", "medium", "tiny", "small", "oriented"]}
# the head of a lossless JPEG, as raw data is stored, which is no preview
jpeg["lossless"] = bytes.fromhex("ffd8ffc3000b08001000100101110000ffd9")
jpeg["data"] = b"\0\0\0\0"
SHORT, LONG, UNDEFINED, IFD = 3, 4, 7, 13

def tiff(order, ifds, root, head=None, magic=42, blobs=()):
    """A TIFF structure: `ifds` maps names to (entries, next, prefix), each entry (tag, type,
    value); values are numbers, bytes, ("ifd", name), ("ifds", names), ("at", blob),
    ("len", blob), ("blob", blob) for a blob as an UNDEFINED value, or ("note", name) for a
    MakerNote that is a directory of this structure after its prefix. Offsets count from the
    structure's start."""
    e = "<" if order == b"II" else ">"
    head = head if head is not None else order + struct.pack(e + "HI", magic, 0)
    def outside(value):
        if isinstance(value, bytes) and len(value) > 4:
            return len(value)
        if isinstance(value, tuple) and value[0] == "ifds" and len(value[1]) > 1:
            return 4 * len(value[1])
        return 0
    at, place = len(head), {}
    for name, (entries, _, prefix) in ifds.items():
        at += len(prefix)
        place[name] = at
        at += 2 + 12 * len(entries) + 4 + sum(outside(v) for _, _, v in entries)
        at += at % 2
    where = {}
    for blob in blobs:
        where[blob] = at
        at += len(jpeg[blob])
    data = bytearray(at)
    data[:len(head)] = head
    if head[:2] in (b"II", b"MM") and len(head) >= 8 and head[2:4] != b"\x03\x00":
        struct.pack_into(e + "I", data, 4, place[root])
    for name, (entries, next_name, prefix) in ifds.items():
        at = place[name]
        data[at - len(prefix):at] = prefix
        struct.pack_into(e + "H", data, at, len(entries))
        extra = at + 2 + 12 * len(entries) + 4
        for i, (tag, kind, value) in enumerate(sorted(entries, key=lambda x: x[0])):
            row = at + 2 + 12 * i
            count, field = 1, b""
            if isinstance(value, int):
                field = struct.pack(e + ("H" if kind == SHORT else "I"), value)
            elif isinstance(value, bytes):
                count = len(value)
                if count > 4:
                    data[extra:extra + count] = value
                    field, extra = struct.pack(e + "I", extra), extra + count
                else:
                    field = value
            elif value[0] == "ifd":
                field = struct.pack(e + "I", place[value[1]])
            elif value[0] == "ifds":
                count = len(value[1])
                offsets = b"".join(struct.pack(e + "I", place[n]) for n in value[1])
                if count > 1:
                    data[extra:extra + len(offsets)] = offsets
                    field, extra = struct.pack(e + "I", extra), extra + len(offsets)
                else:
                    field = offsets
            elif value[0] == "at":
                field = struct.pack(e + "I", where[value[1]])
            elif value[0] == "len":
                field = struct.pack(e + "I", len(jpeg[value[1]]))
            elif value[0] == "blob":
                count, field = len(jpeg[value[1]]), struct.pack(e + "I", where[value[1]])
            elif value[0] == "note":
                prefix = ifds[value[1]][2]
                count = len(prefix) + 64
                field = struct.pack(e + "I", place[value[1]] - len(prefix))
            data[row:row + 12] = struct.pack(e + "HHI", tag, kind, count) + field.ljust(4, b"\0")
        nxt = place[next_name] if next_name else 0
        struct.pack_into(e + "I", data, at + 2 + 12 * len(entries), nxt)
    for blob in blobs:
        data[where[blob]:where[blob] + len(jpeg[blob])] = jpeg[blob]
    return bytes(data)

def write(name, data):
    open(f"{out}/{name}", "wb").write(data)

# DNG: a thumbnail in the first directory, and in SubIFDs the raw data, a lossless JPEG,
# beside the preview
write("raw-subifd.dng", tiff(b"II", {
    "ifd0": ([(0x00fe, LONG, 1), (0x0112, SHORT, 6), (0x0201, LONG, ("at", "small")),
              (0x0202, LONG, ("len", "small")), (0x014a, LONG, ("ifds", ["raw", "preview"]))],
             None, b""),
    "raw": ([(0x00fe, LONG, 0), (0x0103, SHORT, 7), (0x0111, LONG, ("at", "lossless")),
             (0x0117, LONG, ("len", "lossless"))], None, b""),
    "preview": ([(0x00fe, LONG, 1), (0x0103, SHORT, 7), (0x0111, LONG, ("at", "large")),
                 (0x0117, LONG, ("len", "large"))], None, b""),
}, "ifd0", blobs=["small", "lossless", "large"]))
# CR2: its mark after the header, a strip of JPEG in the first directory and a smaller one
# of the same shape in the next
write("raw-two.cr2", tiff(b"II", {
    "ifd0": ([(0x0103, SHORT, 6), (0x0111, LONG, ("at", "medium")),
              (0x0117, LONG, ("len", "medium")), (0x0112, SHORT, 1)], "ifd1", b""),
    "ifd1": ([(0x0201, LONG, ("at", "tiny")), (0x0202, LONG, ("len", "tiny"))], None, b""),
}, "ifd0", head=b"II*\0\0\0\0\0CR\x02\0\0\0\0\0", blobs=["medium", "tiny"]))
# NEF: the preview in Nikon's MakerNote, a TIFF structure of its own
nikon = b"Nikon\0\x02\x10\0\0" + tiff(b"MM", {
    "main": ([(0x0011, IFD, ("ifd", "preview"))], None, b""),
    "preview": ([(0x0201, LONG, ("at", "large")), (0x0202, LONG, ("len", "large"))], None, b""),
}, "main", blobs=["large"])
jpeg["nikon"] = nikon
write("raw-nikon.nef", tiff(b"MM", {
    "ifd0": ([(0x0112, SHORT, 8), (0x0201, LONG, ("at", "small")),
              (0x0202, LONG, ("len", "small")), (0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("blob", "nikon"))], None, b""),
}, "ifd0", blobs=["small", "nikon"]))
# ARW: the preview whole as a value in Sony's MakerNote, offsets from the file's start
write("raw-sony.arw", tiff(b"II", {
    "ifd0": ([(0x0201, LONG, ("at", "small")), (0x0202, LONG, ("len", "small")),
              (0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("note", "sony"))], None, b""),
    "sony": ([(0x2001, UNDEFINED, ("blob", "large"))], None, b"SONY DSC \0\0\0"),
}, "ifd0", blobs=["small", "large"]))
# PEF: the preview's start and length in Pentax's MakerNote
write("raw-pentax.pef", tiff(b"MM", {
    "ifd0": ([(0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("note", "pentax"))], None, b""),
    "pentax": ([(0x0003, LONG, ("len", "large")), (0x0004, LONG, ("at", "large"))], None,
               b"AOC\0MM"),
}, "ifd0", blobs=["large"]))
# ORF: Olympus's MakerNote, offsets from its own start, the preview in its camera settings
olympus = tiff(b"II", {
    "main": ([(0x2020, IFD, ("ifd", "settings"))], None, b""),
    "settings": ([(0x0101, LONG, ("at", "large")), (0x0102, LONG, ("len", "large"))], None,
                 b""),
}, "main", head=b"OLYMPUS\0II\x03\0", blobs=["large"])
jpeg["olympus"] = olympus
write("raw.orf", tiff(b"II", {
    "ifd0": ([(0x0112, SHORT, 6), (0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("blob", "olympus"))], None, b""),
}, "ifd0", magic=0x4f52, blobs=["olympus"]))
# the other MakerNote headers: OM System's, offsets from its own start; Olympus's old one,
# offsets from the file's; and Pentax's own, with its own byte order inside a file of the other
om = tiff(b"II", {
    "main": ([(0x2020, IFD, ("ifd", "settings"))], None, b""),
    "settings": ([(0x0101, LONG, ("at", "large")), (0x0102, LONG, ("len", "large"))], None,
                 b""),
}, "main", head=b"OM SYSTEM\0\0\0II\x04\0", blobs=["large"])
jpeg["om"] = om
write("raw-om.orf", tiff(b"II", {
    "ifd0": ([(0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("blob", "om"))], None, b""),
}, "ifd0", magic=0x4f52, blobs=["om"]))
write("raw-olymp.orf", tiff(b"II", {
    "ifd0": ([(0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("note", "olymp"))], None, b""),
    "olymp": ([(0x2020, LONG, ("ifd", "settings"))], None, b"OLYMP\0\x01\0"),
    "settings": ([(0x0101, LONG, ("at", "large")), (0x0102, LONG, ("len", "large"))], None,
                 b""),
}, "ifd0", magic=0x4f52, blobs=["large"]))
pentax = tiff(b"II", {
    "main": ([(0x0003, LONG, ("len", "large")), (0x0004, LONG, ("at", "large"))], None, b""),
}, "main", head=b"PENTAX \0II", blobs=["large"])
jpeg["pentax"] = pentax
write("raw-pentax-own.pef", tiff(b"MM", {
    "ifd0": ([(0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("blob", "pentax"))], None, b""),
}, "ifd0", blobs=["pentax"]))
# RW2: Panasonic's mark, and the JPEG whole as JpgFromRaw
write("raw.rw2", tiff(b"II", {
    "ifd0": ([(0x0112, SHORT, 8), (0x002e, UNDEFINED, ("blob", "large"))], None, b""),
}, "ifd0", magic=0x55, blobs=["large"]))
# ERF: Epson's MakerNote, the preview whole as a value with its first byte EE, as the R-D1
# writes it
jpeg["epson"] = b"\xee" + jpeg["large"][1:]
write("raw-epson.erf", tiff(b"MM", {
    "ifd0": ([(0x8769, LONG, ("ifd", "exif"))], None, b""),
    "exif": ([(0x927c, UNDEFINED, ("note", "epson"))], None, b""),
    "epson": ([(0x0280, 1, ("blob", "epson"))], None, b"EPSON\0\x01\0"),
}, "ifd0", blobs=["epson"]))
# DNG with no JPEG, only uncompressed data
write("raw-none.dng", tiff(b"II", {
    "ifd0": ([(0x00fe, LONG, 0), (0x0103, SHORT, 1), (0x0111, LONG, ("at", "data")),
              (0x0117, LONG, ("len", "data"))], None, b""),
}, "ifd0", blobs=["data"]))

# CR3: ISO boxes; the orientation in CMT1's TIFF, a thumbnail in THMB, the preview in PRVW
def box(kind, payload):
    return struct.pack(">I", 8 + len(payload)) + kind + payload
canon = bytes.fromhex("85c0b687820f11e08111f4ce462b6a48")
preview = bytes.fromhex("eaf42b5e1c984b88b9fbb7dc406e4d16")
cmt1 = tiff(b"II", {"ifd0": ([(0x0112, SHORT, 6)], None, b"")}, "ifd0")
write("raw.cr3", box(b"ftyp", b"crx \0\0\0\x01crx isom")
      + box(b"moov", box(b"uuid", canon + box(b"CMT1", cmt1)
                         + box(b"THMB", bytes(16) + jpeg["small"])))
      + box(b"uuid", preview + bytes(8) + box(b"PRVW", bytes(16) + jpeg["medium"]))
      + box(b"mdat", b""))
# the same with the PRVW box's container in a box of 64-bit size
def large_box(kind, payload):
    return struct.pack(">I", 1) + kind + struct.pack(">Q", 16 + len(payload)) + payload
write("raw-large.cr3", box(b"ftyp", b"crx \0\0\0\x01crx isom")
      + box(b"moov", box(b"uuid", canon + box(b"CMT1", cmt1)))
      + large_box(b"uuid", preview + bytes(8) + box(b"PRVW", bytes(16) + jpeg["medium"])))
# RAF: Fujifilm's magic, and the JPEG's offset and length at 84 and 88
head = b"FUJIFILMCCD-RAW 0201FF383501".ljust(84, b"\0")
head += struct.pack(">II", 100, len(jpeg["oriented"])) + bytes(8)
write("raw.raf", head + jpeg["oriented"])
EOF
rm -r "$raw"
# Photoshop documents, tiny: what ImageMagick writes (RGB run-length coded, 16-bit RGB raw,
# gray, CMYK, indexed, bitmap, RGB with transparency in straight colors, dark and light at
# partial alpha, RGB with the P3 profile djxl writes, and CMYK with transparency in straight
# colors), then layouts it does not write,
# built by hand below, CMYK with a profile, at 16 bits and with transparency among them.
psd=$(mktemp -d)
magick -size 6x4 gradient:red-blue -depth 8 -compress RLE "$out/psd-rgb.psd"
magick -size 6x4 gradient:red-blue -depth 16 -compress None "$out/psd-rgb16.psd"
magick -size 6x4 gradient:black-white -colorspace Gray -depth 8 -compress RLE "$out/psd-gray.psd"
magick -size 4x2 xc:red -colorspace CMYK -depth 8 -compress RLE "$out/psd-cmyk.psd"
magick -size 4x2 xc:red xc:lime +append -depth 8 -type Palette "$out/psd-index.psd"
magick -size 6x4 pattern:gray50 -type Bilevel -depth 1 "$out/psd-bitmap.psd"
magick 'xc:rgba(40,40,40,0.5)' 'xc:rgba(220,220,220,0.5)' 'xc:rgba(40,160,40,0.2)' xc:red \
	'xc:rgba(10,20,30,0)' +append -depth 8 "$out/psd-straight.psd"
djxl --quiet "$out/p3.jxl" "$psd/p3.png"
magick "$psd/p3.png" -depth 8 "$out/psd-p3.psd"
magick 'xc:rgba(40,40,40,0.5)' 'xc:rgba(220,220,220,0.5)' 'xc:rgba(0,160,160,0.5)' xc:red \
	'xc:rgba(10,20,30,0)' +append -colorspace CMYK -depth 8 "$out/psd-cmyk-straight.psd"
rm -r "$psd"
python3 - "$out" <<'EOF'
import struct, sys

out = sys.argv[1]

def write(name, data):
    with open(f"{out}/{name}", "wb") as f:
        f.write(data)

def packbits(row):
    packed, at = b"", 0
    while at < len(row):
        run = 1
        while at + run < len(row) and run < 128 and row[at + run] == row[at]:
            run += 1
        if run > 1:
            packed += bytes([257 - run, row[at]])
        else:
            end = at + 1
            while end < len(row) and end - at < 128 and (end + 1 >= len(row)
                                                         or row[end] != row[end + 1]):
                end += 1
            packed += bytes([end - at - 1]) + row[at:end]
            run = end - at
        at += run
    return packed

def psd(name, w, h, mode, depth, planes, big=False, rle=False, resources=(), colors=b"",
        layers=b"", compression=0, pack=packbits):
    """A document of `planes`, each the rows of one channel; `resources` are (ID, data)."""
    data = b"8BPS" + struct.pack(">H", 2 if big else 1) + bytes(6)
    data += struct.pack(">HIIHH", len(planes), h, w, depth, mode)
    data += struct.pack(">I", len(colors)) + colors
    blocks = b"".join(b"8BIM" + struct.pack(">H", id) + b"\0\0" + struct.pack(">I", len(body))
                      + body + b"\0" * (len(body) % 2) for id, body in resources)
    data += struct.pack(">I", len(blocks)) + blocks
    data += struct.pack(">Q" if big else ">I", len(layers)) + layers
    row = (w + 7) // 8 if depth == 1 else w * depth // 8
    if rle:
        rows = [pack(plane[y * row:(y + 1) * row]) for plane in planes for y in range(h)]
        data += struct.pack(">H", 1)
        data += struct.pack(f">{len(rows)}{'I' if big else 'H'}", *map(len, rows))
        data += b"".join(rows)
    else:
        data += struct.pack(">H", compression) + b"".join(planes)
    write(name, data)

def matte(color, alpha, most=255):
    """A color blended with white by its alpha, as Photoshop stores the composite."""
    return [c * alpha // most + most - alpha for c in color]

def planar(pixels, size=1):
    return [b"".join(p[c].to_bytes(size, "big") for p in pixels) for c in range(len(pixels[0]))]

# RGB with transparency, the layer count negative, in colors blended with white: opaque red,
# blue at 40%, green at 20%, dark and light gray at half, and nothing
pixels = [(255, 0, 0, 255), (*matte((0, 0, 255), 102), 102), (*matte((0, 255, 0), 51), 51),
          (*matte((40, 40, 40), 128), 128), (*matte((220, 220, 220), 128), 128),
          (255, 255, 255, 0)]
psd("psd-matte.psd", 3, 2, 3, 8, planar(pixels), rle=True,
    layers=struct.pack(">Ih", 2, -1) + bytes(4))
# PSB, 16-bit gray with transparency that a Mt16 block marks, the layers left empty, run-length
# coded with four-byte counts: opaque middle gray, a quarter gray at half, and nothing
pixels = [(0x8000, 0xffff), (*matte((0x4000,), 0x8000, 0xffff), 0x8000), (0xffff, 0)]
psd("psd-gray16.psb", 3, 1, 1, 16, planar(pixels, 2), big=True, rle=True,
    layers=struct.pack(">QI", 0, 0) + b"8BIMMt16" + struct.pack(">Q", 0))
# 16-bit RGB whose layers sit in a Lr16 block, with the negative count, the layer info empty
pixels = [(0xffff, 0, 0, 0xffff), (0xffff, 0xffff, 0xffff, 0)]
psd("psd-lr16.psd", 2, 1, 3, 16, planar(pixels, 2),
    layers=struct.pack(">II", 0, 0) + b"8BIMLr16" + struct.pack(">Ih", 4, -1) + bytes(2))
# indexed color with a transparent index: red, green and blue, the last transparent
table = bytearray(768)
table[0], table[256 + 1], table[512 + 2] = 255, 255, 255
psd("psd-index-alpha.psd", 3, 1, 2, 8, [bytes([0, 1, 2])], colors=bytes(table),
    resources=[(1047, struct.pack(">H", 2))])
# gray packed loosely, each byte a literal of its own and two bytes left over, as some
# writers do
psd("psd-loose.psd", 3, 1, 1, 8, [bytes([0, 128, 255])], rle=True,
    pack=lambda row: b"".join(b"\0" + bytes([value]) for value in row) + b"\0\0")

def cmyk_profile():
    """A tiny CMYK printer profile, a lut16 of two points a channel into XYZ, whose magenta ink
    takes away red and cyan green, the other way about from real inks, so a conversion through
    it tells itself apart from one without."""
    s15 = lambda v: struct.pack(">i", round(v * 65536))
    # sRGB's primaries adapted to D50, the columns of its matrix
    primaries = [(0.4360747, 0.2225045, 0.0139322), (0.3850649, 0.7168786, 0.0971045),
                 (0.1430804, 0.0606169, 0.7141733)]
    table = b""
    for c in (0, 1):
        for m in (0, 1):
            for y in (0, 1):
                for k in (0, 1):
                    light = [(1 - m) * (1 - k), (1 - c) * (1 - k), (1 - y) * (1 - k)]
                    xyz = [sum(light[i] * primaries[i][j] for i in range(3)) for j in range(3)]
                    table += b"".join(struct.pack(">H", round(v * 32768)) for v in xyz)
    lut = b"mft2" + bytes(4) + bytes([4, 3, 2, 0])
    lut += b"".join(s15(v) for v in (1, 0, 0, 0, 1, 0, 0, 0, 1))
    lut += struct.pack(">HH", 2, 2) + struct.pack(">8H", *[0, 65535] * 4) + table
    lut += struct.pack(">6H", *[0, 65535] * 3)
    white = b"XYZ " + bytes(4) + s15(0.9642) + s15(1.0) + s15(0.8249)
    tags = [(b"A2B0", lut), (b"wtpt", white)]
    at, directory, data = 128 + 4 + 12 * len(tags), b"", b""
    for signature, body in tags:
        body += bytes(-len(body) % 4)
        directory += signature + struct.pack(">II", at + len(data), len(body))
        data += body
    size = 128 + 4 + len(directory) + len(data)
    header = struct.pack(">I", size) + bytes(4) + struct.pack(">I", 0x02100000)
    header += b"prtrCMYKXYZ " + bytes(12) + b"acsp" + bytes(28) + s15(0.9642) + s15(1.0)
    header += s15(0.8249) + bytes(48)
    return header + struct.pack(">I", len(tags)) + directory + data

# CMYK, stored with 0 for full ink: through that profile, full cyan and no ink
psd("psd-cmyk-icc.psd", 2, 1, 4, 8, planar([(0, 255, 255, 255), (255, 255, 255, 255)]),
    resources=[(1039, cmyk_profile())])
# CMYK at 16 bits with no profile: red
psd("psd-cmyk16.psd", 1, 1, 4, 16, planar([(0xffff, 0, 0, 0xffff)], 2))
# CMYK with transparency, blended with white where no ink is: red, cyan at half, and nothing
pixels = [(255, 0, 0, 255, 255), (*matte((0, 255, 255, 255), 128), 128), (255, 255, 255, 255, 0)]
psd("psd-cmyk-alpha.psd", 3, 1, 4, 8, planar(pixels), rle=True,
    layers=struct.pack(">Ih", 2, -1) + bytes(4))
# CMYK with transparency over cyan and black ink, blended with white in its stored samples as
# Photoshop does: each source at half beside itself opaque, then nothing; with no profile, and
# through the profile above
pixels = []
for source in [(0, 255, 255, 191), (255, 255, 255, 0), (64, 200, 255, 128)]:
    pixels += [(*matte(source, 128), 128), (*source, 255)]
pixels.append((255, 255, 255, 255, 0))
psd("psd-cmyk-black.psd", 7, 1, 4, 8, planar(pixels), rle=True,
    layers=struct.pack(">Ih", 2, -1) + bytes(4))
psd("psd-cmyk-icc-alpha.psd", 7, 1, 4, 8, planar(pixels), rle=True,
    layers=struct.pack(">Ih", 2, -1) + bytes(4), resources=[(1039, cmyk_profile())])
# RGB with its transparency and a spot channel after it, which is not shown
pixels = [(255, 0, 0, 255, 0x55), (0, 0, 0, 0, 0x55)]
psd("psd-spot.psd", 2, 1, 3, 8, planar(pixels), rle=True,
    layers=struct.pack(">Ih", 2, -1) + bytes(4))
# duotone, which shows as the gray it stores
psd("psd-duotone.psd", 2, 1, 8, 8, [bytes([0, 255])], colors=bytes(24))
# saved with Maximize Compatibility off: the version info says the composite is no image, and
# Photoshop leaves it white
writer = "Adobe Photoshop".encode("utf-16-be")
version = struct.pack(">IBI", 1, 0, 15) + writer + struct.pack(">I", 0) + struct.pack(">I", 1)
psd("psd-no-composite.psd", 2, 1, 3, 8, [bytes([255, 255])] * 3, resources=[(1057, version)])
# refused: a ZIP-compressed composite, Lab color, and 32 bits a channel
psd("psd-zip.psd", 2, 1, 3, 8, [bytes(2)] * 3, compression=2)
psd("psd-lab.psd", 2, 1, 9, 8, [bytes(2)] * 3)
psd("psd-32.psd", 2, 1, 3, 32, [bytes(8)] * 3)
EOF
# Apple icon files: what libicns's png2icns writes (run-length RGB with masks at 16 and 32,
# and PNG at 128, and the same without the PNG), then by hand run-length RGB with it32 the
# largest, ih32 with its mask, il32 with a mask of the wrong size, is32 as it stands, ARGB,
# and JPEG 2000
icns=$(mktemp -d)
for side in 16 32 128; do
	magick -size "${side}x${side}" gradient:red-blue -depth 8 -strip "PNG32:$icns/$side.png"
done
png2icns "$out/icns-png.icns" "$icns/16.png" "$icns/32.png" "$icns/128.png" >/dev/null
png2icns "$out/icns-small.icns" "$icns/16.png" "$icns/32.png" >/dev/null
magick -size 32x32 gradient:red-blue -alpha off -quality 50 "$icns/32.jp2"
python3 - "$out" "$icns" <<'EOF'
import struct, sys

out, icns = sys.argv[1:]

def pack(channel):
    """Apple's icon run-length code: runs of three to 130, literals of one to 128."""
    packed, at = b"", 0
    while at < len(channel):
        run = 1
        while at + run < len(channel) and run < 130 and channel[at + run] == channel[at]:
            run += 1
        if run >= 3:
            packed += bytes([run + 125, channel[at]])
            at += run
        else:
            end = min(at + run, len(channel))
            literal = channel[at:end]
            packed += bytes([len(literal) - 1]) + literal
            at = end
    return packed

def icon_file(entries):
    body = b"".join(kind + struct.pack(">I", 8 + len(data)) + data for kind, data in entries)
    return b"icns" + struct.pack(">I", 8 + len(body)) + body

def quarters(side, colors):
    """Each channel of an icon in four quarters of `colors`, by rows then columns."""
    half = side // 2
    return [bytes(colors[(y >= half) * 2 + (x >= half)][c] for y in range(side)
                  for x in range(side)) for c in range(len(colors[0]))]

# it32 beside a smaller il32 with its mask: red, green, blue and white, opaque as it32 has no
# mask here
rgb = quarters(128, [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)])
small = quarters(32, [(9, 9, 9)] * 4)
with open(f"{out}/icns-rgb.icns", "wb") as f:
    f.write(icon_file([(b"il32", b"".join(map(pack, small))), (b"l8mk", bytes(32 * 32)),
                       (b"it32", bytes(4) + b"".join(map(pack, rgb)))]))
# ih32 with its mask: the quarters, the last transparent
rgba = quarters(48, [(255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255), (255, 255, 255, 0)])
with open(f"{out}/icns-ih32.icns", "wb") as f:
    f.write(icon_file([(b"ih32", b"".join(map(pack, rgba[:3]))), (b"h8mk", rgba[3])]))
# il32 with a mask of the wrong size, which is left out
with open(f"{out}/icns-mask-size.icns", "wb") as f:
    f.write(icon_file([(b"il32", b"".join(map(pack, small))), (b"l8mk", bytes(100))]))
# is32 as RGB that stands as it is, three bytes a pixel: red
with open(f"{out}/icns-raw.icns", "wb") as f:
    f.write(icon_file([(b"is32", b"\xff\0\0" * 256)]))
# ARGB at 32, beside RGB at 16: the same quarters, the last at half
argb = quarters(32, [(255, 255, 0, 0), (255, 0, 255, 0), (255, 0, 0, 255), (128, 255, 255, 255)])
with open(f"{out}/icns-argb.icns", "wb") as f:
    f.write(icon_file([(b"is32", b"".join(map(pack, quarters(16, [(9, 9, 9)] * 4)))),
                       (b"ic05", b"ARGB" + b"".join(map(pack, argb)))]))
# JPEG 2000 at 32, as icp5 may hold it
jp2 = open(f"{icns}/32.jp2", "rb").read()
with open(f"{out}/icns-jp2.icns", "wb") as f:
    f.write(icon_file([(b"icp5", jp2)]))
EOF
rm -r "$icns"
# Vision's inputs for edit mode on macOS, with ImageMagick and zint: printed words holding an
# email address, and a phone number in groups among other words, in DejaVu Sans from
# fonts-dejavu-core; a QR code whose payload the tests know; and a dark disk with a soft shadow
# on a plain ground, which stands out as the foreground. None holds a face: no image of one
# with a license that allows it is to hand.
# The four images are the project's own, under its MIT license. The glyphs in text.png and
# phone.png are DejaVu Sans's, from Debian's fonts-dejavu-core, under the DejaVu Fonts License:
# the Bitstream Vera Fonts license, with DejaVu's changes in the public domain, which lets a
# font's glyphs be drawn into an image under any license.
magick -size 720x200 xc:white -font /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf \
	-pointsize 36 -fill black -annotate +24+76 'Write to jane.doe@example.com' \
	-annotate +24+150 'before the end of May' -strip "$out/text.png"
magick -size 720x100 xc:white -font /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf \
	-pointsize 36 -fill black -annotate +24+64 'Call +1 555 123 4567 today' -strip "$out/phone.png"
qr=$(mktemp --suffix=.png)
zint -b QRCODE -d 'https://github.com/nsxiv/nsxiv' --scale=4 -o "$qr"
magick "$qr" -bordercolor white -border 16 -strip "$out/qr.png"
rm "$qr"
magick -size 400x300 xc:'#d8d8d8' \
	\( -size 400x300 xc:none -fill 'rgba(0,0,0,0.35)' -draw 'ellipse 205,250 95,18 0,360' \
	-blur 0x8 \) -composite -fill '#7a1800' -draw 'circle 200,150 200,62' \
	-alpha off -depth 8 -strip "$out/subject.png"
