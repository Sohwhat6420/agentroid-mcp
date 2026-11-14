"""Shared helpers for the README art: outlined text, the ghost-glass card, the two themes.

Adapted from tools/koi/koilib.py in github.com/Sohwhat6420/Sohwhat6420 (same author), so the README's
images match the profile's cards. Only the parts the README art uses are kept.

Everything is drawn as vector paths, so the SVGs need no fonts or network at render time (GitHub
serves README images with a strict CSP, so nothing may be fetched from inside them). Fonts, all SIL
OFL 1.1, are in fonts/ with their licences: Instrument Serif, Hanken Grotesk, JetBrains Mono, Antonio.
"""
import os

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.ttLib import TTFont

FONT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fonts")

def f(v, nd=1):
    s = ("%." + str(nd) + "f") % v
    if "." in s:
        s = s.rstrip("0").rstrip(".")
    if s in ("-0", ""):
        s = "0"
    return s



def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")




# ----------------------------------------------------------------------------------------------
# fonts: outlined glyphs shared through <defs>, laid out with GPOS pair kerning
# ----------------------------------------------------------------------------------------------
class Font:
    def __init__(self, key, filename):
        self.key = key
        self.tt = TTFont(os.path.join(FONT_DIR, filename))
        self.gs = self.tt.getGlyphSet()
        self.cmap = self.tt.getBestCmap()
        self.upm = self.tt["head"].unitsPerEm
        self.hmtx = self.tt["hmtx"]
        self._paths = {}
        self._pair = {}
        self._class = []
        self._load_kern()
        self.ids = {}

    def _load_kern(self):
        if "GPOS" not in self.tt:
            return
        gpos = self.tt["GPOS"].table
        idx = set()
        for fr in gpos.FeatureList.FeatureRecord:
            if fr.FeatureTag == "kern":
                idx.update(fr.Feature.LookupListIndex)
        for li in sorted(idx):
            lk = gpos.LookupList.Lookup[li]
            for st in lk.SubTable:
                if lk.LookupType == 9:
                    st = st.ExtSubTable
                if getattr(st, "LookupType", 2) != 2:
                    continue
                cov = st.Coverage.glyphs
                if st.Format == 1:
                    for g1, ps in zip(cov, st.PairSet):
                        for rec in ps.PairValueRecord:
                            v = getattr(rec.Value1, "XAdvance", 0) if rec.Value1 else 0
                            self._pair.setdefault((g1, rec.SecondGlyph), v or 0)
                elif st.Format == 2:
                    self._class.append((set(cov), st.ClassDef1.classDefs, st.ClassDef2.classDefs, st.Class1Record))

    def kern(self, a, b):
        if (a, b) in self._pair:
            return self._pair[(a, b)]
        for cov, cd1, cd2, recs in self._class:
            if a in cov:
                c1 = cd1.get(a, 0)
                c2 = cd2.get(b, 0)
                v = recs[c1].Class2Record[c2].Value1
                return (getattr(v, "XAdvance", 0) or 0) if v else 0
        return 0

    def glyph(self, ch):
        g = self.cmap.get(ord(ch))
        if g is None:
            raise KeyError("glyph missing for %r in %s" % (ch, self.key))
        return g

    def adv(self, g):
        return self.hmtx[g][0]

    def path(self, g):
        if g not in self._paths:
            pen = SVGPathPen(self.gs, ntos=lambda v: f(v, 0))
            self.gs[g].draw(pen)
            self._paths[g] = pen.getCommands()
        return self._paths[g]

    def layout(self, s, size, tracking=0.0):
        """Return [(glyph, x_in_font_units)], width_px. tracking is in em."""
        gl = [self.glyph(c) for c in s]
        tr = tracking * self.upm
        xs, cur = [], 0.0
        for i, g in enumerate(gl):
            xs.append(cur)
            cur += self.adv(g) + tr
            if i + 1 < len(gl):
                cur += self.kern(g, gl[i + 1])
        width = (cur - tr) * size / self.upm if gl else 0
        return list(zip(gl, xs)), width

    def width(self, s, size, tracking=0.0):
        return self.layout(s, size, tracking)[1]


FONTS = {}


