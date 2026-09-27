#!/usr/bin/env python3
"""A local stand-in for SecurePlan CAD's GitHub Releases (DSK-07, CP5).

Serves one release from a folder of assets, the way the updater reads GitHub:

  GET /latest                    the release JSON (published two hours ago)
  GET /download/<tag>/<name>     302 to /assets/<name> (like GitHub's asset redirect)
  GET /assets/<name>             the file

Start a `secureplan-test` build of SecurePlan CAD with
SECUREPLAN_TEST_RELEASES_BASE=http://127.0.0.1:<port> (release builds ignore
it), then choose SecurePlan > Check for updates. With --corrupt, SHA256SUMS
lists wrong checksums, so the update must be refused with nothing changed.

  packaging/secureplan/mock-releases.py <assets folder> <version> [--port 8765] [--corrupt]

The folder holds the platform asset (for example SecurePlanCAD-macos-arm64.dmg
from a release workflow run); SHA256SUMS is computed here. Serves 127.0.0.1 only.
"""

import argparse
import hashlib
import http.server
import json
import pathlib
import time

ASSETS = ("SecurePlanCAD-macos-arm64.dmg", "SecurePlanCAD-macos-x64.dmg", "SecurePlanCAD-windows-x64.msi")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("folder", type=pathlib.Path)
    parser.add_argument("version")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--corrupt", action="store_true", help="serve SHA256SUMS with wrong checksums")
    args = parser.parse_args()

    tag = f"secureplan-cad-v{args.version}"
    files = {path.name: path for path in args.folder.iterdir() if path.name in ASSETS}
    if not files:
        parser.error(f"{args.folder} holds none of {', '.join(ASSETS)}")
    lines = []
    for name, path in sorted(files.items()):
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if args.corrupt:
            digest = digest[::-1]
        lines.append(f"{digest}  {name}\n")
    sums = "".join(lines).encode()
    published = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 7200))
    release = {
        "tag_name": tag,
        "draft": False,
        "prerelease": False,
        "published_at": published,
        "body": f"## SecurePlan CAD {args.version}\n- Served by mock-releases.py" + (" with corrupted checksums" if args.corrupt else ""),
        "assets": [{"name": name, "size": path.stat().st_size, "state": "uploaded"} for name, path in files.items()]
        + [{"name": "SHA256SUMS", "size": len(sums), "state": "uploaded"}],
    }

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802 (http.server naming)
            if self.path == "/latest":
                return self.reply(200, json.dumps(release).encode(), "application/json")
            prefix = f"/download/{tag}/"
            if self.path == prefix + "SHA256SUMS":
                return self.reply(200, sums, "text/plain")
            if self.path.startswith(prefix) and self.path[len(prefix):] in files:
                self.send_response(302)
                self.send_header("Location", "/assets/" + self.path[len(prefix):])
                self.send_header("Content-Length", "0")
                self.end_headers()
                return None
            if self.path.startswith("/assets/") and self.path[len("/assets/"):] in files:
                return self.reply(200, files[self.path[len("/assets/"):]].read_bytes(), "application/octet-stream")
            return self.reply(404, b"not found", "text/plain")

        def reply(self, status: int, body: bytes, kind: str) -> None:
            self.send_response(status)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"Serving {tag} ({', '.join(files)}) at http://127.0.0.1:{args.port}" + (" with corrupted checksums" if args.corrupt else ""))
    server.serve_forever()


if __name__ == "__main__":
    main()
