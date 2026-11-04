#!/usr/bin/env python3
"""Draws the bar's whole icon set, sized to the font's cell.

Nerd Font glyphs are authored for a wider, taller cell than this condensed
font's. Grafting them (tools/merge-icons.py) scales them uniformly, which left
most of the set *overflowing* the cell — the wifi glyph measured 938 units in a
500-unit cell, nearly double — while the shapes drawn by hand ended up far
smaller than a character. Neither matched the text beside them.

So every icon the bar uses is drawn here instead, on the same measurements a
character occupies:

    width   ~460 units of the 500-unit cell (as 'X')
    height   0 (baseline) to 714 (cap height)

A single stroke weight (`STROKE`) keeps the set coherent, and drawing straight
into the destination cell means nothing is ever scaled after the fact.

Run via tools/build-font.sh, after merge-icons.py: what is drawn here replaces
the grafted glyph for the same codepoint.
"""

import math
import sys

from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont

# The box a character occupies, which every icon fills.
LEFT = 20
RIGHT = 480
BOTTOM = 0
TOP = 714

MID_X = (LEFT + RIGHT) / 2
MID_Y = (BOTTOM + TOP) / 2
WIDTH = RIGHT - LEFT
HEIGHT = TOP - BOTTOM

# One weight across the set: thinner reads as broken at 12 px, thicker fills in.
STROKE = 62


# --------------------------------------------------------------------------
# Primitives. Outer contours run clockwise, holes counter-clockwise, which is
# the winding TrueType needs to fill correctly.
# --------------------------------------------------------------------------


def rect(pen, x0, y0, x1, y1):
    pen.moveTo((x0, y0))
    pen.lineTo((x0, y1))
    pen.lineTo((x1, y1))
    pen.lineTo((x1, y0))
    pen.closePath()


def hole(pen, x0, y0, x1, y1):
    """Rectangular hole: same as `rect` wound the other way."""
    pen.moveTo((x0, y0))
    pen.lineTo((x1, y0))
    pen.lineTo((x1, y1))
    pen.lineTo((x0, y1))
    pen.closePath()


def frame(pen, x0, y0, x1, y1, t=STROKE):
    rect(pen, x0, y0, x1, y1)
    hole(pen, x0 + t, y0 + t, x1 - t, y1 - t)


def arc_points(cx, cy, r, a0, a1, steps=24):
    return [
        (cx + r * math.cos(a0 + (a1 - a0) * i / steps),
         cy + r * math.sin(a0 + (a1 - a0) * i / steps))
        for i in range(steps + 1)
    ]


def poly(pen, pts):
    pen.moveTo(pts[0])
    for p in pts[1:]:
        pen.lineTo(p)
    pen.closePath()


def disc(pen, cx, cy, r, steps=28):
    poly(pen, list(reversed(arc_points(cx, cy, r, 0, 2 * math.pi, steps)[:-1])))


def ring(pen, cx, cy, r, t=STROKE, steps=28):
    """Circle outline of thickness `t`."""
    poly(pen, list(reversed(arc_points(cx, cy, r, 0, 2 * math.pi, steps)[:-1])))
    poly(pen, arc_points(cx, cy, r - t, 0, 2 * math.pi, steps)[:-1])


def band(pen, pts, t):
    """Thick stroke along an open path, drawn as a closed outline."""
    out, back = [], []
    for i, (x, y) in enumerate(pts):
        px, py = pts[max(i - 1, 0)]
        nx, ny = pts[min(i + 1, len(pts) - 1)]
        dx, dy = nx - px, ny - py
        length = math.hypot(dx, dy) or 1
        ox, oy = -dy / length * t / 2, dx / length * t / 2
        out.append((x + ox, y + oy))
        back.append((x - ox, y - oy))
    poly(pen, out + list(reversed(back)))


# --------------------------------------------------------------------------
# The icons. Each fills the character box; shapes are chosen to stay legible
# at 12 px, where fine detail disappears.
# --------------------------------------------------------------------------


