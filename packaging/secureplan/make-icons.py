#!/usr/bin/env python3
"""Regenerate SecurePlan CAD's application icons from the Vizmo logo mark (DSK-08).

    python3 packaging/secureplan/make-icons.py

Reads assets/secureplan/vizmo-logo-mark.png (224 x 224, from the SecurePlan web
app) and writes, next to this script:
- AppIcon.icns: the macOS .app and .dmg icon (make-release-dmg.sh, make-dev-app.sh);
- AppIcon.ico: the Windows executable (build.rs) and MSI (build-msi.ps1) icon.

The larger sizes are upscaled from the 224 px source; replace the source with a
larger mark (1024 px) when one exists and run this again. Needs Pillow.
"""

from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
SOURCE = HERE.parent.parent / "assets" / "secureplan" / "vizmo-logo-mark.png"


def square(mark: Image.Image, size: int, content: float) -> Image.Image:
    """The mark centred on a transparent square, filling `content` of it."""
    inner = round(size * content)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    scaled = mark.resize((inner, inner), Image.Resampling.LANCZOS)
    canvas.paste(scaled, ((size - inner) // 2, (size - inner) // 2), scaled)
    return canvas


def main() -> None:
    mark = Image.open(SOURCE).convert("RGBA")
    # macOS icons keep a margin around the artwork, as Apple's template does.
    square(mark, 1024, 0.8).save(HERE / "AppIcon.icns")
    # Windows icons use the whole square; Pillow keeps sizes up to the image's.
    square(mark, 256, 1.0).save(
        HERE / "AppIcon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


if __name__ == "__main__":
    main()
