# What the edit smokes share, sourced by them with $pics their folder.

# how many pixels of images $1 and $2 differ in any channel, alpha left out, counted from their
# difference's pixels that are not black, which every ImageMagick counts the same
differ() {
    magick "$1" -alpha off "$pics/one.ppm"
    magick "$2" -alpha off "$pics/two.ppm"
    magick "$pics/one.ppm" "$pics/two.ppm" -compose difference -composite -separate \
        -evaluate-sequence max -threshold 0 -format '%[fx:round(mean*w*h)]' info:
}
# whether $1's fx expression $2 holds
holds() { [ "$(magick "$1" -format "%[fx:$2]" info:)" = 1 ]; }
# the images an edit smoke opens: a plain 640x400 one, and 3200x2400 of noise, 8 bits a channel,
# whose PNG is larger than one X request
edit_images() {
    magick -size 640x400 xc:'#405060' "$pics/plain.png"
    magick -size 3200x2400 xc: +noise Random -depth 8 "$pics/noise.png"
}