def cpu(pen):
    """A chip: square body with pins down all four sides."""
    inset = 78
    frame(pen, LEFT + inset, BOTTOM + inset, RIGHT - inset, TOP - inset)
    rect(pen, MID_X - 70, MID_Y - 70, MID_X + 70, MID_Y + 70)
    pin, plen = 46, inset
    for frac in (0.28, 0.5, 0.72):
        y = BOTTOM + inset + (HEIGHT - 2 * inset) * frac - pin / 2
        rect(pen, LEFT, y, LEFT + plen, y + pin)
        rect(pen, RIGHT - plen, y, RIGHT, y + pin)
        x = LEFT + inset + (WIDTH - 2 * inset) * frac - pin / 2
        rect(pen, x, BOTTOM, x + pin, BOTTOM + plen)
        rect(pen, x, TOP - plen, x + pin, TOP)


def memory(pen):
    """A RAM stick: a plain body with contact pins under it.

    Kept deliberately bare. A notch in the body — which real modules have —
    breaks the outline in two at 12 px and the icon stops reading as one
    object, so the body stays a clean rectangle.
    """
    body_bottom = BOTTOM + 175
    body_left = LEFT + 20
    body_right = RIGHT - 20
    frame(pen, body_left, body_bottom, body_right, TOP - 25)

    # Four pins, spaced symmetrically about the centre.
    pins, pin_w = 4, 55
    span = (body_right - body_left) - 2 * 40
    step = span / (pins - 1)
    for i in range(pins):
        x = body_left + 40 + i * step - pin_w / 2
        rect(pen, x, BOTTOM, x + pin_w, body_bottom)


def temp(pen):
    """A thermometer: bulb, stem, and scale marks that widen it.

    A bare bulb-and-stem is far narrower than a character — barely half the
    cell — which reads as a thin sliver next to the digits. The marks fill the
    width without changing what the icon says.
    """
    bulb_r = 145
    bulb_cy = BOTTOM + bulb_r + 5
    stem_w = 62
    disc(pen, MID_X, bulb_cy, bulb_r)
    rect(pen, MID_X - stem_w, bulb_cy, MID_X + stem_w, TOP - stem_w)
    disc(pen, MID_X, TOP - stem_w, stem_w)
    # Scale marks on the right of the stem.
    for i in range(3):
        y = bulb_cy + 130 + i * 125
        rect(pen, MID_X + stem_w + 45, y, RIGHT, y + 52)


def freq(pen):
    """A lightning bolt: speed, and it survives being tiny."""
    poly(pen, [
        (MID_X + 130, TOP),
        (LEFT + 60, MID_Y + 30),
        (MID_X - 10, MID_Y + 30),
        (MID_X - 120, BOTTOM),
        (RIGHT - 60, MID_Y - 10),
        (MID_X + 20, MID_Y - 10),
    ])


def wifi(pen):
    """Three arcs and a dot, stacked to use the full height."""
    cy = BOTTOM + 70
    # The outermost arc must stay inside the box: its half-chord, plus half the
    # stroke, is what sets the sweep. Solving for the angle keeps it exact.
    outer = 430
    max_half = WIDTH / 2 - STROKE / 2
    spread = math.degrees(math.asin(min(1.0, max_half / outer)))
    for i, r in enumerate((outer * 0.36, outer * 0.68, outer)):
        pts = arc_points(
            MID_X, cy, r, math.radians(90 - spread), math.radians(90 + spread), 16
        )
        band(pen, pts, STROKE - 6 if i else STROKE)
    disc(pen, MID_X, cy + 30, 62)


def bluetooth(pen):
    """The runic B: a vertical stem crossed by two mirrored triangles.

    The rune is a tall, narrow mark — that is its nature, not a squashing — so
    it takes the full height and about half the width. Drawn as three separate
    strokes meeting at the waist, which keeps the joints thin: a single
    continuous path piled thickness up where it doubled back.
    """
    top = TOP - 15
    bottom = BOTTOM + 15
    mid = (top + bottom) / 2
    stem_x = MID_X - 45
    tip_x = MID_X + 115

    band(pen, [(stem_x, bottom), (stem_x, top)], STROKE)
    # Upper triangle: waist -> top-right tip -> stem top.
    band(pen, [(stem_x - 120, mid + 165), (tip_x, mid + 90), (stem_x, top)], STROKE)
    # Lower triangle, mirrored.
    band(pen, [(stem_x - 120, mid - 165), (tip_x, mid - 90), (stem_x, bottom)], STROKE)


