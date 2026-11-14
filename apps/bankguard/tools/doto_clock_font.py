"""Build res/font/doto_clock.ttf (DotoClock): Doto's round, bold dot (ROND 100, wght 900) laid on the classic 5x7
LED grid that the owner's own clock uses, read dot by dot from a screenshot of it (14 Nov 2026).
Doto: https://github.com/oliverlalan/Doto (SIL OFL 1.1). Needs fontTools; Pillow only for the optional sheet.

- Proportional, like the reference: a glyph's advance is its ink width plus one empty cell; a space is two cells.
- Digits, A-Z and ! , . : ? are drawn here from the bitmaps below; the lowercase stays Doto's own.
- No contextual alternates (Doto's calt swaps the colon). Renamed DotoClock, as the OFL asks of a modified version.
usage: python doto_clock_font.py <Doto[ROND,wght].ttf> <out.ttf> [sheet.png]"""
import sys

from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools import subset

SRC, OUT = sys.argv[1], sys.argv[2]
TEXT = "0123456789:.,!? ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"

# Rows top to bottom; an 8th row is below the baseline (the comma's tail). From the reference: 0, 1, 4, 6, 7, the
# colon, the comma, H E L O !, S A T N V. The rest are the standard 5x7 LED shapes in the same style.
B = {
    "0": ".###.|#...#|#...#|#...#|#...#|#...#|.###.",
    "1": ".#.|##.|.#.|.#.|.#.|.#.|###",
    "2": ".###.|#...#|....#|...#.|..#..|.#...|#####",
    "3": "#####|...#.|..#..|...#.|....#|#...#|.###.",
    "4": "...#.|..##.|.#.#.|#..#.|#####|...#.|...#.",
    "5": "#####|#....|####.|....#|....#|#...#|.###.",
    "6": "..###|.#...|#....|####.|#...#|#...#|.###.",
    "7": "#####|....#|...#.|..#..|.#...|.#...|.#...",
    "8": ".###.|#...#|#...#|.###.|#...#|#...#|.###.",
    "9": ".###.|#...#|#...#|.####|....#|...#.|###..",
    ":": ".|.|#|#|.|#|#",
    ".": ".|.|.|.|.|.|#",
    ",": "..|..|..|..|..|..|.#|#.",
    "!": "#|#|#|#|#|.|#",
    "?": ".###.|#...#|....#|...#.|..#..|.....|..#..",
    "A": ".###.|#...#|#...#|#...#|#####|#...#|#...#",
    "B": "####.|#...#|#...#|####.|#...#|#...#|####.",
    "C": ".###.|#...#|#....|#....|#....|#...#|.###.",
    "D": "###..|#..#.|#...#|#...#|#...#|#..#.|###..",
    "E": "#####|#....|#....|####.|#....|#....|#####",
    "F": "#####|#....|#....|####.|#....|#....|#....",
    "G": ".###.|#...#|#....|#.###|#...#|#...#|.####",
    "H": "#...#|#...#|#...#|#####|#...#|#...#|#...#",
    "I": "###|.#.|.#.|.#.|.#.|.#.|###",
    "J": "..###|...#.|...#.|...#.|...#.|#..#.|.##..",
    "K": "#...#|#..#.|#.#..|##...|#.#..|#..#.|#...#",
    "L": "#....|#....|#....|#....|#....|#....|#####",
    "M": "#...#|##.##|#.#.#|#.#.#|#...#|#...#|#...#",
    "N": "#...#|#...#|##..#|#.#.#|#..##|#...#|#...#",
    "O": ".###.|#...#|#...#|#...#|#...#|#...#|.###.",
    "P": "####.|#...#|#...#|####.|#....|#....|#....",
    "Q": ".###.|#...#|#...#|#...#|#.#.#|#..#.|.##.#",
    "R": "####.|#...#|#...#|####.|#.#..|#..#.|#...#",
    "S": ".####|#....|#....|.###.|....#|....#|####.",
    "T": "#####|..#..|..#..|..#..|..#..|..#..|..#..",
    "U": "#...#|#...#|#...#|#...#|#...#|#...#|.###.",
    "V": "#...#|#...#|#...#|#...#|#...#|.#.#.|..#..",
    "W": "#...#|#...#|#...#|#.#.#|#.#.#|#.#.#|.#.#.",
    "X": "#...#|#...#|.#.#.|..#..|.#.#.|#...#|#...#",
    "Y": "#...#|#...#|#...#|.#.#.|..#..|..#..|..#..",
    "Z": "#####|....#|...#.|..#..|.#...|#....|#####",
}

