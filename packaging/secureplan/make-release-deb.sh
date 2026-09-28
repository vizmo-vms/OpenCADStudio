#!/usr/bin/env bash
# Build the release Debian package of SecurePlan CAD for Linux x64 (DSK-05):
# the package `secureplan-cad`, which installs
#   /usr/bin/secureplan-cad                         the release build (--features secureplan)
#   /usr/share/applications/in.vizmo.secureplan.cad.desktop
#                                                   the menu entry, and the secureplan-cad: link
#                                                   scheme (x-scheme-handler/secureplan-cad)
#   /usr/share/icons/hicolor/<N>x<N>/apps/in.vizmo.secureplan.cad.png
#                                                   the Vizmo icon (make-icons.py)
#   /usr/share/doc/secureplan-cad/copyright         GPL-3, with the source location
#   /usr/share/doc/secureplan-cad/changelog.gz      this version, pointing to its release
# and no maintainer scripts: the dpkg triggers of desktop-file-utils and the
# hicolor icon theme refresh the MIME handler and icon caches. It registers no
# drawing file types.
#
#   packaging/secureplan/make-release-deb.sh <release binary> <output .deb>
#
# Needs dpkg-deb and dpkg-shlibdeps (dpkg-dev) and readelf (binutils); no
# fakeroot (files are owned by root through --root-owner-group). The release
# workflow builds on ubuntu-22.04, so the package runs on Ubuntu 22.04 and
# later and on Debian 12 and later.
set -euo pipefail
umask 022

binary="${1:?usage: make-release-deb.sh <release binary> <output .deb>}"
out="${2:?output .deb}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
version="$(sed -n 's/^pub const VERSION: &str = "\(.*\)";$/\1/p' "$root/src/secureplan/mod.rs")"
test -n "$version"
package="secureplan-cad"
# The updater expects these (src/secureplan/update_helper.rs).
program="/usr/bin/secureplan-cad"
desktop_id="in.vizmo.secureplan.cad"
icon_sizes=(16 24 32 48 64 128 256)

if ! readelf -h "$binary" | grep -q 'Machine:.*X86-64'; then
  echo "error: $binary is not an x86-64 program" >&2
  exit 1
fi
# Reproducible file times: the commit's, unless the caller sets one.
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct)}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
pkg="$work/pkg"
install -d -m 0755 "$pkg/DEBIAN"
install -D -m 0755 "$binary" "$pkg$program"
install -D -m 0644 "$here/linux/$desktop_id.desktop" "$pkg/usr/share/applications/$desktop_id.desktop"
for size in "${icon_sizes[@]}"; do
  install -D -m 0644 "$here/linux/icon-$size.png" "$pkg/usr/share/icons/hicolor/${size}x${size}/apps/$desktop_id.png"
done
install -D -m 0644 "$here/linux/copyright" "$pkg/usr/share/doc/$package/copyright"
maintainer="Vizmo <hari@vizmo.in>"
{
  echo "$package ($version) stable; urgency=medium"
  echo
  echo "  * SecurePlan CAD $version. Release notes:"
  echo "    https://github.com/vizmo-vms/OpenCADStudio/releases/tag/secureplan-cad-v$version"
  echo
  echo " -- $maintainer  $(LC_ALL=C date -u -R -d "@$SOURCE_DATE_EPOCH")"
} | gzip -9n > "$pkg/usr/share/doc/$package/changelog.gz"
chmod 0644 "$pkg/usr/share/doc/$package/changelog.gz"

# Libraries the program links (its ELF NEEDED entries), as dpkg-shlibdeps
# resolves them on the build system.
mkdir -p "$work/shlibs/debian"
printf 'Source: %s\n\nPackage: %s\nArchitecture: amd64\n' "$package" "$package" > "$work/shlibs/debian/control"
linked="$(cd "$work/shlibs" && dpkg-shlibdeps -O "$pkg$program" | sed -n 's/^shlibs:Depends=//p')"
test -n "$linked"
# Libraries the program opens at run time (dlopen, so not in NEEDED; the
# sonames are in the binary: strings | grep '\.so'):
# - libvulkan1 (libvulkan.so.1), libegl1 (libEGL.so.1): the wgpu Vulkan and
#   GL renderers;
# - libwayland-client0, libwayland-egl1: Wayland windows;
# - libx11-6, libx11-xcb1, libxcb1, libxcursor1, libxi6: X11 windows;
# - libxkbcommon0, libxkbcommon-x11-0: the keyboard on both;
# - libdbus-1-3: the file chooser (xdg-desktop-portal).
runtime="libvulkan1, libegl1, libwayland-client0, libwayland-egl1, libx11-6, libx11-xcb1, libxcb1, libxcursor1, libxi6, libxkbcommon0, libxkbcommon-x11-0, libdbus-1-3"
# Usually present on a desktop: GPU drivers, the portal that shows the file
# chooser, zenity (message boxes and the chooser without a portal), pkexec
# (installing updates) and xdg-utils (opening links).
recommends="mesa-vulkan-drivers | vulkan-icd, libegl-mesa0, xdg-desktop-portal-gtk | xdg-desktop-portal-backend, zenity, pkexec | policykit-1, xdg-utils"