def headphones(pen):
    """A headband with two ear cups."""
    pts = arc_points(MID_X, MID_Y - 20, 195, math.radians(20), math.radians(160), 20)
    band(pen, pts, STROKE)
    cup_w, cup_h = 105, 210
    rect(pen, LEFT + 10, BOTTOM + 60, LEFT + 10 + cup_w, BOTTOM + 60 + cup_h)
    rect(pen, RIGHT - 10 - cup_w, BOTTOM + 60, RIGHT - 10, BOTTOM + 60 + cup_h)


def battery(pen):
    """Upright battery with a cap and a charge level.

    Vertical because the bar is short and wide: a vertical shape uses the
    height available, where the horizontal Nerd Font one wasted width.
    """
    cap_w, cap_h = 170, 52
    body_top = TOP - cap_h
    rect(pen, MID_X - cap_w / 2, body_top, MID_X + cap_w / 2, TOP)
    frame(pen, LEFT + 45, BOTTOM, RIGHT - 45, body_top - 12)
    inset = STROKE + 30
    rect(pen, LEFT + 45 + inset, BOTTOM + inset,
         RIGHT - 45 - inset, BOTTOM + (body_top - BOTTOM) * 0.5)


def power(pen):
    """A plug: two prongs over a body."""
    rect(pen, LEFT + 100, TOP - 210, LEFT + 162, TOP)
    rect(pen, RIGHT - 162, TOP - 210, RIGHT - 100, TOP)
    frame(pen, LEFT + 40, BOTTOM + 150, RIGHT - 40, TOP - 210)
    rect(pen, MID_X - 45, BOTTOM, MID_X + 45, BOTTOM + 150)


def brightness(pen):
    """A sun: ring plus rays."""
    ring(pen, MID_X, MID_Y, 130, STROKE)
    for i in range(8):
        a = math.radians(i * 45)
        x0, y0 = MID_X + 175 * math.cos(a), MID_Y + 175 * math.sin(a)
        x1, y1 = MID_X + 250 * math.cos(a), MID_Y + 250 * math.sin(a)
        band(pen, [(x0, y0), (x1, y1)], STROKE - 8)


def clock(pen):
    """A clock face with two hands."""
    # A circle can only be as wide as the box, so its height follows: this is
    # the one shape that cannot fill the band vertically without distorting.
    r = WIDTH / 2
    ring(pen, MID_X, MID_Y, r, STROKE)
    band(pen, [(MID_X, MID_Y), (MID_X, MID_Y + r * 0.62)], STROKE - 10)
    band(pen, [(MID_X, MID_Y), (MID_X + r * 0.48, MID_Y)], STROKE - 10)


def bell(pen):
    """A bell: dome, rim and clapper."""
    dome = arc_points(MID_X, MID_Y + 40, 175, math.radians(0), math.radians(180), 18)
    pts = [(MID_X + 175, BOTTOM + 150)] + dome + [(MID_X - 175, BOTTOM + 150)]
    poly(pen, list(reversed(pts)))
    rect(pen, LEFT + 10, BOTTOM + 90, RIGHT - 10, BOTTOM + 150)
    disc(pen, MID_X, BOTTOM + 45, 58)