def font(key):
    # the TV's widget type: Hanken Grotesk for the UI (labels 600 caps), Instrument Serif for display, Antonio for numbers
    files = {
        "serif": "InstrumentSerif-Regular.ttf",
        "sans": "HankenGrotesk-Regular.ttf",
        "sansm": "HankenGrotesk-Medium.ttf",
        "cap": "HankenGrotesk-SemiBold.ttf",
        "sansb": "HankenGrotesk-Bold.ttf",
        "num": "Antonio-Light.ttf",
        "mono": "JetBrainsMono-Regular.ttf",
        "monob": "JetBrainsMono-Bold.ttf",
    }
    if key not in FONTS:
        FONTS[key] = Font(key, files[key])
    return FONTS[key]


class Doc:
    """Collects defs (glyphs, gradients, patterns) and the CSS for one SVG."""

    def __init__(self, w, h, title, desc=""):
        self.w, self.h = w, h
        self.title, self.desc = title, desc
        self.defs = []
        self.glyphs = {}
        self.css = []
        self.body = []
        self._n = 0
        self.pad = (0, 0, 0, 0)   # (left, top, right, bottom): room outside the drawing for a card's soft shadow

    def uid(self, p="i"):
        self._n += 1
        return "%s%d" % (p, self._n)

    def gid(self, fnt, g):
        k = (fnt.key, g)
        if k not in self.glyphs:
            self.glyphs[k] = "%s%d" % (fnt.key[0] + fnt.key[-1], len(self.glyphs))
        return self.glyphs[k]

    def text(self, s, x, y, key, size, fill, tracking=0.0, anchor="start", cls=None, opacity=None, extra=""):
        fnt = font(key)
        gl, width = fnt.layout(s, size, tracking)
        if anchor == "end":
            x -= width
        elif anchor == "middle":
            x -= width / 2
        sc = size / fnt.upm
        uses = []
        for g, gx in gl:
            if not fnt.path(g):
                continue
            uses.append('<use href="#%s" x="%s"/>' % (self.gid(fnt, g), f(gx, 0)))
        attrs = ' fill="%s"' % fill
        if cls:
            attrs += ' class="%s"' % cls
        if opacity is not None:
            attrs += ' opacity="%s"' % f(opacity, 2)
        inner = '<g transform="translate(%s %s) scale(%s %s)">%s</g>' % (f(x, 2), f(y, 2), f(sc, 5), f(-sc, 5), "".join(uses))
        return '<g%s%s>%s</g>' % (attrs, extra, inner), width

    def render(self):
        glyph_defs = []
        for (key, g), gid in self.glyphs.items():
            glyph_defs.append('<path id="%s" d="%s"/>' % (gid, font(key).path(g)))
        css = "\n".join(self.css)
        pl, pt, pr, pb = self.pad
        vw, vh = self.w + pl + pr, self.h + pt + pb
        out = ['<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="%d" viewBox="%s %s %d %d" role="img" aria-labelledby="t d">'
               % (vw, vh, f(-pl), f(-pt), vw, vh),
               '<title id="t">%s</title><desc id="d">%s</desc>' % (esc(self.title), esc(self.desc)),
               "<style><![CDATA[\n%s\n]]></style>" % css,
               "<defs>%s%s</defs>" % ("".join(glyph_defs), "".join(self.defs)),
               "".join(self.body),
               "</svg>"]
        return "\n".join(out)


REDUCED = """
@media (prefers-reduced-motion: reduce){
  *{animation:none!important}
  .motion{display:none}
  .still{display:inline!important}
}
.still{display:none}
"""

