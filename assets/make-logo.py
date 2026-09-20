#!/usr/bin/env python3
"""Generate the Disk Scanner logo and app icon: a treemap that spells a glyph.

What makes this legible at 16pt is that the tiles are not laid out and then
coloured. The glyph is rasterised to a coarse grid first, and the tiles are a
*rectangle decomposition* of that grid -- one set covering the cells inside
the glyph, another covering the cells outside. No tile ever straddles the
outline, so the shape is exact at every size while the rectangles stay as
irregular as a real treemap's.

Colours come from the app itself (Theme.swift, Palette): the map in slate, the
lens in the teal that means "bytes you would actually get back".

Two artefacts, from the same tiling:

  logo-scan.svg     a flat, self-contained SVG with its own panel, for the
                    README and anywhere that is not a macOS app icon.
  AppIcon.icon      an Icon Composer document: the map and the lens as two
                    separate layers on a transparent canvas, with the shape
                    mask, the background fill and the Liquid Glass material
                    left to macOS. AppIcon.icns and the preview renders are
                    produced from it by ictool, Apple's own renderer, so what
                    you see here is what the system draws.

  ./make-logo.py                    # regenerate everything, into assets/
  ./make-logo.py --glyph D          # a letter instead of the magnifier
  ./make-logo.py --ascii            # print the glyph grid and stop
  ./make-logo.py --flat-only        # skip anything that needs Xcode
"""

import argparse
import json
import math
import pathlib
import random
import shutil
import subprocess
from PIL import Image, ImageDraw, ImageFont

HERE = pathlib.Path(__file__).resolve().parent

# Palette (dark appearance) from app/Sources/DiskScanner/Theme.swift.
TEAL = (0x3E, 0xD9, 0xBC)  # yours: what deleting actually returns
BLUE = (0x6F, 0xA8, 0xFF)  # cloned
PURPLE = (0xB4, 0x9B, 0xFF)  # pinned
TEAL_DEEP = (0x0E, 0xA8, 0x8E)  # Palette.yours, pushed deeper for glass
BLUE_DEEP = (0x2F, 0x6F, 0xD0)
SLATE_LO = (0x1C, 0x28, 0x38)
SLATE_HI = (0x2A, 0x3A, 0x50)
BG_TOP = (0x13, 0x1D, 0x2B)
BG_BOT = (0x08, 0x0C, 0x14)

SS = 32  # supersampled pixels per grid cell

# The chosen geometry, picked from the `--matrix` contact sheet:
# grid 10, ring radius 3.5 cells, 1.5-cell wall, 1.5-cell handle.
DEFAULT_GRID = 10
SCAN_RADIUS_FRAC = 0.35
SCAN_WALL = 1.5
FUTURA = "/System/Library/Fonts/Supplemental/Futura.ttc"
FUTURA_BOLD = 2


def hexs(c):
    return "#%02X%02X%02X" % tuple(max(0, min(255, int(v))) for v in c)


def mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def jitter(c, rnd, amount=0.06):
    """The app's deterministic lightness jitter: siblings stay distinct
    without the jitter carrying a second meaning."""
    d = (rnd.random() - 0.5) * 2 * amount * 255
    return tuple(v + d for v in c)


# ---------------------------------------------------------------- glyph masks


