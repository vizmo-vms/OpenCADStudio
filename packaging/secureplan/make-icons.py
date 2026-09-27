#!/usr/bin/env python3
"""Regenerate SecurePlan CAD's application icons from the Vizmo logo mark (DSK-08).

Run it with the pinned Pillow (requirements-icons.txt), from the repository root:

    python3 -m venv /tmp/secureplan-icons
    /tmp/secureplan-icons/bin/pip install -r packaging/secureplan/requirements-icons.txt
    /tmp/secureplan-icons/bin/python packaging/secureplan/make-icons.py

Reads assets/secureplan/vizmo-logo-mark.png (224 x 224, the SecurePlan web
app's src/assets/vizmo-logo-mark.png) and writes, next to this script:
- AppIcon.icns: the macOS .app and .dmg icon (make-release-dmg.sh, make-dev-app.sh);
- AppIcon.ico: the Windows executable (build.rs) and MSI (build-msi.ps1) icon.

The 256 px Windows icon holds the mark itself, unscaled and centred; the Rust
test `the_window_icon_and_about_are_vizmo_and_name_the_source` checks that
pixel for pixel. The macOS icon is upscaled from the 224 px source; replace the
source with a larger mark (1024 px) when one exists and run this again.
"""

from pathlib import Path

import PIL
from PIL import Image

PILLOW = "12.1.1"
HERE = Path(__file__).resolve().parent
SOURCE = HERE.parent.parent / "assets" / "secureplan" / "vizmo-logo-mark.png"


def scaled(mark: Image.Image, size: int, content: float) -> Image.Image:
    """The mark centred on a transparent square, scaled to fill `content` of it."""
    inner = round(size * content)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.paste(mark.resize((inner, inner), Image.Resampling.LANCZOS), ((size - inner) // 2, (size - inner) // 2))
    return canvas


def unscaled(mark: Image.Image, size: int) -> Image.Image:
    """The mark itself, pixel for pixel, centred on a transparent square."""
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.paste(mark, ((size - mark.width) // 2, (size - mark.height) // 2))
    return canvas


def main() -> None:
    if PIL.__version__ != PILLOW:
        raise SystemExit(f"make-icons.py needs Pillow {PILLOW} (requirements-icons.txt), not {PIL.__version__}.")
    mark = Image.open(SOURCE).convert("RGBA")
    # macOS icons keep a margin around the artwork, as Apple's template does.
    scaled(mark, 1024, 0.8).save(HERE / "AppIcon.icns")
    # Windows: the mark at its own size in the 256 px icon, smaller sizes from it.
    unscaled(mark, 256).save(
        HERE / "AppIcon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


if __name__ == "__main__":
    main()