# ----------------------------------------------------------------------------------------------
# themes
# ----------------------------------------------------------------------------------------------
#   The boxes are ghost glass in the TV widgets' own tokens (xiaomi-tv-revamp/tv-setup.html: the dock's --g-* glass,
#   the widget cards' type colours). A README image cannot blur the page behind it, so the frost is imitated: a
#   low-alpha neutral fill over GitHub's own page colour, the TV's sheen over the top half, a glow where the light
#   lands, a fine grain, a top-edge highlight, a hairline ring and a soft outer shadow. No pond colour is left.
#   g* keys feed glass(); text tokens are checked for WCAG AA against the glass composited on GitHub's page colours.
THEMES = {
    "dark": dict(
        name="dark",
        # type and accents, the TV's (tv-setup.html --g-ink, the cards' grey #b8bcc4, --accent, HOT, the low red;
        # page: #0d1117)
        text="#f4f3ef", dim="#b8bcc4", faint="#8f949c", gold="#d9b77a", accent="#ffb340", danger="#ff8a7e",
        ok="#a9c9ec", acc="#e9eef5",
        # ghost glass, the TV dock's: sheen .07 over the top half, top edge .42, rim .15, long shadow .52; the fill is
        # lifted a little off GitHub's flat page, since there is no painting behind it to frost
        gBase="rgba(28,32,39,.56)", gHiA="rgba(255,255,255,.07)", gHiB="rgba(255,255,255,.01)",
        gGlow="#ffffff", gGlowOp=0.035, gGrain="#ffffff", gGrainOp=0.05,
        gRing="#ffffff", gRingOp=0.15, gSheen="#ffffff", gSheenOp=0.42,
        gShadow="#000000", gShadowOp=0.52,
        # glass inside the glass: nodes, pills and lanes; solid pills for packets that cross a line
        card="#1f242b", cardHi="#1f242b", stroke="rgba(255,255,255,.12)",
        chip="rgba(255,255,255,.045)", chipStroke="rgba(255,255,255,.14)",
        line="#4b535d",
    ),
    "light": dict(
        name="light",
        # type and accents: the TV's light glass (--g-ink #14161a, --g-sub at .78, the darker status colours it uses
        # for 4.5:1 on white glass); page: #ffffff
        text="#14161a", dim="#4c5058", faint="#5d636c", gold="#7a5612", accent="#9a4f00", danger="#8f0012",
        ok="#2b5f91", acc="#3a4250",
        # ghost glass, the TV's light dock: warm off-white fill (246,245,241), sheen .18, top edge .88, its hairline
        # (20,22,26,.14) for the ring, shadow rgba(10,20,40,.3)
        gBase="rgba(243,242,238,.62)", gHiA="rgba(255,255,255,.18)", gHiB="rgba(232,234,238,.24)",
        gGlow="#ffffff", gGlowOp=0.6, gGrain="#14161a", gGrainOp=0.03,
        gRing="#14161a", gRingOp=0.11, gSheen="#ffffff", gSheenOp=0.88,
        gShadow="#0a1428", gShadowOp=0.22,
        card="#ffffff", cardHi="#ffffff", stroke="rgba(31,35,40,.10)",
        chip="rgba(255,255,255,.62)", chipStroke="rgba(31,35,40,.12)",
        line="#bcc3cc",
    ),
}

# room around a card for its outer shadow (left, top, right, bottom), in the card's own pixels
GLASS_PAD = (16, 6, 16, 26)