def _bin(img, n, coverage, pad=1.0, crop=True):
    """Bin a high-resolution drawing down to an n x n boolean grid.

    Letters are cropped to their ink first: a font's own side bearings would
    otherwise waste two columns of tiles. Geometric glyphs are not, because
    they are composed against the square already and cropping would recentre
    the ring on the handle's bounding box.
    """
    if crop:
        box = img.getbbox()
        if box:
            img = img.crop(box)
        w, h = img.size
        side = max(w, h)
        square = Image.new("L", (side, side), 0)
        square.paste(img, ((side - w) // 2, (side - h) // 2))
    else:
        square = img
    inner = max(1, int(round(n - 2 * pad)))
    small = square.resize((inner, inner), Image.BOX)
    px = small.load()
    off = (n - inner) // 2
    grid = [[False] * n for _ in range(n)]
    for y in range(inner):
        for x in range(inner):
            grid[y + off][x + off] = px[x, y] / 255.0 >= coverage
    return grid


def mask_text(n, text, coverage=0.40, pad=1.0, font=FUTURA, index=FUTURA_BOLD):
    ss = 32 * n
    img = Image.new("L", (ss * 2, ss * 2), 0)
    d = ImageDraw.Draw(img)
    f = ImageFont.truetype(font, int(ss * 0.8), index=index)
    d.text((ss * 0.5, ss * 0.5), text, fill=255, font=f)
    return _bin(img, n, coverage, pad)


def _quantise(img, n):
    """Bin a supersampled drawing to the grid, symmetric across the diagonal.

    Each component of the glyph is quantised on its own. Symmetrising the
    *combined* coverage instead lets one component lend coverage to another
    across the diagonal, which is how an earlier version grew a stray tile
    hanging off the bottom of the ring: the disc's tangent cell (0.3) averaged
    with the handle's cell opposite it (0.7) and both crossed the threshold.
    """
    small = img.resize((n, n), Image.BOX)
    px = small.load()
    cov = [[px[x, y] / 255.0 for x in range(n)] for y in range(n)]
    return [[(cov[y][x] + cov[x][y]) / 2 >= 0.5 for x in range(n)] for y in range(n)]


def _disc(n, c, r):
    """A disc of radius `r` cells centred at `c` cells, quantised to the grid.

    The -1 on the far corner is not a fudge: PIL's ellipse bounds are
    *inclusive*, so the naive box draws a shape one pixel wider to the right
    and below. That is enough to push the tangent cell on those two sides past
    the 0.5 cut while its mirror stays under, which is what made the ring
    lopsided and left a stray tile under it.
    """
    ss = n * SS
    img = Image.new("L", (ss, ss), 0)
    ImageDraw.Draw(img).ellipse(
        [(c - r) * SS, (c - r) * SS, (c + r) * SS - 1, (c + r) * SS - 1], fill=255)
    return _quantise(img, n)


def mask_scan(n, r_out=None, wall=SCAN_WALL, handle_w=None, handle_end=None):
    """A magnifying glass, defined in CELL units rather than canvas fractions.

    Three things make it symmetric and round rather than lopsided:

    1. The ring's centre sits on the diagonal (cx == cy), at a whole number of
       supersampled pixels, so binning rounds the left and right sides
       identically. The old version centred it at 0.415/0.395 of the canvas --
       off the diagonal and off the grid -- which is why one side of the ring
       came out a cell thinner than the other.
    2. The ring is the *difference of two quantised discs*, not a thresholded
       ring bitmap. Thresholding the ring directly puts a notch at the top and
       bottom of the circle, where the inner edge runs tangent to a row of
       cells and their coverage lands just under the cut.
    3. Every component is symmetrised across the diagonal before thresholding,
       and separately from the others. The mask is exactly equal to its own
       transpose; `--ascii` reports it.
    """
    if r_out is None:
        r_out = round(n * SCAN_RADIUS_FRAC * 2) / 2   # snapped to half-cells
    r_in = r_out - wall
    c = r_out + 0.5               # ring tucked against the top-left corner
    if handle_w is None:
        handle_w = wall
    if handle_end is None:
        handle_end = n - 0.5      # handle runs at 45 degrees to the far corner

    outer, inner = _disc(n, c, r_out), _disc(n, c, r_in)

    ss = n * SS
    img = Image.new("L", (ss, ss), 0)
    k = 0.7071 * (r_in + wall / 2) * SS
    ImageDraw.Draw(img).line(
        [(c * SS + k, c * SS + k), (handle_end * SS, handle_end * SS)],
        fill=255, width=int(handle_w * SS))
    grip = _quantise(img, n)

    return [[(outer[y][x] and not inner[y][x]) or grip[y][x] for x in range(n)]
            for y in range(n)]


def mask_disk(n, coverage=0.40):
    """A usage ring: thick annulus, the one shape that says "proportion of a
    whole" without any text."""
    ss = 32 * n
    img = Image.new("L", (ss, ss), 0)
    d = ImageDraw.Draw(img)
    c, r = ss / 2, ss * 0.46
    d.ellipse([c - r, c - r, c + r, c + r], fill=255)
    h = ss * 0.245
    d.ellipse([c - h, c - h, c + h, c + h], fill=0)
    return _bin(img, n, coverage, crop=False)


# Overridden by --matrix to sweep the magnifier's geometry.
SCAN = {}


def glyph(name, n):
    if name == "scan":
        return mask_scan(n, **SCAN)
    if name == "disk":
        return mask_disk(n)
    return mask_text(n, name)


def ascii_mask(mask):
    return "\n".join("".join("##" if c else ". " for c in row) for row in mask)


# ------------------------------------------------------ rectangle decomposition


def decompose(mask, rnd, cap_in=2, cap_out=5):
    """Cover the grid with rectangles that never cross the glyph outline.

    Greedy, top-left first: grow the widest run of same-valued free cells,
    then the tallest block over that run, then shrink both by a random amount
    so sibling tiles differ in size the way a treemap's do. Inside the glyph
    the cap is tighter, because a few big slabs would read as a solid letter
    rather than as a map -- and because a 2x2 ceiling is what keeps a ring
    closed instead of beading into blobs.
    """
    n = len(mask)
    free = [[True] * n for _ in range(n)]
    rects = []
    for y in range(n):
        for x in range(n):
            if not free[y][x]:
                continue
            inside = mask[y][x]
            cap = cap_in if inside else cap_out
            w = 0
            while x + w < n and free[y][x + w] and mask[y][x + w] == inside and w < cap:
                w += 1
            h = 1
            while y + h < n and h < cap:
                if all(free[y + h][i] and mask[y + h][i] == inside for i in range(x, x + w)):
                    h += 1
                else:
                    break
            w = rnd.choice([w, w, w, max(1, w - 1), max(1, w - 2)])
            h = rnd.choice([h, h, h, max(1, h - 1), max(1, h - 2)])
            if w > 2 * h:
                w = min(w, h * 2)
            if h > 2 * w:
                h = min(h, w * 2)
            for yy in range(y, y + h):
                for xx in range(x, x + w):
                    free[yy][xx] = False
            rects.append((x, y, w, h, inside))
    return rects


# ------------------------------------------------------------------------- svg


def squircle(x, y, size, n=5.0, steps=192):
    """An Apple-style continuous corner is a superellipse, not an arc; a plain
    `rx` rounded rect reads as the wrong shape beside the system icons."""
    a = size / 2
    cx, cy = x + a, y + a
    pts = []
    for i in range(steps + 1):
        t = i / steps * 2 * math.pi
        ct, st = math.cos(t), math.sin(t)
        px = cx + a * (abs(ct) ** (2 / n)) * (1 if ct >= 0 else -1)
        py = cy + a * (abs(st) ** (2 / n)) * (1 if st >= 0 else -1)
        pts.append((px, py))
    return "M %.2f %.2f " % pts[0] + " ".join("L %.2f %.2f" % p for p in pts[1:]) + " Z"


def build(glyph_name, grid=DEFAULT_GRID, size=1024, seed=13, plate=True, margin=0.0977, detail=True,
          layer=None, gap_frac=None, deep=False):
    """Render the logo as SVG.

    `layer` selects one half of the tiling for Icon Composer: "ground" is the
    map, "glyph" is the magnifier. Each is emitted full-bleed on a transparent
    canvas, because in a Liquid Glass icon the shape mask, the background fill
    and the glass material all belong to the system, not to the artwork.
    """
    rnd = random.Random(seed)
    mask = glyph(glyph_name, grid)
    rects = decompose(mask, rnd)

    o = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" '
        f'width="{size}" height="{size}">',
        "<defs>",
        f'<linearGradient id="bg" x1="0" y1="0" x2="0.35" y2="1">'
        f'<stop offset="0" stop-color="{hexs(BG_TOP)}"/>'
        f'<stop offset="1" stop-color="{hexs(BG_BOT)}"/></linearGradient>',
    ]

    m = size * margin if plate else 0.0
    field = size - 2 * m
    path = squircle(m, m, field)
    o.append(f'<clipPath id="plate"><path d="{path}"/></clipPath>')
    o.append("</defs>")
    if plate:
        o.append(f'<path d="{path}" fill="url(#bg)"/>')

    # The tiles fill the panel and are cut by its corner, exactly as the map
    # is clipped by the window panel in the app.
    o.append('<g clip-path="url(#plate)">' if plate else "<g>")
    cell = field / grid
    # The glass material puts a bevel on every tile edge, so a gap that looks
    # right on the flat logo doubles visually and breaks the ring into beads.
    gap = cell * (gap_frac if gap_frac is not None else (0.095 if detail else 0.06))
    hair = max(0.6, cell * 0.022)

    for (gx, gy, gw, gh, inside) in rects:
        if layer == "ground" and inside:
            continue
        if layer == "glyph" and not inside:
            continue
        x, y = m + gx * cell + gap / 2, m + gy * cell + gap / 2
        w, h = gw * cell - gap, gh * cell - gap
        if w <= 0 or h <= 0:
            continue
        r = min(cell * 0.30, min(w, h) / 2.5)
        t = (gx + gy) / (2 * (grid - 1))  # diagonal ramp: direction, not flat fill
        # Light comes from the top, and the background gradient under these
        # tiles follows it. The map has to be lit by the same light: ramped
        # the other way it ends up darker than the background at the top, and
        # the gaps read as brighter than the tiles they separate.
        shade = 1 - gy / (grid - 1)
        rect = f'x="{x:.2f}" y="{y:.2f}" width="{w:.2f}" height="{h:.2f}" rx="{r:.2f}"'
        if inside:
            rest = jitter(mix(BLUE, mix(BLUE, PURPLE, 0.35), t), rnd, 0.05)
            # Glass lightens and desaturates whatever is under it, so the
            # artwork feeding a glass layer starts deeper than the app's own
            # teal in order to come out matching it.
            a, b = (TEAL_DEEP, BLUE_DEEP) if deep else (TEAL, mix(TEAL, BLUE, 0.35))
            core = jitter(mix(a, mix(a, b, 0.55), t), rnd, 0.05)
            frac = 0.45 + rnd.random() * 0.45 if detail else 1.0
            o.append(f'<rect {rect} fill="{hexs(rest)}" fill-opacity="0.72"/>')
            if frac < 0.995:
                cid = f"k{gx}_{gy}"
                o.append(f'<clipPath id="{cid}"><rect {rect}/></clipPath>')
                o.append(
                    f'<rect clip-path="url(#{cid})" x="{x:.2f}" y="{y + h * (1 - frac):.2f}" '
                    f'width="{w:.2f}" height="{h * frac:.2f}" fill="{hexs(core)}"/>'
                )
            else:
                o.append(f'<rect {rect} fill="{hexs(core)}"/>')
        else:
            base = jitter(mix(SLATE_LO, SLATE_HI, shade), rnd, 0.04)
            o.append(f'<rect {rect} fill="{hexs(base)}"/>')
            if detail:
                fr = 0.25 + rnd.random() * 0.4
                cid = f"k{gx}_{gy}"
                o.append(f'<clipPath id="{cid}"><rect {rect}/></clipPath>')
                o.append(
                    f'<rect clip-path="url(#{cid})" x="{x:.2f}" y="{y + h * (1 - fr):.2f}" '
                    f'width="{w:.2f}" height="{h * fr:.2f}" fill="#FFFFFF" '
                    f'fill-opacity="0.045"/>'
                )
        o.append(f'<rect {rect} fill="none" stroke="#000000" stroke-opacity="0.35" '
                 f'stroke-width="{hair:.2f}"/>')
    o.append("</g>")
    if plate:
        o.append(
            f'<path d="{path}" fill="none" stroke="#FFFFFF" stroke-opacity="0.10" '
            f'stroke-width="{size*0.0035:.2f}"/>'
        )
    o.append("</svg>")
    return "\n".join(o)


def layers(glyph_name, outdir, grid=DEFAULT_GRID, seed=13):
    """Write the two layer files an Icon Composer document consumes.

    `detail=False` drops the per-tile bottom fill here on purpose: the glass
    material supplies the depth, and the fill motif under it reads as noise
    at any size the icon is actually seen.
    """
    out = {}
    for name, key, gap in (("Ground", "ground", 0.065), ("Glyph", "glyph", 0.042)):
        f = outdir / f"{name}.svg"
        f.write_text(
            build(glyph_name, grid, seed=seed, plate=False, layer=key, detail=False,
                  gap_frac=gap, deep=(key == "glyph"))
        )
        out[key] = f
    return out


# Apple's Icon Composer document format, written by hand.
#
# Everything here was verified against `ictool`, the renderer inside Icon
# Composer.app, rather than guessed: appearance overrides are *arrays* of
# {appearance, value} pairs, not dictionaries keyed by appearance, and the
# dictionary form fails to parse with "the data couldn't be read".
#
# Two groups, which is the whole composition: the map lies flat on the
# background fill, and the lens sits above it in glass. The map hides itself
# in the tinted appearance -- tinting collapses every layer to one colour,
# and with the map present the lens would disappear into a field of tiles.
ICON_JSON = {
    # A *solid* fill, not Icon Composer's `automatic-gradient`. The automatic
    # gradient is generated bright-at-top -- measured at 75 luminance against
    # 31 at the bottom -- which is brighter than any tile in the map, so the
    # gaps glowed through the top third of the icon and the map looked like it
    # was lying under the background instead of being it. A solid fill renders
    # dead flat at 12 (there is no material sheen on the background at all),
    # which leaves the vertical light ramp to the tiles themselves, where it
    # belongs. The colour is the app window's own background.
    "fill": {"solid": "extended-srgb:0.03137,0.05098,0.08235,1.00000"},
    "groups": [
        {
            "layers": [
                {
                    "image-name": "Ground.svg",
                    "name": "Map",
                    "hidden-specializations": [{"appearance": "tinted", "value": True}],
                }
            ],
            "shadow": {"kind": "neutral", "opacity": 0.12},
            "specular": False,
        },
        {
            "layers": [{"image-name": "Glyph.svg", "name": "Lens", "glass": True}],
            "shadow": {"kind": "neutral", "opacity": 0.55},
            "specular": True,
            "translucency": {"enabled": False, "value": 0.5},
        },
    ],
    "supported-platforms": {"circles": ["watchOS"], "squares": "shared"},
}


def icon_document(glyph_name, outdir, grid=DEFAULT_GRID, seed=13, name="AppIcon"):
    """Write <name>.icon: layer art plus the document Icon Composer reads."""
    doc = outdir / f"{name}.icon"
    assets = doc / "Assets"
    assets.mkdir(parents=True, exist_ok=True)
    layers(glyph_name, assets, grid, seed)
    (doc / "icon.json").write_text(json.dumps(ICON_JSON, indent=2) + "\n")
    return doc


ICTOOL = pathlib.Path(
    "/Applications/Xcode.app/Contents/Applications/Icon Composer.app"
    "/Contents/Executables/ictool"
)


def render_icon(doc, out_path, px, rendition="Default", platform="macOS"):
    """Rasterise a .icon through Apple's own renderer, so the glass material,
    the shape mask and the highlights are the real ones and not an imitation.

    The re-save is not cosmetic: ictool writes PNGs with no compression
    tuning at all, and a lossless re-encode takes the 1024pt render from
    589K to 138K -- which is the difference between a 3.4M .icns in the
    repository and well under one.
    """
    subprocess.run(
        [str(ICTOOL), str(doc), "--export-image", "--output-file", str(out_path),
         "--platform", platform, "--rendition", rendition,
         "--width", str(px), "--height", str(px), "--scale", "1",
         "--design-generation", "26"],
        check=True, capture_output=True,
    )
    Image.open(out_path).save(out_path, optimize=True)


def png(svg_path, out_path, px):
    subprocess.run(
        ["rsvg-convert", "-w", str(px), "-h", str(px), str(svg_path), "-o", str(out_path)],
        check=True,
    )


ICNS_ENTRIES = [
    (16, "icon_16x16.png"), (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"), (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"), (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"), (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"), (1024, "icon_512x512@2x.png"),
]


def icns(doc, outdir, name="AppIcon"):
    """Build an .icns from the .icon, rendering every size separately.

    Every entry is rendered at its own size rather than downsampled from
    1024, because the glass renderer simplifies the material as the size
    drops -- and because a 13-cell map survives 16pt this way, which is the
    one thing a downsample would destroy. actool also emits an .icns, but
    only at 16 and 128; this one is complete.
    """
    iconset = outdir / f"{name}.iconset"
    shutil.rmtree(iconset, ignore_errors=True)
    iconset.mkdir(parents=True)
    for px, entry in ICNS_ENTRIES:
        render_icon(doc, iconset / entry, px)
    out = outdir / f"{name}.icns"
    subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out)], check=True)
    shutil.rmtree(iconset)
    return out


