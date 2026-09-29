#!/usr/bin/env bash
# Install the internal SecurePlan CAD Linux test package on a disposable CI
# runner, check it, and remove it again. Needs sudo; never run it on a
# workstation.
#
#   packaging/secureplan/check-deb-install.sh <package .deb>
#
# Checks: apt installs it with its dependencies, desktop-file-utils (whose
# trigger registers the link scheme) among them; `secureplan-cad --version`
# names SecurePlan CAD and the package version; the desktop database lists it
# as the handler of secureplan-cad: links (x-scheme-handler/secureplan-cad,
# through the desktop-file-utils trigger) and registers no drawing types; the
# icon is installed; `apt-get remove` takes everything away again.
set -euo pipefail

deb="$(realpath "${1:?usage: check-deb-install.sh <package .deb>}")"
if [ "${CI:-}" != true ]; then
  echo "error: check-deb-install.sh installs a system package; it runs on CI runners only" >&2
  exit 1
fi
version="$(dpkg-deb -f "$deb" Version)"
cache=/usr/share/applications/mimeinfo.cache

case ", $(dpkg-deb -f "$deb" Depends), " in
  *", desktop-file-utils, "*) ;;
  *) echo "error: the package does not depend on desktop-file-utils" >&2; exit 1 ;;
esac
sudo apt-get install -y "$deb"
[ "$(dpkg-query -W -f '${Status} ${Version}' secureplan-cad)" = "install ok installed $version" ]
[ "$(dpkg-query -W -f '${Status}' desktop-file-utils)" = "install ok installed" ]
desktop-file-validate /usr/share/applications/in.vizmo.secureplan.cad.desktop
out="$(env -u DISPLAY -u WAYLAND_DISPLAY secureplan-cad --version)"
case "$out" in
  "SecurePlan CAD $version"*) ;;
  *) echo "error: --version printed: $out" >&2; exit 1 ;;
esac
grep -qx 'x-scheme-handler/secureplan-cad=in.vizmo.secureplan.cad.desktop;' "$cache"
if grep -q 'in.vizmo.secureplan.cad.desktop' <(grep -v '^x-scheme-handler/secureplan-cad=' "$cache"); then
  echo "error: SecurePlan CAD is registered for other types" >&2
  exit 1
fi
test -s /usr/share/icons/hicolor/256x256/apps/in.vizmo.secureplan.cad.png

sudo apt-get remove -y secureplan-cad
if dpkg-query -W -f '${Status}' secureplan-cad 2>/dev/null | grep -q 'ok installed'; then
  echo "error: secureplan-cad is still installed" >&2
  exit 1
fi
test ! -e /usr/bin/secureplan-cad
if grep -q 'secureplan' "$cache"; then
  echo "error: the secureplan-cad: handler is still registered" >&2
  exit 1
fi
echo "Installed, checked and removed $deb (secureplan-cad $version)."
