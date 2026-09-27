#!/usr/bin/env python3
"""Refuse to publish a SecurePlan CAD release that is not newer than the latest.

  release_order.py <candidate tag> <latest published tag | --none>

Exits 0 when the candidate `secureplan-cad-vX.Y.Z` is strictly greater than
the latest published release, or when the caller passes `--none` because
GitHub confirmed there is no release yet (HTTP 404). Exits 1 otherwise,
including for a latest tag outside the scheme, and 2 for bad arguments; an
empty latest tag is bad arguments, so a failed lookup that produced nothing
is never taken for "no release". This keeps `releases/latest` (which the
updater and the SecurePlan download button read) from moving backwards
(DSK-05, DSK-07).
"""

import re
import sys

PREFIX = "secureplan-cad-v"
PART = r"(0|[1-9][0-9]{0,8})"
TAG = re.compile(rf"^{re.escape(PREFIX)}{PART}\.{PART}\.{PART}$")


def version(tag: str):
    """(major, minor, patch) of a release tag, or None outside the scheme."""
    match = TAG.match(tag)
    if not match:
        return None
    parts = tuple(int(group) for group in match.groups())
    return parts if parts[0] <= 255 else None


NONE = "--none"


def may_publish(candidate: str, latest: str):
    """(allowed, reason) for publishing `candidate` over `latest` (NONE: there is none)."""
    new = version(candidate)
    if new is None:
        return False, f"{candidate} is not a SecurePlan CAD release tag"
    if latest == NONE:
        return True, f"{candidate} is the first release"
    current = version(latest)
    if current is None:
        return False, f"the latest release {latest} is not a SecurePlan CAD release tag; publish by hand after checking it"
    if new <= current:
        return False, f"{candidate} is not newer than the latest release {latest}"
    return True, f"{candidate} is newer than {latest}"


def main(argv) -> int:
    if len(argv) != 3 or not argv[2]:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    allowed, reason = may_publish(argv[1], argv[2])
    print(reason, file=sys.stdout if allowed else sys.stderr)
    return 0 if allowed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
