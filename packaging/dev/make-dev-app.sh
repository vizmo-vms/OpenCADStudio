#!/usr/bin/env bash
# Assemble a development SecurePlan CAD .app bundle from a debug build, with
# the secureplan-cad: link scheme registered (DSK-04). For manual checkpoints
# only; release bundles come from the release workflow.
#
#   packaging/dev/make-dev-app.sh <build dir> [output .app]
#
# <build dir> holds the `OpenCADStudio` GUI binary and the `ocs_launcher`
# relay from `cargo build --features secureplan` (for example
# target/aarch64-apple-darwin/debug). Then move the app to /Applications,
# approve it once in System Settings > Privacy & Security, and launch it once
# so Launch Services registers the scheme.
set -euo pipefail

build_dir="${1:?usage: make-dev-app.sh <build dir> [output .app]}"
app="${2:-SecurePlan CAD Dev.app}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
version="$(sed -n 's/^pub const VERSION: &str = "\(.*\)";$/\1/p' "$root/src/secureplan/mod.rs")"
test -n "$version"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# Finder and Launch Services talk to the relay, which hands launch URLs to the
# GUI over the per-user channel (never as arguments).
cp "$build_dir/ocs_launcher" "$app/Contents/MacOS/OpenCADStudio"
cp "$build_dir/OpenCADStudio" "$app/Contents/MacOS/OpenCADStudio-App"
chmod +x "$app/Contents/MacOS/OpenCADStudio" "$app/Contents/MacOS/OpenCADStudio-App"
cp "$root/packaging/secureplan/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
# A development identity, so it never replaces an installed release.
sed -e "s/__VERSION__/$version/g" \
    -e "s/<string>in.vizmo.secureplan.cad<\/string>/<string>in.vizmo.secureplan.cad.dev<\/string>/" \
    -e "s/<string>SecurePlan CAD<\/string>/<string>SecurePlan CAD Dev<\/string>/g" \
    "$root/packaging/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
# Apple Silicon runs only signed code; an ad-hoc signature is enough here.
codesign --force --deep --sign - --timestamp=none "$app"
codesign --verify --deep --strict "$app"
echo "Built $app (SecurePlan CAD $version, development)."