def cloud(pen):
    """A cloud, drawn as an explicit outline rather than merged circles.

    Every attempt at unioning discs ended up either a house (lobe outlines
    cutting a rectangle's corners into a roof) or a plain dome (a big centre
    swallowing its neighbours). Tracing the silhouette by hand — up the left
    shoulder, over three bumps, down the right — gives the bumps directly and
    is the only version that reads as a cloud at 12 px.
    """
    # The cloud is wide and squat by nature, so its height comes from stacking
    # the middle bump well above the base rather than from bigger lobes: the
    # side ones are capped by the cell's width.
    # Proportions matter more than height here: a middle bump much taller and
    # narrower than its neighbours reads as a head over shoulders. Keeping the
    # three radii close, with the centre only slightly raised, is what makes it
    # a cloud — so the icon is squatter than the others by necessity.
    # Two forces pull against each other: bumps close together merge into one
    # dome, while a centre much taller than the sides reads as a head over
    # shoulders. What works is pushing the side bumps right out to the edges
    # and keeping all three radii similar.
    base = BOTTOM + 120
    bumps = [
        (LEFT + 95, base + 130, 95),
        (MID_X, base + 200, 130),
        (RIGHT - 95, base + 125, 95),
    ]

    pts = [(LEFT, base)]
    for cx, cy, r in bumps:
        # Half-circle over each bump, from its left edge to its right. The
        # centre is `cy`, so the apex reaches cy + r — writing the sine term
        # relative to `cy - r` flattens the whole cloud, which is what an
        # earlier version did.
        for step in range(17):
            a = math.pi - math.pi * step / 16
            y = cy + r * math.sin(a)
            pts.append((cx + r * math.cos(a), max(base, y)))
    pts.append((RIGHT, base))
    poly(pen, pts)


def clipboard(pen):
    """A clipboard: board with a clip on top and two written lines."""
    frame(pen, LEFT + 20, BOTTOM, RIGHT - 20, TOP - 90)
    rect(pen, MID_X - 105, TOP - 130, MID_X + 105, TOP)
    for i in range(2):
        y = BOTTOM + 150 + i * 150
        rect(pen, LEFT + 110, y, RIGHT - 110, y + 52)


def music(pen):
    """A quaver: note head, stem and flag.

    The head is an oval leaning left, as engraved music draws it — a plain
    circle reads as a full note. Kept to the lower half so the stem has the
    height to be seen at 12 px.
    """
    r = WIDTH * 0.27
    cx, cy = LEFT + r + 20, BOTTOM + r + 40
    disc(pen, cx, cy, r)
    # Stem up the right side of the head, stopping short of the cap height so
    # the flag has room.
    stem_x = cx + r - STROKE * 0.35
    rect(pen, stem_x, cy, stem_x + STROKE, TOP)
    # The flag: a wedge off the top of the stem, angled down and right.
    poly(pen, [
        (stem_x + STROKE, TOP),
        (RIGHT, TOP - 150),
        (RIGHT, TOP - 260),
        (stem_x + STROKE, TOP - 150),
    ])


def camera(pen):
    """A camera: body with a raised viewfinder hump and a lens.

    At 12 px the lens has to be a ring rather than a filled disc, or the body
    reads as a solid block; and the hump has to break the top edge, which is
    what separates a camera from the clipboard's rectangle.
    """
    body_top = TOP - 130
    # The hump sits over the left half, the way a viewfinder does.
    rect(pen, LEFT + 90, body_top, LEFT + 230, body_top + 90)
    frame(pen, LEFT, BOTTOM + 60, RIGHT, body_top)
    r = (body_top - BOTTOM - 60) / 2 - 55
    ring(pen, MID_X, (BOTTOM + 60 + body_top) / 2, r)


def blank(pen):
    """A blanked screen: a filled rectangle, the widget's whole meaning."""
    rect(pen, LEFT + 20, BOTTOM + 90, RIGHT - 20, TOP - 90)


def moon(pen):
    """A crescent, built as one closed contour.

    Subtracting a circle from another does not work here: TrueType fills by
    winding, so a bite that extends past the disc's edge either adds to it or
    leaves a ring, depending on direction. Tracing the crescent's own outline —
    down the outer arc, back up the inner one — sidesteps the question.
    """
    r_out = WIDTH / 2
    cx, cy = MID_X, MID_Y
    # Inner arc: a larger circle, offset right, whose edge cuts the disc.
    # A deeper bite makes a thinner crescent, and a crescent much narrower
    # than the cell reads as a sliver next to the digits: keep the cut shallow
    # enough that the shape still spans most of the box.
    r_in = r_out * 1.15
    ix, iy = cx + r_out * 0.98, cy + r_out * 0.12

    # Where the two circles meet, in angles measured from each centre.
    d = math.hypot(ix - cx, iy - cy)
    a = (d * d - r_in * r_in + r_out * r_out) / (2 * d)
    h = math.sqrt(max(0.0, r_out * r_out - a * a))
    base = math.atan2(iy - cy, ix - cx)
    half = math.atan2(h, a)

    # Outer arc: the long way round the disc, away from the bite.
    outer = arc_points(cx, cy, r_out, base + half, base + 2 * math.pi - half, 30)
    # Inner arc: back along the bite's edge between the same two points.
    p0, p1 = outer[-1], outer[0]
    a0 = math.atan2(p0[1] - iy, p0[0] - ix)
    a1 = math.atan2(p1[1] - iy, p1[0] - ix)
    if a1 < a0:
        a1 += 2 * math.pi
    # Walk the inner arc the *short* way between the two intersections, so it
    # bulges into the disc and hollows it out. Taking the long way traces the
    # far side of the bite circle, which bulges outward and fattens the shape.
    if a1 - a0 > math.pi:
        a1 -= 2 * math.pi
    inner = arc_points(ix, iy, r_in, a0, a1, 24)

    poly(pen, list(reversed(outer + inner)))


