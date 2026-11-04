#!/bin/sh
# Install/repair the VM launcher beside ATOMICS.app. Also embedded by `atomics vm launcher`.
set -eu
[ "$(uname -s)" = Darwin ] || { echo 'The ATOMICS VM launcher is macOS-only.' >&2; exit 1; }
dest=${1:-"$HOME/Applications"}
source=${2:-"$(dirname "$0")/macos-vm-launcher.c"}
app="$dest/ATOMICS VM.app"
main="$dest/ATOMICS.app"
identifier=ai.atomisk.atomics.vm
[ -x "$main/Contents/MacOS/atomics" ] || { echo 'Install ATOMICS.app in the destination first.' >&2; exit 1; }
[ ! -L "$app" ] || { echo 'The VM launcher is a symlink; keeping it.' >&2; exit 1; }
if [ -e "$app" ]; then
  existing=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist" 2>/dev/null || true)
  [ "$existing" = "$identifier" ] || { echo 'Keeping an app with a different bundle identifier.' >&2; exit 1; }
fi
work=$(mktemp -d "$dest/.atomics-vm-launcher.XXXXXX")
trap 'rm -rf "$work"' EXIT
stage="$work/ATOMICS VM.app"
mkdir -p "$stage/Contents/MacOS" "$stage/Contents/Resources"
/usr/bin/clang -Wall -Wextra -Werror -Os "$source" -o "$stage/Contents/MacOS/atomics-vm"
cp "$main/Contents/Resources/ATOMICS.icns" "$stage/Contents/Resources/ATOMICS.icns"
version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$main/Contents/Info.plist")
cat > "$stage/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>ATOMICS VM</string>
<key>CFBundleDisplayName</key><string>ATOMICS VM</string>
<key>CFBundleIdentifier</key><string>$identifier</string>
<key>CFBundleExecutable</key><string>atomics-vm</string>
<key>CFBundleIconFile</key><string>ATOMICS</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundleVersion</key><string>$version</string>
<key>LSMinimumSystemVersion</key><string>12.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --deep --sign - "$stage" >/dev/null 2>&1
# Keep the previous launcher until the replacement has built and signed successfully.
if [ -e "$app" ]; then mv "$app" "$work/previous.app"; fi
if ! mv "$stage" "$app"; then
  [ ! -e "$work/previous.app" ] || mv "$work/previous.app" "$app"
  exit 1
fi
printf '%s\n' "$app"