def glass(doc, T, x, y, w, h, r, shadow=True, sd=9.0):
    """One ghost-glass panel: soft shadow, translucent fill, light gradient and glow, grain, top-edge sheen and a
    hairline ring. Returns the SVG; the content goes on top. Every def gets this doc's own ids."""
    k = doc.uid("gl")
    rect = 'x="%s" y="%s" width="%s" height="%s" rx="%s"' % (f(x, 2), f(y, 2), f(w, 2), f(h, 2), f(r, 2))
    doc.defs.append('<clipPath id="%sc"><rect %s/></clipPath>' % (k, rect))
    # the TV's sheen: light over the top half, gone by 48 %, then a faint shade towards the foot
    doc.defs.append('<linearGradient id="%sg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="%s"/>'
                    '<stop offset=".48" stop-color="#fff" stop-opacity="0"/><stop offset="1" stop-color="%s"/></linearGradient>'
                    % (k, T["gHiA"], T["gHiB"]))
    doc.defs.append('<radialGradient id="%sr" gradientUnits="userSpaceOnUse" cx="%s" cy="%s" r="%s">'
                    '<stop offset="0" stop-color="%s" stop-opacity="%s"/><stop offset="1" stop-color="%s" stop-opacity="0"/>'
                    '</radialGradient>' % (k, f(x + w * 0.18, 1), f(y - h * 0.1, 1), f(max(w, h) * 0.75, 1),
                                           T["gGlow"], f(T["gGlowOp"], 3), T["gGlow"]))
    doc.defs.append('<linearGradient id="%ss" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="%s" stop-opacity="0"/>'
                    '<stop offset=".22" stop-color="%s" stop-opacity="%s"/><stop offset=".78" stop-color="%s" stop-opacity="%s"/>'
                    '<stop offset="1" stop-color="%s" stop-opacity="0"/></linearGradient>'
                    % (k, T["gSheen"], T["gSheen"], f(T["gSheenOp"], 3), T["gSheen"], f(T["gSheenOp"] * 0.55, 3), T["gSheen"]))
    # grain: static fractal noise, its red channel turned into a faint alpha
    gr, gg, gb = (int(T["gGrain"][i:i + 2], 16) / 255 for i in (1, 3, 5))
    doc.defs.append('<filter id="%sn" x="0" y="0" width="100%%" height="100%%" color-interpolation-filters="sRGB">'
                    '<feTurbulence type="fractalNoise" baseFrequency="1.15" numOctaves="2" seed="%d" stitchTiles="stitch"/>'
                    '<feColorMatrix values="0 0 0 0 %s  0 0 0 0 %s  0 0 0 0 %s  2.2 0 0 0 -.9"/></filter>'
                    % (k, len(doc.defs) % 97, f(gr, 3), f(gg, 3), f(gb, 3)))
    out = []
    if shadow:
        doc.defs.append('<filter id="%sb" x="-20%%" y="-20%%" width="140%%" height="160%%"><feGaussianBlur stdDeviation="%s"/></filter>'
                        % (k, f(sd, 2)))
        inset = min(10.0, w * 0.04)
        out.append('<rect x="%s" y="%s" width="%s" height="%s" rx="%s" fill="%s" fill-opacity="%s" filter="url(#%sb)"/>'
                   % (f(x + inset, 2), f(y + sd * 1.7, 2), f(w - 2 * inset, 2), f(h - sd * 2, 2), f(r, 2),
                      T["gShadow"], f(T["gShadowOp"], 3), k))
    out.append('<rect %s fill="%s"/>' % (rect, T["gBase"]))
    out.append('<g clip-path="url(#%sc)"><rect %s fill="url(#%sg)"/><rect %s fill="url(#%sr)"/>'
               '<rect %s opacity="%s" filter="url(#%sn)"/></g>'
               % (k, rect, k, rect, k, rect, f(T["gGrainOp"] * 1.6, 3), k))
    # the top edge catches the light: an inner line along the top, fading round the corners
    rr = max(0.0, r - 0.6)
    out.append('<path d="M%s %sa%s %s 0 0 1 %s-%sH%sa%s %s 0 0 1 %s %s" fill="none" stroke="url(#%ss)" stroke-width="1.1"/>'
               % (f(x + 0.6, 2), f(y + rr, 2), f(rr, 2), f(rr, 2), f(rr, 2), f(rr - 0.6, 2), f(x + w - rr - 0.6, 2),
                  f(rr, 2), f(rr, 2), f(rr, 2), f(rr - 0.6, 2), k))
    out.append('<rect x="%s" y="%s" width="%s" height="%s" rx="%s" fill="none" stroke="%s" stroke-opacity="%s"/>'
               % (f(x + 0.5, 2), f(y + 0.5, 2), f(w - 1, 2), f(h - 1, 2), f(max(0, r - 0.5), 2), T["gRing"], f(T["gRingOp"], 3)))
    return "".join(out)

# ----------------------------------------------------------------------------------------------
# the koi
# ----------------------------------------------------------------------------------------------
SLIM = 0.8  # body width relative to the first sketch: koi seen from above are long and slim
PROFILE = [(52, 0), (51, 3.2), (49, 6.4), (46, 9.2), (41, 12.2), (34, 14.8), (25, 16.6), (15, 17.4), (5, 17),
           (-5, 15.6), (-15, 13.4), (-25, 10.6), (-33, 8), (-40, 6), (-45, 4.8), (-48, 4.3)]