f = instantiateVariableFont(TTFont(SRC), {"ROND": 100, "wght": 900})
cmap = f.getBestCmap()
glyf = f["glyf"]
one = cmap[ord("1")]
g1 = glyf[one]
g1.recalcBounds(glyf)
coords, ends, flags = g1.getCoordinates(glyf)
first = list(coords[: ends[0] + 1])  # one dot of Doto's "1"
fl = [bool(v & 1) for v in flags[: ends[0] + 1]]
dx0, dy0 = min(p[0] for p in first), min(p[1] for p in first)
dot = max(p[1] for p in first) - dy0
base = g1.yMin  # Doto's digits sit 5 units above the baseline
P = (g1.yMax - g1.yMin - dot) / 6.0  # seven rows of dots
pad = (P - dot) / 2.0  # a dot sits centred in its cell


def draw_dot(pen, x, y):
    pts = [(px - dx0 + x, py - dy0 + y) for px, py in first]
    start = fl.index(True)
    seq, on = pts[start:] + pts[:start], fl[start:] + fl[:start]
    pen.moveTo(seq[0])
    buf = []
    for p, o in zip(seq[1:], on[1:]):
        if o:
            if buf:
                pen.qCurveTo(*buf, p)
                buf = []
            else:
                pen.lineTo(p)
        else:
            buf.append(p)
    if buf:
        pen.qCurveTo(*buf, seq[0])
    pen.closePath()


gs = f.getGlyphSet()
for ch, bits in B.items():
    rows = bits.split("|")
    w = len(rows[0])
    pen = TTGlyphPen(gs)
    for r, line in enumerate(rows):
        for c, v in enumerate(line):
            if v == "#":
                draw_dot(pen, c * P + pad, base + (6 - r) * P)
    name = cmap[ord(ch)]
    glyf[name] = pen.glyph()
    glyf[name].recalcBounds(glyf)
    f["hmtx"][name] = (round((w + 1) * P), glyf[name].xMin)
space = cmap[ord(" ")]
f["hmtx"][space] = (round(2 * P), 0)

for rec_ in f["name"].names:
    if rec_.nameID in (1, 4, 16):
        rec_.string = "DotoClock"
    elif rec_.nameID == 6:
        rec_.string = "DotoClock-Black"
    elif rec_.nameID == 3:
        rec_.string = "DotoClock-Black;subset for Agentroid Companion"

opts = subset.Options()
opts.name_IDs = ["*"]
opts.notdef_outline = True
opts.layout_features = []
sub = subset.Subsetter(opts)
sub.populate(text=TEXT)
sub.subset(f)
f.save(OUT)
print("saved", OUT, "pitch", round(P, 1), "dot", dot)

if len(sys.argv) > 3:
    from PIL import Image, ImageDraw, ImageFont
    im = Image.new("RGB", (1400, 420), (12, 12, 14))
    d = ImageDraw.Draw(im)
    for i, t in enumerate(["0123456789 :.,!?", "ABCDEFGHIJKLM", "NOPQRSTUVWXYZ", "HELLO! SAT, 14 NOV"]):
        d.text((20, 20 + i * 100), t, font=ImageFont.truetype(OUT, 80), fill=(255, 255, 255))
    im.save(sys.argv[3])
