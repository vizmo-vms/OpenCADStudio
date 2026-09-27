#!/usr/bin/env bash
# Publish a built and verified SecurePlan CAD release (DSK-05): the release
# workflow's publish job, as a script so that it can be tested with a stub
# `gh` (packaging/secureplan/test_publish_release.py).
#
#   packaging/secureplan/publish-release.sh <tag> <version> <assets folder> <notes file>
#
# Needs gh (with GH_REPO and GH_TOKEN), curl, sha256sum and python3. Steps:
# verify the assets -> the tag must be newer than the latest release ->
# create a draft -> upload -> download and verify the draft -> check the
# order again -> publish and mark latest -> check releases/latest serves it.
#
# The latest release is "none" only when GitHub answers 404 for
# /releases/latest; any other failure to read it stops the publication, so
# a failed lookup can never let an older release create a draft or become
# latest.
set -euo pipefail

tag="${1:?usage: publish-release.sh <tag> <version> <assets folder> <notes file>}"
version="${2:?version}"
dist="${3:?assets folder}"
notes="${4:?notes file}"
: "${GH_REPO:?GH_REPO must name the repository}"
here="$(cd "$(dirname "$0")" && pwd)"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
assets=(SecurePlanCAD-macos-arm64.dmg SecurePlanCAD-macos-x64.dmg SecurePlanCAD-windows-x64.msi SecurePlanCAD-source.tar.gz SHA256SUMS)

# The latest published release's tag, or `--none` when GitHub confirms there
# is none (HTTP 404). Fails on anything else.
latest_release() {
  local err out
  err="$(mktemp)"
  if out="$(gh api "repos/$GH_REPO/releases/latest" --jq .tag_name 2>"$err")"; then
    rm -f "$err"
    if [ -z "$out" ]; then
      echo "::error::GitHub answered releases/latest without a tag" >&2
      return 1
    fi
    printf '%s\n' "$out"
    return 0
  fi
  if grep -q 'HTTP 404' "$err"; then
    rm -f "$err"
    printf '%s\n' --none
    return 0
  fi
  echo "::error::the latest release could not be read, so nothing is published: $(tr '\n' ' ' < "$err")" >&2
  rm -f "$err"
  return 1
}

# Stop unless $tag is newer than the latest release.
check_order() {
  local latest
  latest="$(latest_release)" || exit 1
  python3 "$here/release_order.py" "$tag" "$latest" || exit 1
}

echo "Verifying the assets."
(
  cd "$dist"
  if [ "$(ls | sort)" != "$(printf '%s\n' "${assets[@]}" | sort)" ]; then
    echo "::error::unexpected assets: $(ls | tr '\n' ' ')" >&2
    exit 1
  fi
  sha256sum -c SHA256SUMS
)

check_order

existing="$(gh api "repos/$GH_REPO/releases" --paginate --jq ".[] | select(.tag_name == \"$tag\") | .id")"
if [ -n "$existing" ]; then
  echo "::error::a release (possibly a draft) for $tag already exists; delete it or release a new version" >&2
  exit 1
fi
gh release create "$tag" --draft --verify-tag --title "SecurePlan CAD $version" --notes-file "$notes"
gh release upload "$tag" "${assets[@]/#/$dist/}"

draft="$(mktemp -d)"
gh release download "$tag" --dir "$draft"
if [ "$(ls "$draft" | sort)" != "$(ls "$dist" | sort)" ]; then
  echo "::error::the draft's assets differ: $(ls "$draft" | tr '\n' ' ')" >&2
  exit 1
fi
cmp "$draft/SHA256SUMS" "$dist/SHA256SUMS"
(cd "$draft" && sha256sum -c SHA256SUMS)

# Another release may have been published meanwhile: check again.
check_order
gh release edit "$tag" --draft=false --prerelease=false --latest

latest="$(latest_release)"
if [ "$latest" != "$tag" ]; then
  echo "::error::releases/latest is $latest, not $tag" >&2
  exit 1
fi
served="$(mktemp -d)"
for name in "${assets[@]}"; do
  curl -fsSL --retry 5 --retry-delay 10 -o "$served/$name" "https://github.com/$GH_REPO/releases/latest/download/$name"
done
(cd "$served" && sha256sum -c SHA256SUMS)
echo "Published $tag and marked it latest." | tee -a "$summary"
