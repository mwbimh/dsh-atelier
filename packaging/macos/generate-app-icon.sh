#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
source_svg="$repo_root/assets/icons/deepseek-app-monochrome.svg"
output_icns="$script_dir/DSH Atelier.icns"
temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/dsh-atelier-icon.XXXXXX")
iconset="$temporary_root/DSH Atelier.iconset"

cleanup() {
  rm -rf "$temporary_root"
}
trap cleanup EXIT INT TERM

mkdir -p "$iconset"
qlmanage -t -s 1024 -o "$temporary_root" "$source_svg" >/dev/null
rendered_png="$temporary_root/$(basename "$source_svg").png"
master_png="$temporary_root/DSH Atelier.png"
swift "$script_dir/apply-rounded-mask.swift" "$rendered_png" "$master_png"

render_size() {
  size=$1
  destination=$2
  sips -z "$size" "$size" "$master_png" --out "$iconset/$destination" >/dev/null
}

render_size 16 icon_16x16.png
render_size 32 icon_16x16@2x.png
render_size 32 icon_32x32.png
render_size 64 icon_32x32@2x.png
render_size 128 icon_128x128.png
render_size 256 icon_128x128@2x.png
render_size 256 icon_256x256.png
render_size 512 icon_256x256@2x.png
render_size 512 icon_512x512.png
cp "$master_png" "$iconset/icon_512x512@2x.png"

iconutil -c icns "$iconset" -o "$output_icns"
echo "$output_icns"