size_kib="$(du -sk --exclude=DEBIAN "$pkg" | cut -f1)"
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: $package
Version: $version
Architecture: amd64
Maintainer: $maintainer
Installed-Size: $size_kib
Depends: $linked, $runtime
Recommends: $recommends
Section: graphics
Priority: optional
Homepage: https://github.com/vizmo-vms/OpenCADStudio
Description: CAD companion for SecurePlan
 SecurePlan CAD opens, edits and applies the drawings of SecurePlan surveys.
 Start it from a survey's CAD tab in SecurePlan (Edit in desktop), which
 opens secureplan-cad: links. It is built on Open CAD Studio.
CONTROL
(cd "$pkg" && find usr -type f -print0 | LC_ALL=C sort -z | xargs -0 md5sum) > "$pkg/DEBIAN/md5sums"
chmod 0644 "$pkg/DEBIAN/control" "$pkg/DEBIAN/md5sums"

rm -f "$out"
dpkg-deb --root-owner-group -Zxz --build "$pkg" "$out"

# Check what was built.
dpkg-deb --info "$out"
[ "$(dpkg-deb -f "$out" Package)" = "$package" ]
[ "$(dpkg-deb -f "$out" Version)" = "$version" ]
[ "$(dpkg-deb -f "$out" Architecture)" = amd64 ]
# No maintainer scripts.
controls="$(dpkg-deb --ctrl-tarfile "$out" | tar -t | sed 's|^\./||' | grep -v '^$' | LC_ALL=C sort | tr '\n' ' ')"
if [ "$controls" != "control md5sums " ]; then
  echo "error: unexpected control files: $controls" >&2
  exit 1
fi
expected="$( {
  echo ".$program"
  echo "./usr/share/applications/$desktop_id.desktop"
  for size in "${icon_sizes[@]}"; do echo "./usr/share/icons/hicolor/${size}x${size}/apps/$desktop_id.png"; done
  echo "./usr/share/doc/$package/copyright"
  echo "./usr/share/doc/$package/changelog.gz"
} | LC_ALL=C sort)"
files="$(dpkg-deb --fsys-tarfile "$out" | tar -t | grep -v '/$' | LC_ALL=C sort)"
if [ "$files" != "$expected" ]; then
  echo "error: unexpected package contents:" >&2
  diff <(echo "$expected") <(echo "$files") >&2 || true
  exit 1
fi
dpkg-deb --contents "$out"
dpkg-deb -x "$out" "$work/check"
cmp "$binary" "$work/check$program"
# Owned by root, the program executable and nothing else writable by others.
owners="$(dpkg-deb --fsys-tarfile "$out" | tar -tv | awk '{print $2}' | sort -u)"
[ "$owners" = "root/root" ]
[ "$(dpkg-deb --fsys-tarfile "$out" | tar -tv | awk -v p=".$program" '$6 == p {print $1}')" = "-rwxr-xr-x" ]
if dpkg-deb --fsys-tarfile "$out" | tar -tv | awk '{print $1}' | grep -q '^.....w\|^........w'; then
  echo "error: the package has group- or world-writable entries" >&2
  exit 1
fi
if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$work/check/usr/share/applications/$desktop_id.desktop"
fi
grep -qx "MimeType=x-scheme-handler/secureplan-cad;" "$work/check/usr/share/applications/$desktop_id.desktop"
grep -qx "Exec=$program %u" "$work/check/usr/share/applications/$desktop_id.desktop"
echo "Built $out (SecurePlan CAD $version, amd64)."
