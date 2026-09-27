#!/usr/bin/env bash
# Build the release disk image of SecurePlan CAD for one macOS architecture
# (DSK-05): "SecurePlan CAD.app" (bundle id in.vizmo.secureplan.cad, the
# secureplan-cad: scheme, no document types), ad-hoc signed with
# `codesign --force --deep -s -`, with an Applications shortcut and the Vizmo
# volume icon.
#
#   packaging/secureplan/make-release-dmg.sh <release build dir> <arm64|x86_64> <output .dmg>
#
# <release build dir> holds `OpenCADStudio` and `ocs_launcher` from
# `cargo build --release --features secureplan --target <target>`.
set -euo pipefail

build_dir="${1:?usage: make-release-dmg.sh <build dir> <arm64|x86_64> <output .dmg>}"
arch="${2:?architecture}"
out="${3:?output .dmg}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
version="$(sed -n 's/^pub const VERSION: &str = "\(.*\)";$/\1/p' "$root/src/secureplan/mod.rs")"
test -n "$version"

work="$(mktemp -d)"
trap 'hdiutil detach "$work/mnt" >/dev/null 2>&1 || true; hdiutil detach "$work/rwmnt" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
app="$work/root/SecurePlan CAD.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# Finder and Launch Services talk to the relay, which hands launch URLs to the
# GUI over the per-user channel (never as arguments).
cp "$build_dir/ocs_launcher" "$app/Contents/MacOS/OpenCADStudio"
cp "$build_dir/OpenCADStudio" "$app/Contents/MacOS/OpenCADStudio-App"
chmod +x "$app/Contents/MacOS/OpenCADStudio" "$app/Contents/MacOS/OpenCADStudio-App"
for binary in "$app/Contents/MacOS/OpenCADStudio" "$app/Contents/MacOS/OpenCADStudio-App"; do
  archs="$(lipo -archs "$binary")"
  if [ "$archs" != "$arch" ]; then
    echo "error: $(basename "$binary") is built for '$archs', expected '$arch'" >&2
    exit 1
  fi
done

# The Vizmo icon (DSK-08), made by packaging/secureplan/make-icons.py.
cp "$root/packaging/secureplan/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"

plist="$app/Contents/Info.plist"
sed "s/__VERSION__/$version/g" "$root/packaging/Info.plist" > "$plist"
plutil -lint "$plist"
buddy() { /usr/libexec/PlistBuddy -c "Print :$1" "$plist"; }
[ "$(buddy CFBundleIdentifier)" = "in.vizmo.secureplan.cad" ]
[ "$(buddy CFBundleShortVersionString)" = "$version" ]
[ "$(buddy CFBundleURLTypes:0:CFBundleURLSchemes:0)" = "secureplan-cad" ]
if buddy CFBundleDocumentTypes >/dev/null 2>&1; then
  echo "error: the app must not register document types (DSK-05)" >&2
  exit 1
fi

# Apple Silicon runs only signed code: an ad-hoc signature (DSK-05).
codesign --force --deep -s - --timestamp=none "$app"
codesign --verify --deep --strict --verbose=2 "$app"
ln -s /Applications "$work/root/Applications"
# The volume's Vizmo icon (DSK-08): .VolumeIcon.icns at the volume root, and
# the custom-icon Finder flag on the root, set on a writable image below.
cp "$root/packaging/secureplan/AppIcon.icns" "$work/root/.VolumeIcon.icns"

rm -f "$out"
rw="$work/rw.dmg"
for attempt in 1 2 3 4 5; do
  if hdiutil create -volname "SecurePlan CAD" -srcfolder "$work/root" -ov -format UDRW "$rw"; then
    break
  fi
  echo "hdiutil create failed (attempt $attempt); retrying in 5 s" >&2
  sleep 5
done
test -s "$rw"
mkdir -p "$work/rwmnt"
hdiutil attach -nobrowse -noautoopen -mountpoint "$work/rwmnt" "$rw"
# FinderInfo: finderFlags (bytes 8-9) = kHasCustomIcon (0x0400).
xattr -wx com.apple.FinderInfo 0000000000000000040000000000000000000000000000000000000000000000 "$work/rwmnt"
hdiutil detach "$work/rwmnt"
hdiutil convert "$rw" -format UDZO -o "$out"
test -s "$out"
hdiutil verify "$out"

# The image holds the signed app the updater expects.
mkdir -p "$work/mnt"
hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$work/mnt" "$out"
codesign --verify --deep --strict "$work/mnt/SecurePlan CAD.app"
cmp "$root/packaging/secureplan/AppIcon.icns" "$work/mnt/.VolumeIcon.icns"
flags="$(xattr -px com.apple.FinderInfo "$work/mnt" | tr -d ' \n')"
if (( (0x${flags:16:2} & 0x04) == 0 )); then
  echo "error: the volume has no custom icon flag" >&2
  exit 1
fi
hdiutil detach "$work/mnt"
echo "Built $out (SecurePlan CAD $version, $arch, ad-hoc signed)."
