#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
portable_root="$repo_root/dist/DSH Atelier Portable"
app="$portable_root/DSH Atelier.app"

if [ -d "$portable_root/data" ] && [ -n "$(find "$portable_root/data" -mindepth 1 -print -quit)" ]; then
  echo "refusing to package over non-empty portable data: $portable_root/data" >&2
  exit 1
fi

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/icons" "$portable_root/data"
: > "$portable_root/atelier.portable"

install -m 755 "$repo_root/target/release/dsh-atelier" "$app/Contents/MacOS/DSH Atelier"
install -m 755 "$repo_root/target/release/dsh-atelier-runtime" "$app/Contents/MacOS/dsh-atelier-runtime"
cp "$script_dir/Info.plist" "$app/Contents/Info.plist"
cp "$script_dir/DSH Atelier.icns" "$app/Contents/Resources/DSH Atelier.icns"
cp "$repo_root/assets/icons/deepseek-blue.ico" "$app/Contents/Resources/icons/"
cp "$repo_root/assets/icons/deepseek-black.ico" "$app/Contents/Resources/icons/"
cp "$repo_root/assets/icons/deepseek-black.svg" "$app/Contents/Resources/icons/"

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/crates/dsh-atelier/Cargo.toml" | head -1)
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion 1" "$app/Contents/Info.plist"

plutil -lint "$app/Contents/Info.plist" >/dev/null
echo "$app"