def preview(doc, outdir, px=512):
    """Default, Dark and Tinted side by side, plus the 32pt render, because
    the only questions that matter about an icon are whether it survives the
    appearances and whether it still reads small."""
    shots = []
    for r in ("Default", "Dark", "TintedDark"):
        f = outdir / f"icon-{r.lower()}.png"
        render_icon(doc, f, px, rendition=r)
        shots.append(Image.open(f).convert("RGBA"))
    small = outdir / ".icon-32.png"
    render_icon(doc, small, 32)
    shots.append(Image.open(small).convert("RGBA").resize((px, px), Image.NEAREST))
    small.unlink()
    sheet = Image.new("RGBA", (px * len(shots), px), (150, 152, 158, 255))
    for i, im in enumerate(shots):
        sheet.alpha_composite(im, (px * i, 0))
    out = outdir / "preview.png"
    sheet.save(out, optimize=True)
    return out


def matrix(outdir, px=240, cols=9):
    """Render every valid magnifier geometry as one labelled contact sheet.

    Grid resolution, ring radius, wall thickness and handle width, crossed.
    Combinations that cannot work are skipped rather than drawn badly: a hole
    smaller than two cells, a ring that does not fit the grid, or a ring
    leaving no room for a handle.
    """
    from PIL import ImageFont
    variants = []
    for n in (9, 10, 11, 12):
        for r_frac in (0.30, 0.35, 0.40):
            r_out = round(n * r_frac * 2) / 2
            if r_out > (n - 0.5) / 2:
                continue                       # ring does not fit
            if (n - 0.5) - (2 * r_out + 0.5) < 1.0:
                continue                       # no room for a handle
            for wall in (1.0, 1.5, 2.0):
                if r_out - wall < 1.0:
                    continue                   # hole too small to read
                for handle_w in (1.0, 1.5):
                    variants.append((n, r_out, wall, handle_w))

    work = outdir / "variants"
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    shots = []
    for i, (n, r_out, wall, handle_w) in enumerate(variants):
        SCAN.clear()
        SCAN.update(r_out=r_out, wall=wall, handle_w=handle_w)
        d = work / f"v{i:03d}"
        doc = icon_document("scan", d, grid=n, name="V")
        big, small = d / "b.png", d / "s.png"
        render_icon(doc, big, px)
        render_icon(doc, small, 32)
        shots.append((f"g{n} r{r_out} w{wall} h{handle_w}", big, small))
    SCAN.clear()

    rows = (len(shots) + cols - 1) // cols
    label_h = 18
    cell_h = px + px // 2 + label_h
    sheet = Image.new("RGB", (px * cols, cell_h * rows), (150, 152, 158))
    draw = ImageDraw.Draw(sheet)
    try:
        font = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial.ttf", 13)
    except OSError:
        font = None
    for i, (label, big, small) in enumerate(shots):
        cx, cy = (i % cols) * px, (i // cols) * cell_h
        sheet.paste(Image.open(big).convert("RGB"), (cx, cy))
        # the same variant at 32pt, nearest-upscaled beside its own label
        sheet.paste(Image.open(small).convert("RGB").resize((px//2, px//2), Image.NEAREST),
                    (cx, cy + px))
        draw.text((cx + px//2 + 6, cy + px + 8), label.replace(" ", "\n"),
                  fill=(20, 20, 24), font=font)
    out = outdir / "variants-matrix.png"
    sheet.save(out, optimize=True)
    shutil.rmtree(work, ignore_errors=True)
    return out, len(shots)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--glyph", default="scan", help="scan, disk, D, or any text")
    ap.add_argument("--grid", type=int, default=DEFAULT_GRID)
    ap.add_argument("--seed", type=int, default=13)
    ap.add_argument("--ascii", action="store_true", help="print the glyph grid and stop")
    ap.add_argument("--matrix", action="store_true",
                    help="render every magnifier geometry as a contact sheet and stop")
    ap.add_argument("--flat-only", action="store_true",
                    help="skip the .icon document and the renders that need Xcode")
    ap.add_argument("--out", default=str(HERE))
    args = ap.parse_args()
    outdir = pathlib.Path(args.out)
    outdir.mkdir(parents=True, exist_ok=True)
    name = args.glyph

    if args.matrix:
        out, count = matrix(outdir)
        print(f"{out}  ({count} variants)")
        return
    if args.ascii:
        m = glyph(name, args.grid)
        print(ascii_mask(m))
        n = len(m)
        sym = all(m[y][x] == m[x][y] for y in range(n) for x in range(n))
        print(f"diagonal-symmetric: {sym}")
        return

    # The flat logo: one self-contained SVG with its own panel, for the README
    # and anywhere that is not a macOS app icon.
    svg = outdir / f"logo-{name}.svg"
    svg.write_text(build(name, args.grid, seed=args.seed))
    png(svg, outdir / f"logo-{name}-512.png", 512)
    print(svg)

    if args.flat_only:
        return
    if not ICTOOL.exists():
        raise SystemExit(f"Icon Composer not found at {ICTOOL}; install Xcode 26 "
                         "or pass --flat-only")
    doc = icon_document(name, outdir, args.grid, args.seed)
    print(doc)
    print(icns(doc, outdir))
    print(preview(doc, outdir))


if __name__ == "__main__":
    main()
