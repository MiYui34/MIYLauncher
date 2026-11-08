"""Rasterize the MIYLauncher icon: 1024px master -> PNG set + multi-size .ico.

Inputs : docs/icon/miy-icon-1024.png        (from miy-icon.svg, used for 48px and up)
         docs/icon/miy-icon-small-1024.png  (from miy-icon-small.svg, used for 32/24/16px)
Outputs: docs/icon/miy-icon-<size>.png, docs/icon/miy.ico, docs/icon/check-sizes.png

Never round-trip this file through PowerShell 5.1 (Get-Content/Set-Content):
it reads UTF-8 without BOM as ANSI and eats the lines after Chinese comments.
"""

import io
import os
import struct
import sys

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
ICON_DIR = os.path.join(os.path.dirname(HERE), "docs", "icon")

PNG_SIZES = [512, 256, 128, 64, 48, 32, 24, 16]
SMALL_SIZES = {32, 24, 16}
ICO_SIZES = [256, 128, 64, 48, 32, 24, 16]


def load(path, size=1024):
    if not os.path.exists(path):
        return None
    im = Image.open(path).convert("RGBA")
    if im.size != (size, size):
        print("[icon] skip %s: %dx%d" % (os.path.basename(path), im.size[0], im.size[1]))
        return None
    return im


def main() -> int:
    big = load(os.path.join(ICON_DIR, "miy-icon-1024.png"))
    if big is None:
        print("[icon] missing master: docs/icon/miy-icon-1024.png")
        return 1
    small = load(os.path.join(ICON_DIR, "miy-icon-small-1024.png")) or big
    print("[icon] masters: big=1024, small=%s" % ("dedicated" if small is not big else "shared"))

    rendered = {}
    for size in PNG_SIZES:
        src = small if size in SMALL_SIZES else big
        out = src.resize((size, size), Image.LANCZOS)
        path = os.path.join(ICON_DIR, "miy-icon-%d.png" % size)
        out.save(path, "PNG", optimize=True)
        rendered[size] = out
        print("[icon] %-28s %d B" % (os.path.basename(path), os.path.getsize(path)))

    # Pillow's ICO writer always rescales from one source image, so it cannot mix
    # two masters. Build the container by hand: every entry is a PNG blob (Vista+).
    # Entries are 128-colour palettes — on this flat artwork it is visually
    # indistinguishable and cuts the .ico from ~36 KB to ~12 KB (the icon sits
    # inside the exe, and the exe has a size budget).
    blobs = []
    for size in ICO_SIZES:
        src = small if size in SMALL_SIZES else big
        img = rendered.get(size) or src.resize((size, size), Image.LANCZOS)
        buf = io.BytesIO()
        img.quantize(colors=128, method=Image.Quantize.FASTOCTREE).save(
            buf, "PNG", optimize=True
        )
        blobs.append((size, buf.getvalue()))

    header = struct.pack("<HHH", 0, 1, len(blobs))
    offset = 6 + 16 * len(blobs)
    entries = b""
    data = b""
    for size, blob in blobs:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob

    ico_path = os.path.join(ICON_DIR, "miy.ico")
    with open(ico_path, "wb") as f:
        f.write(header + entries + data)
    print("[icon] %-28s %d B" % (os.path.basename(ico_path), os.path.getsize(ico_path)))

    # Contact sheet: 16px x8, 24px x6, 32px x4 — the sizes where an icon dies.
    zoom = [(16, 8), (24, 6), (32, 4)]
    width = sum(s * z for s, z in zoom) + 10 * (len(zoom) + 1)
    height = max(s * z for s, z in zoom) + 20
    sheet = Image.new("RGBA", (width, height), (249, 249, 255, 255))
    x = 10
    for size, factor in zoom:
        img = rendered[size].resize((size * factor, size * factor), Image.NEAREST)
        sheet.alpha_composite(img, (x, 10))
        x += size * factor + 10
    sheet_path = os.path.join(ICON_DIR, "check-sizes.png")
    sheet.save(sheet_path, "PNG")
    print("[icon] %-28s %d B" % (os.path.basename(sheet_path), os.path.getsize(sheet_path)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
