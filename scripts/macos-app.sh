#!/bin/sh
# Builds ATOMICS.app: the release binary, the atomisk.ai icon and an Info.plist.
# The app belongs to macOS once opened, so closing a terminal no longer quits it.
# Usage: scripts/macos-app.sh [destination folder, default ~/Applications]
set -eu
[ "$(uname -s)" = Darwin ] || { echo 'This packager requires macOS; use scripts/linux-install.sh on Linux.' >&2; exit 1; }

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-"$HOME/Applications"}
app="$dest/ATOMICS.app"
# What this replaces is kept here, under a name macOS does not offer as an app: `atomics rollback` goes back.
previous="$dest/.atomics-previous"
target_dir=${CARGO_TARGET_DIR:-"$root/target"}
target=${CARGO_BUILD_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}

cargo build --release --locked --manifest-path "$root/Cargo.toml" -p atomics \
  --target "$target" --target-dir "$target_dir"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)

mkdir -p "$dest"
# Assembled beside where it goes, on the same disk, and swapped in only once it is whole: a failure on the way leaves the
# app that was there.
work=$(mktemp -d "$dest/.atomics-update-XXXXXX")
trap 'rm -rf "$work"' EXIT
iconset="$work/ATOMICS.iconset"
new="$work/ATOMICS.app"
mkdir -p "$iconset"
render() { # size output
  if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w "$1" -h "$1" "$root/assets/icon.svg" -o "$2"
  else
    sips -z "$1" "$1" "$root/assets/icon.png" --out "$2" >/dev/null
  fi
}
for size in 16 32 128 256 512; do
  render "$size" "$iconset/icon_${size}x${size}.png"
  render $((size * 2)) "$iconset/icon_${size}x${size}@2x.png"
done

mkdir -p "$new/Contents/MacOS" "$new/Contents/Resources"
cp "$target_dir/$target/release/atomics" "$new/Contents/MacOS/atomics"
iconutil -c icns "$iconset" -o "$new/Contents/Resources/ATOMICS.icns"
cat > "$new/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ATOMICS</string>
  <key>CFBundleDisplayName</key><string>ATOMICS</string>
  <key>CFBundleIdentifier</key><string>ai.atomisk.atomics</string>
  <key>CFBundleExecutable</key><string>atomics</string>
  <key>CFBundleIconFile</key><string>ATOMICS</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>ai.atomisk.atomics</string>
      <key>CFBundleURLSchemes</key>
      <array><string>atomics</string></array>
    </dict>
  </array>
  <key>NSMicrophoneUsageDescription</key>
  <string>ATOMICS records your voice while you use the voice shortcut and sends the transcript to the chief agent.</string>
</dict>
</plist>
EOF

# Ad-hoc signature: lets macOS remember the microphone permission for the app.
codesign --force --deep --sign - "$new" >/dev/null 2>&1 || echo "codesign failed; the app still runs" >&2
# A build for this Mac has to run here before it goes under the real name.
if [ "$target" = "$(rustc -vV | sed -n 's/^host: //p')" ]; then
  said=$("$new/Contents/MacOS/atomics" --version 2>&1) || { echo "The new build does not run: $said" >&2; exit 1; }
fi
sync
if [ -d "$app" ]; then
  rm -rf "$previous"
  mv "$app" "$previous"
fi
if ! mv "$new" "$app"; then
  [ -d "$previous" ] && mv "$previous" "$app"
  echo "Could not put the new app at $app" >&2
  exit 1
fi
sync
# The bundle builds from before the rename to ATOMICS made.
rm -rf -- "$dest/IMACS.app"
echo "$app"
