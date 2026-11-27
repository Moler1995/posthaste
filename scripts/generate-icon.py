"""Pack the application icon and the sidebar brand mark from the brand assets.

The artwork is **C3 / THE WAX** from ``resources/brand/brand-mark/``. It is
hand-written SVG rendered to a PNG outside this repository; this script does
not draw it. Both seals are transparent renders of the seal group alone,
without the badge background or wordmark, so every icon slot shows the wax on
whatever surface it lands on. The script packs the Windows icon container:

- ``resources/icons/posthaste.ico`` with 16, 24, 32, 48, 64, 128, and 256 px
  frames, embedded in the executable by ``build.rs``.
- ``resources/icons/posthaste-mark.png``, the seal at 32x32, drawn at 16
  logical pixels in the sidebar's brand row. gpui cannot decode ICO, so the
  mark needs its own raster file.

Frames below ``DETAIL_FLOOR`` come from ``seal-small.png``, the character-only
cut: it drops the ring inscription, and the hare of the detailed seal is scaled
up to fill the face. The 26-unit ring text turns into dark specks below 128 px,
while the single dark hare still reads at 24 px. The 128 and 256 px frames come
from the detailed ``seal.png``, where the engraving is readable. Every frame is
a LANCZOS reduction of the 1024 px transparent seal, so the silhouette matches
at every size. The icon deliberately uses C3 below its usual 128 px floor: as
application chrome it needs one mark that survives the small slots.

This script is a maintenance tool, not a build prerequisite. Pillow is not
required to build Posthaste; the committed ICO and PNG are used as-is.
Regenerate only when the brand asset changes.

Requirements:
    Python 3.10+ and Pillow (``pip install pillow``).

Usage:
    python scripts/generate-icon.py [--out resources/icons/posthaste.ico]
                                    [--mark-out resources/icons/posthaste-mark.png]
"""

from __future__ import annotations

import argparse
import io
import struct
from pathlib import Path
from typing import cast

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
BRAND = ROOT / "resources" / "brand" / "brand-mark"
DETAILED_SEAL = BRAND / "seal.png"
SMALL_SEAL = BRAND / "seal-small.png"

EXPECTED_SIZES = (16, 24, 32, 48, 64, 128, 256)
# Frames at or above this size show the ring inscription; smaller ones use the
# character-only cut.
DETAIL_FLOOR = 128


def source_for(size: int) -> Path:
    return DETAILED_SEAL if size >= DETAIL_FLOOR else SMALL_SEAL


def load_seal(path: Path) -> Image.Image:
    with Image.open(path) as source:
        image = source.convert("RGBA")
    if image.size != (1024, 1024):
        raise SystemExit(f"{path.name}: expected 1024x1024, found {image.size}")
    return image


def load_frame(seal: Image.Image, size: int) -> Image.Image:
    return seal.resize((size, size), Image.Resampling.LANCZOS)


def bmp_frame(image: Image.Image) -> bytes:
    """Encode a 32-bit BGRA icon frame with the required empty AND mask."""
    width, height = image.size
    pixels = image.load()
    assert pixels is not None
    rows = bytearray()
    for y in range(height - 1, -1, -1):
        for x in range(width):
            red, green, blue, alpha = pixels[x, y]
            rows += bytes((blue, green, red, alpha))
    and_stride = ((width + 31) // 32) * 4
    and_mask = b"\x00" * (and_stride * height)
    header = struct.pack(
        "<IiiHHIIiiII",
        40,  # biSize
        width,
        height * 2,  # XOR bitmap plus AND mask
        1,  # biPlanes
        32,  # biBitCount
        0,  # biCompression (BI_RGB)
        len(rows) + len(and_mask),
        0,
        0,
        0,
        0,
    )
    return header + bytes(rows) + and_mask


def png_frame(image: Image.Image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", optimize=True)
    return buffer.getvalue()


def write_ico(path: Path, frames: list[tuple[int, bytes]]) -> None:
    directory = struct.pack("<HHH", 0, 1, len(frames))
    offset = 6 + 16 * len(frames)
    entries = bytearray()
    payload = bytearray()
    for size, data in frames:
        dimension = 0 if size >= 256 else size
        entries += struct.pack(
            "<BBBBHHII",
            dimension,
            dimension,
            0,  # color count
            0,  # reserved
            1,  # planes
            32,  # bit count
            len(data),
            offset,
        )
        payload += data
        offset += len(data)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(directory + bytes(entries) + bytes(payload))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        type=Path,
        default=ROOT / "resources" / "icons" / "posthaste.ico",
        help="output ICO path (default: resources/icons/posthaste.ico)",
    )
    parser.add_argument(
        "--mark-out",
        type=Path,
        default=ROOT / "resources" / "icons" / "posthaste-mark.png",
        help="output in-app mark path (default: resources/icons/posthaste-mark.png)",
    )
    args = parser.parse_args()

    seals = {DETAILED_SEAL: load_seal(DETAILED_SEAL), SMALL_SEAL: load_seal(SMALL_SEAL)}
    images: dict[int, Image.Image] = {}
    sources: dict[int, Path] = {}
    for size in EXPECTED_SIZES:
        source = source_for(size)
        if source not in seals:
            raise SystemExit(f"{size} px frame has no source seal: {source}")
        images[size] = load_frame(seals[source], size)
        sources[size] = source
    # The floor is the whole point of the split: frames below it must come from
    # the character-only cut, and the large frames from the detailed seal.
    for size in EXPECTED_SIZES:
        expected = SMALL_SEAL if size < DETAIL_FLOOR else DETAILED_SEAL
        if sources[size] != expected:
            raise SystemExit(f"{size} px frame packed from {sources[size].name}")

    # The wax is the icon, so the smallest frame must still show a red centre
    # rather than collapsing into the dark badge background.
    red, green, blue, alpha = cast(
        tuple[int, int, int, int], images[16].getpixel((8, 8))
    )
    if alpha < 200 or red <= green or red <= blue:
        raise SystemExit("16 px frame does not show the wax seal")

    frames: list[tuple[int, bytes]] = []
    for size in EXPECTED_SIZES:
        image = images[size]
        frames.append((size, png_frame(image) if size >= 256 else bmp_frame(image)))

    write_ico(args.out, frames)

    # Read the directory back so a malformed file fails here, not in Explorer.
    data = args.out.read_bytes()
    reserved, kind, count = struct.unpack_from("<HHH", data)
    found = []
    for index in range(count):
        width, height = struct.unpack_from("<BB", data, 6 + 16 * index)
        found.append((width or 256, height or 256))
    if (reserved, kind) != (0, 1):
        raise SystemExit("ICO header is malformed")
    if tuple(size for size, _ in found) != EXPECTED_SIZES:
        raise SystemExit(f"ICO frames are wrong: {found}")

    print(f"wrote {args.out} ({args.out.stat().st_size} bytes)")
    print(
        "frames: "
        + ", ".join(f"{size} px <- {sources[size].name}" for size in EXPECTED_SIZES)
    )

    args.mark_out.parent.mkdir(parents=True, exist_ok=True)
    images[32].save(args.mark_out, format="PNG", optimize=True)
    with Image.open(args.mark_out) as mark:
        if mark.size != (32, 32) or mark.mode != "RGBA":
            raise SystemExit(f"mark is malformed: {mark.size} {mark.mode}")
    print(f"wrote {args.mark_out} ({args.mark_out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