def lid(pen):
    """A laptop: screen over a base."""
    frame(pen, LEFT + 55, MID_Y - 40, RIGHT - 55, TOP - 40)
    rect(pen, LEFT, BOTTOM + 60, RIGHT, MID_Y - 100)


def volume(pen):
    """A speaker cone with one wave."""
    poly(pen, [
        (LEFT + 30, MID_Y - 95),
        (LEFT + 30, MID_Y + 95),
        (LEFT + 130, MID_Y + 95),
        (LEFT + 250, TOP - 40),
        (LEFT + 250, BOTTOM + 40),
        (LEFT + 130, MID_Y - 95),
    ])
    band(pen, arc_points(LEFT + 250, MID_Y, 155, math.radians(-55), math.radians(55), 14), STROKE - 12)


def mic(pen):
    """A microphone: capsule on a stand."""
    cap_w = 130
    poly(pen, list(reversed(
        arc_points(MID_X, TOP - 150, cap_w / 2, 0, math.pi, 14)
        + [(MID_X - cap_w / 2, MID_Y + 20), (MID_X + cap_w / 2, MID_Y + 20)]
    )))
    band(pen, arc_points(MID_X, MID_Y + 20, 175, math.radians(200), math.radians(340), 16), STROKE - 10)
    band(pen, [(MID_X, MID_Y - 155), (MID_X, BOTTOM + 45)], STROKE - 10)
    band(pen, [(MID_X - 130, BOTTOM + 45), (MID_X + 130, BOTTOM + 45)], STROKE - 10)


# Codepoint -> drawing function. Keep in sync with src/icons.rs; the tests
# cross-check that every icon the bar uses is present here.
ICONS = {
    0xF4BC: cpu,
    0xEFC5: memory,
    0xF2C9: temp,
    0xF0E4: freq,
    0xF1EB: wifi,
    0xF293: bluetooth,
    0xF025: headphones,
    0xF240: battery,
    0xF0E7: power,
    0xF185: brightness,
    0xF017: clock,
    0xF0F3: bell,
    0xF1FA: cloud,
    0xF0C5: clipboard,
    0xF04D: blank,
    0xF186: moon,
    0xF0AB: lid,
    0xF028: volume,
    0xF130: mic,
    0xF030: camera,
    0xF001: music,
}


def draw_into(font_path, out_path):
    font = TTFont(font_path)
    cmap = font.getBestCmap()
    cell = font["hmtx"][cmap[ord("M")]][0]

    for cp, draw in ICONS.items():
        name = f"drawn_{cp:04X}"
        pen = TTGlyphPen(None)
        draw(pen)
        font["glyf"][name] = pen.glyph()
        font["hmtx"][name] = (cell, 0)
        for table in font["cmap"].tables:
            if table.isUnicode():
                table.cmap[cp] = name

    font["maxp"].numGlyphs = len(font.getGlyphOrder())
    font.save(out_path)
    print(f"  drew {len(ICONS)} icons to fit a {cell}-unit cell")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("usage: draw-icons.py <font-in> <font-out>", file=sys.stderr)
        sys.exit(1)
    draw_into(sys.argv[1], sys.argv[2])
