"""Builds the README's three drawings, each in a dark and a light theme.

    pip install fonttools pillow
    python docs/media/src/build_readme_art_r2.py

Writes, into docs/media/:
    banner-dark.svg  banner-light.svg     the banner at the top (1280 x 640)
    tokens-dark.svg  tokens-light.svg     what one look at the phone costs (1280 x 938)
    how-dark.svg     how-light.svg        how the parts connect (1280 x 392)

The banner is built in layers so its background can be swapped
    layer-background   ONE <image>: src/banner-bg-dark.jpg or banner-bg-light.jpg, 1280 x 640. Replace the
                       file and run this script again. If the file is missing a plain placeholder is made.
    layer-scrim        a soft shade under the text column so any background stays readable (SCRIM below)
    layer-phone        the real screenshot in a thin bezel; PHONE below is the screen's pixel box
    layer-steps        what you ask and what the AI does, beside the phone
    layer-text         the name, the headline, the one-line pitch and the one number (strings in COPY below)

What is real
- The phone screen is a real screenshot of a Xiaomi POCO X6 Pro home screen taken 14 Nov 2026
  (src/phone-home-480.jpg), embedded as a data URI: GitHub blocks anything fetched from inside an SVG.
- Every token figure is in MEASURED. They were counted on 14 Nov 2026 with Anthropic's count_tokens
  endpoint on Claude Opus 5.5 (minus the 8-token empty-message baseline) for one real screen, the POCO X6
  Pro home screen. The "fits" figures are arithmetic: 200,000 // tokens.
- The three steps beside the phone are an illustration of a session, not a recording; the recordings are
  the GIFs in docs/media/.

How it is drawn
- Text is outlined from the fonts in fonts/ (all SIL OFL 1.1), so nothing depends on the reader's fonts.
- One accent colour (Android green) marks Agentroid everywhere; everything else is neutral.
- Motion is CSS @keyframes inside the SVG (GitHub strips scripts). The drawing with no animation applied
  is the finished picture, so prefers-reduced-motion and a still render both show the complete state.
- No child process is started and no browser is needed.
"""
import base64
import io
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from artlib import Doc, f, font  # noqa: E402

MEDIA = os.environ.get("ART_OUT") or os.path.dirname(HERE)
STILL = bool(os.environ.get("ART_STILL"))     # draw no hidden frames: for renderers that ignore clip paths
SHOT = os.path.join(HERE, "phone-home-480.jpg")
SHOT_W, SHOT_H = 480, 1067

# ----------------------------------------------------------------------------------------------
# the measured figures (tokens, Claude Opus 5.5, one real screen)
# ----------------------------------------------------------------------------------------------
WINDOW = 200_000
MEASURED = [
    dict(key="raw", ours=False, tokens=11565, fits=17,
         label="Android’s raw screen description", sub="uiautomator dump: the XML Android hands any app",
         cap=("raw screen", "description")),
    dict(key="shot", ours=False, tokens=3867, fits=51,
         label="A screenshot of the phone", sub="1220 × 2712 PNG",
         cap=("a screenshot", "")),
    dict(key="full", ours=True, tokens=1241, fits=161,
         label="Agentroid: the full list", sub="every element; tappable ones get a short id",
         cap=("Agentroid,", "full list")),
    dict(key="short", ours=True, tokens=229, fits=873,
         label="Agentroid: the short answer", sub="the default: the first 400 characters",
         cap=("Agentroid, short answer (the default)", "")),
    dict(key="diff", ours=True, tokens=35, fits=None,
         label="Agentroid: “what changed?”", sub="asked again when nothing had",
         cap=None),
]
for _r in MEASURED:
    assert _r["fits"] is None or WINDOW // _r["tokens"] == _r["fits"], _r
BY = {r["key"]: r for r in MEASURED}
RATIO = BY["raw"]["tokens"] // BY["short"]["tokens"]         # 50 (50.5, said as "about 50")
RATIO_FULL = BY["raw"]["tokens"] // BY["full"]["tokens"]     # 9 (9.3)
assert (RATIO, RATIO_FULL) == (50, 9)

# the banner's words: edit here, run again
COPY = dict(
    name="Agentroid",
    headline=("Give your AI agent", "a real Android phone."),
    pitch="It reads the screen, taps, types and swipes.",
    upto="UP TO",
    stat=("fewer tokens per look", "than Android’s raw screen description"),
    foot="Apache-2.0 · MCP server",
    ask="open Claude on my phone",
    shot="Real screenshot: POCO X6 Pro, 14 Nov 2026 · phone and desk illustrated",
    recorded="one home screen, recorded 20 Oct 2026",
)
# where the real screen sits in the banner's 1280 x 640 box: x, y, width, height (it runs off the foot)
PHONE = dict(x=910.0, y=60.0, w=336.0, h=336.0 * SHOT_H / SHOT_W, bezel=7.0)
SCRIM = 0.72        # 0 = no shade under the text column, 1 = solid
# photo mode: build_banner_photo.py keys the real screen into a photo-style background and writes where the screen
# landed. When that file exists the phone IS the background picture, and no bezel is drawn here.
PHOTO = {}
if os.path.exists(os.path.join(HERE, "banner-photo.json")):
    import json
    with open(os.path.join(HERE, "banner-photo.json"), encoding="utf-8") as _fh:
        PHOTO = json.load(_fh)


def num(n):
    return "{:,}".format(n)


# ----------------------------------------------------------------------------------------------
# themes: green-black ink or warm paper, neutrals, and ONE accent (Android green)
# ----------------------------------------------------------------------------------------------
THEMES = {
    "dark": dict(
        name="dark", bg="#0A1410", bg2="#0E1E17", ink="#F1F5F1", dim="#A8B5AC", faint="#7F8D85",
        accent="#3DDC84", onAccent="#062313", neutral="#5B6860", surface="#13221B", stroke="#2A4034",
        track="#14221B", shadow="#000000", shadowOp=0.085, rim="#ffffff", rimOp=0.26,
        grain="#ffffff", grainOp=0.06, glow=0.30,
    ),
    "light": dict(
        name="light", bg="#F5F2E8", bg2="#EAF1E0", ink="#0F1B14", dim="#45524A", faint="#5E6A62",
        accent="#0B7D45", onAccent="#FFFFFF", neutral="#A7B0A6", surface="#FFFFFF", stroke="#D3DBCC",
        track="#E7EBDD", shadow="#1B2A20", shadowOp=0.05, rim="#000000", rimOp=0.35,
        grain="#0F1B14", grainOp=0.04, glow=0.40,
    ),
}

REDUCED = "@media (prefers-reduced-motion: reduce){*{animation:none!important}}"


def rgb(h):
    return tuple(int(h[i:i + 2], 16) for i in (1, 3, 5))


def pct(t, loop):
    return f(100.0 * t / loop, 2) + "%"


def jpeg_uri(data):
    return "data:image/jpeg;base64," + base64.b64encode(data).decode("ascii")


def read(path):
    with open(path, "rb") as fh:
        return fh.read()


def placeholder_bg(path, T):
    """A plain stand-in background: the theme's ink or paper with one soft green light behind the phone."""
    import numpy as np
    from PIL import Image
    W, H = 1280, 640
    yy, xx = np.mgrid[0:H, 0:W].astype("float32")
    a, b, g = (np.array(rgb(T[k]), dtype="float32") for k in ("bg", "bg2", "accent"))
    t = ((xx / W + yy / H) / 2)[..., None]
    img = a * (1 - t) + b * t
    for cx, cy, r, op in ((1080, 330, 520, T["glow"]), (640, 760, 420, T["glow"] * 0.45)):
        dd = np.clip(1 - np.hypot(xx - cx, yy - cy) / r, 0, 1) ** 2
        img = img * (1 - dd[..., None] * op) + g * dd[..., None] * op
    img += np.random.default_rng(7).normal(0, 1.1, img.shape)      # dither, so the JPEG does not band
    Image.fromarray(np.clip(img, 0, 255).astype("uint8")).save(path, quality=80, optimize=True, progressive=True)


def thumb_uri(width):
    from PIL import Image
    im = Image.open(SHOT).convert("RGB")
    im = im.resize((width, round(width * SHOT_H / SHOT_W)), Image.LANCZOS)
    buf = io.BytesIO()
    im.save(buf, "JPEG", quality=82, optimize=True)
    return jpeg_uri(buf.getvalue())


class Art(Doc):
    """artlib's Doc, with the shapes these drawings share."""

    def t(self, s, x, y, key, size, fill, **kw):
        return self.text(s, x, y, key, size, fill, **kw)[0]

    def tw(self, s, key, size, tracking=0.0):
        return font(key).width(s, size, tracking)

    def card(self, w, h):
        self.defs.append('<clipPath id="card"><rect width="%d" height="%d" rx="28"/></clipPath>' % (w, h))

    def grain(self, T, w, h):
        k = self.uid("gr")
        gr, gg, gb = (v / 255 for v in rgb(T["grain"]))
        self.defs.append('<filter id="%s" x="0" y="0" width="100%%" height="100%%" color-interpolation-filters="sRGB">'
                         '<feTurbulence type="fractalNoise" baseFrequency=".9" numOctaves="2" seed="7" stitchTiles="stitch"/>'
                         '<feColorMatrix values="0 0 0 0 %s  0 0 0 0 %s  0 0 0 0 %s  1.9 0 0 0 -.8"/></filter>'
                         % (k, f(gr, 3), f(gg, 3), f(gb, 3)))
        # <!--fx--> marks what a renderer without SVG filters should skip
        return '<!--fx--><rect width="%d" height="%d" opacity="%s" filter="url(#%s)"/><!--/fx-->' % (w, h, f(T["grainOp"], 3), k)

    def glow(self, cx, cy, r, colour, op):
        k = self.uid("gl")
        self.defs.append('<radialGradient id="%s"><stop offset="0" stop-color="%s" stop-opacity="%s"/>'
                         '<stop offset="1" stop-color="%s" stop-opacity="0"/></radialGradient>' % (k, colour, f(op, 3), colour))
        return '<circle cx="%s" cy="%s" r="%s" fill="url(#%s)"/>' % (f(cx), f(cy), f(r), k)

    def soft_shadow(self, T, x, y, w, h, r, spread=54.0, drop=22.0, k=1.0):
        """A soft shadow from many faint stacked shapes: no filter, so it draws the same everywhere."""
        n = max(6, int(spread / 2.5))
        step = spread / n
        return "".join('<rect x="%s" y="%s" width="%s" height="%s" rx="%s" fill="%s" fill-opacity="%s"/>'
                       % (f(x - i * step), f(y - i * step + drop), f(w + 2 * i * step), f(h + 2 * i * step), f(r + i * step),
                          T["shadow"], f(T["shadowOp"] * 9.0 * k / n, 4)) for i in range(n))

    def roll(self, s, x, y, key, size, fill, cls, tracking=0.0, anchor="start", k=6):
        """A number whose digits count up into place. Returns (svg, width, travel): animate `.cls` from
        translateY(0) to translateY(-travel px). With no animation the finished number shows."""
        fnt = font(key)
        k = 1 if STILL else k
        gl, width = fnt.layout(s, size, tracking)
        if anchor == "end":
            x -= width
        sc, lh = size / fnt.upm, size * 1.2
        travel = (k - 1) * lh
        cid = self.uid("rc")
        self.defs.append('<clipPath id="%s"><rect x="%s" y="%s" width="%s" height="%s"/></clipPath>'
                         % (cid, f(x - 3), f(y - size * 0.9), f(width + 6), f(size * 1.14)))
        out = []
        for (g, gx), ch in zip(gl, s):
            px = x + gx * sc
            if not ch.isdigit():
                if fnt.path(g):
                    out.append('<use href="#%s" transform="translate(%s %s) scale(%s %s)"/>'
                               % (self.gid(fnt, g), f(px, 2), f(y, 2), f(sc, 5), f(-sc, 5)))
                continue
            adv = fnt.adv(g) * sc
            strip = []
            for j in range(k):
                gg = fnt.glyph(str((int(ch) - (k - 1 - j)) % 10))
                strip.append('<use href="#%s" transform="translate(%s %s) scale(%s %s)"/>'
                             % (self.gid(fnt, gg), f((adv - fnt.adv(gg) * sc) / 2, 2), f(j * lh, 2), f(sc, 5), f(-sc, 5)))
            out.append('<g transform="translate(%s %s)"><g class="%s" transform="translate(0 %s)">%s</g></g>'
                       % (f(px, 2), f(y, 2), cls, f(-travel, 2), "".join(strip)))
        return '<g fill="%s" clip-path="url(#%s)">%s</g>' % (fill, cid, "".join(out)), width, travel

    def typed(self, s, x, y, key, size, fill, cls, t0, step):
        """Text that types itself: each letter carries its own start time. Returns (svg, width)."""
        fnt = font(key)
        gl, width = fnt.layout(s, size)
        sc = size / fnt.upm
        out = ['<use class="%s" style="animation-delay:%ss" href="#%s" transform="translate(%s %s) scale(%s %s)"/>'
               % (cls, f(t0 + i * step, 2), self.gid(fnt, g), f(x + gx * sc, 2), f(y, 2), f(sc, 5), f(-sc, 5))
               for i, (g, gx) in enumerate(gl) if fnt.path(g)]
        return '<g fill="%s">%s</g>' % (fill, "".join(out)), width


# ----------------------------------------------------------------------------------------------
# the banner: 1280 x 640, in layers
# ----------------------------------------------------------------------------------------------
def banner(T):
    W, H = 1280, 640
    C = COPY
    d = Art(W, H, "Agentroid: give your AI agent a real Android phone",
            "A real Xiaomi POCO X6 Pro home screen, shown on a phone standing on a desk (the screen is a real screenshot; "
            "the phone body and desk are a generated picture). Beside it, an example: you ask the AI to open "
            "Claude on the phone; through Agentroid it looks at the screen, taps the Claude app, then asks only for "
            "what changed. One look costs up to %d times fewer tokens than Android's raw screen description: %s tokens "
            "for Agentroid's short answer and %s for its full list (%d times fewer), against %s, measured on one real "
            "home screen with Claude Opus 5.5."
            % (RATIO, num(BY["short"]["tokens"]), num(BY["full"]["tokens"]), RATIO_FULL, num(BY["raw"]["tokens"])))
    o = d.body
    d.card(W, H)
    o.append('<g clip-path="url(#card)">')

    # ---- layer-background: one swappable picture ----
    bg = os.path.join(HERE, "banner-bg-%s.jpg" % T["name"])
    if not os.path.exists(bg):
        placeholder_bg(bg, T)
    o.append('<rect width="%d" height="%d" fill="%s"/>' % (W, H, T["bg"]))
    o.append('<image id="layer-background" href="%s" width="%d" height="%d" preserveAspectRatio="xMidYMid slice"/>' % (jpeg_uri(read(bg)), W, H))

    # ---- layer-scrim ----
    if SCRIM:
        d.defs.append('<linearGradient id="scrim" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="%s" stop-opacity="%s"/>'
                      '<stop offset=".5" stop-color="%s" stop-opacity="%s"/><stop offset=".74" stop-color="%s" stop-opacity="0"/></linearGradient>'
                      % (T["bg"], f(SCRIM, 2), T["bg"], f(SCRIM * 0.9, 2), T["bg"]))
        o.append('<rect id="layer-scrim" width="%d" height="%d" fill="url(#scrim)"/>' % (W, H))

    # ---- layer-phone: the real screenshot in a thin bezel ----
    photo = PHOTO.get(T["name"])
    box = photo or PHONE
    sx, sy, sw, sh, bz = box["x"], box["y"], box["w"], box["h"], box["bezel"]
    S = sw / SHOT_W
    d.defs.append('<clipPath id="scr"><rect x="%s" y="%s" width="%s" height="%s" rx="%s"/></clipPath>' % (f(sx), f(sy), f(sw), f(sh), f(box.get("r", 30))))
    d.defs.append('<linearGradient id="scan" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="%s" stop-opacity="0"/>'
                  '<stop offset=".82" stop-color="%s" stop-opacity=".38"/><stop offset="1" stop-color="%s" stop-opacity="0"/></linearGradient>'
                  % (T["accent"], T["accent"], T["accent"]))
    o.append('<g id="layer-phone"><g%s>' % ("" if photo else ' class="phone"'))
    if not photo:
        o.append(d.soft_shadow(T, sx - bz, sy - bz, sw + 2 * bz, sh + 2 * bz, 37, spread=60, drop=24))
        o.append('<rect x="%s" y="%s" width="%s" height="%s" rx="37" fill="#0B0C0E"/>' % (f(sx - bz), f(sy - bz), f(sw + 2 * bz), f(sh + 2 * bz)))
        o.append('<rect x="%s" y="%s" width="%s" height="%s" rx="36.5" fill="none" stroke="%s" stroke-opacity="%s"/>'
                 % (f(sx - bz + 0.5), f(sy - bz + 0.5), f(sw + 2 * bz - 1), f(sh + 2 * bz - 1), T["rim"], f(T["rimOp"], 2)))
        o.append('<image href="%s" x="%s" y="%s" width="%s" height="%s" preserveAspectRatio="xMidYMid slice" clip-path="url(#scr)"/>'
                 % (jpeg_uri(read(SHOT)), f(sx), f(sy), f(sw), f(sh)))
    o.append('<g clip-path="url(#scr)"><rect class="scan" x="%s" y="%s" width="%s" height="140" fill="url(#scan)" opacity="0"/></g>'
             % (f(sx), f(sy - 140), f(sw)))
    tx, ty = sx + 71 * S, sy + 570 * S            # the Claude icon on the real screenshot
    rip = 24 if photo else 30                     # the tap ring, sized to the icon
    o.append('<circle class="rip p1" cx="%s" cy="%s" r="%s" fill="none" stroke="%s" stroke-width="3" opacity=".9"/>' % (f(tx), f(ty), f(rip), T["accent"] if T["name"] == "dark" else "#3DDC84"))
    o.append('<circle class="rip p2" cx="%s" cy="%s" r="%s" fill="none" stroke="%s" stroke-width="2" opacity="0"/>' % (f(tx), f(ty), f(rip), "#3DDC84"))
    o.append('<circle class="tip" cx="%s" cy="%s" r="8.5" fill="#3DDC84" stroke="#fff" stroke-width="2.5"/>' % (f(tx), f(ty)))
    o.append('</g></g>')

    # ---- layer-steps: what you ask, what the AI does ----
    CW, CH, GAP = 262.0, 44.0, 12.0
    x1 = sx - bz - (16 if photo else 3)
    x0 = x1 - CW
    L = 10.0
    P = lambda t: pct(t, L)  # noqa: E731
    HOLD = -6.2                                   # start mid-loop, with everything showing
    o.append('<g id="layer-steps">')
    c2y = ty - CH / 2                             # the tap step (the middle one) sits level with the icon it taps
    ys = [c2y - (CH + GAP), c2y, c2y + (CH + GAP)]
    ay = ys[0] - 34 - CH - 28                     # the "you ask" box
    o.append('<g class="chip c9">')
    o.append(d.t("EXAMPLE · YOU ASK", x0 + 2, ay - 10, "cap", 12.5, T["faint"], tracking=0.14))
    o.append(d.soft_shadow(T, x0, ay, CW, CH, 13, spread=16, drop=7, k=0.5))
    o.append('<rect x="%s" y="%s" width="%s" height="%s" rx="13" fill="%s" stroke="%s"/>' % (f(x0), f(ay), f(CW), f(CH), T["surface"], T["stroke"]))
    ask, askw = d.typed(C["ask"], x0 + 18, ay + 27.5, "mono", 15, T["ink"], "ty", HOLD + 0.3, 0.05)
    o.append(ask)
    o.append('<g transform="translate(%s 0)"><g class="caret" transform="translate(%s 0)"><rect x="2" y="%s" width="2" height="19" fill="%s"/></g></g>'
             % (f(x0 + 18), f(askw, 1), f(ay + 12.5), T["accent"]))
    o.append('</g>')
    o.append('<g class="chip c0">%s</g>' % d.t("THE AI DOES", x0 + 2, ys[0] - 10, "cap", 12.5, T["faint"], tracking=0.14))
    # no token figures here: the measured ones belong to the recorded 20 Oct screen, not to this run
    steps = [("look", "reads the screen"),
             ("tap Claude", "done"),
             ("changed?", "only what moved")]
    for i, (lab, val) in enumerate(steps):
        cy = ys[i]
        o.append('<g class="chip c%d">' % i)
        o.append(d.soft_shadow(T, x0, cy, CW, CH, 13, spread=16, drop=7, k=0.5))
        o.append('<rect x="%s" y="%s" width="%s" height="%s" rx="13" fill="%s" stroke="%s"/>' % (f(x0), f(cy), f(CW), f(CH), T["surface"], T["stroke"]))
        o.append('<circle cx="%s" cy="%s" r="4.5" fill="%s"/>' % (f(x0 + 19), f(cy + CH / 2), T["accent"]))
        o.append(d.t(lab, x0 + 34, cy + 28.5, "cap", 17.5, T["ink"]))
        o.append(d.t(val, x1 - 16, cy + 27.5, "mono", 14, T["accent"], anchor="end"))
        if i == 1:
            o.append('<path d="M%s %sH%s" stroke="%s" stroke-width="2" stroke-dasharray="1 6" stroke-linecap="round" fill="none"/>'
                     % (f(x1 + 5), f(ty), f(tx - rip - 4), "#3DDC84"))
        o.append('</g>')
    o.append('<g class="in i3">%s</g>' % d.t(C["shot"], x1, 598, "mono", 14, T["faint"], anchor="end"))
    o.append('</g>')

    # ---- layer-text ----
    X0 = 72
    o.append('<g id="layer-text">')
    o.append('<g class="in i0"><rect x="%d" y="60" width="21" height="34" rx="6" fill="none" stroke="%s" stroke-width="3"/>'
             '<circle cx="%s" cy="77" r="4" fill="%s"/>%s</g>'
             % (X0 + 1, T["accent"], f(X0 + 11.5), T["accent"], d.t(C["name"], X0 + 36, 88, "sansb", 30, T["ink"], tracking=-0.015)))
    HS, HT = 58, -0.025
    for i, (s, col, by) in enumerate(((C["headline"][0], T["ink"], 176), (C["headline"][1], T["accent"], 242))):
        d.defs.append('<clipPath id="hl%d"><rect x="%d" y="%d" width="640" height="78"/></clipPath>' % (i, X0 - 8, by - 58))
        o.append('<g clip-path="url(#hl%d)"><g class="rise r%d">%s</g></g>' % (i, i, d.t(s, X0, by, "sansb", HS, col, tracking=HT)))
    o.append('<g class="in i1">%s</g>' % d.t(C["pitch"], X0, 300, "sans", 23, T["dim"]))
    o.append('<g class="in i2">')
    big, bw, travel = d.roll(str(RATIO), X0 - 4, 524, "num", 128, T["accent"], "cnt", k=8)
    o.append(big)
    mx, my, ms = X0 + bw + 8, 474, 17.0            # the multiplication sign, drawn to the numerals' weight
    o.append('<path d="M%s %sl%s %sM%s %sl%s %s" stroke="%s" stroke-width="8" fill="none"/>'
             % (f(mx), f(my - ms), f(2 * ms), f(2 * ms), f(mx + 2 * ms), f(my - ms), f(-2 * ms), f(2 * ms), T["accent"]))
    tx0 = mx + 2 * ms + 26
    o.append(d.t(C["stat"][0], tx0, 456, "cap", 27, T["ink"], tracking=-0.01))
    o.append(d.t(C["stat"][1], tx0, 489, "sans", 20, T["dim"]))
    o.append(d.t("%s → %s tokens short answer · %s full list (%d×)"
                 % (num(BY["raw"]["tokens"]), num(BY["short"]["tokens"]), num(BY["full"]["tokens"]), RATIO_FULL), tx0, 521, "mono", 14, T["faint"]))
    o.append(d.t(C["recorded"], tx0, 543, "mono", 14, T["faint"]))
    o.append(d.t(C["upto"], X0, 400, "cap", 12.5, T["faint"], tracking=0.14))
    o.append('</g>')
    o.append('<g class="in i3">%s</g>' % d.t(C["foot"], X0, 598, "mono", 14, T["faint"]))
    o.append('</g></g>')

    d.css.append("""
.rise{animation:rise 1s cubic-bezier(.16,1,.3,1) both}
.r1{animation-delay:.12s}
@keyframes rise{from{transform:translateY(80px)}to{transform:translateY(0)}}
.in{animation:fadeup .8s cubic-bezier(.16,1,.3,1) both}
.i1{animation-delay:.38s}.i2{animation-delay:.52s}.i3{animation-delay:.7s}
@keyframes fadeup{from{opacity:0;transform:translateY(14px)}to{opacity:1;transform:translateY(0)}}
.cnt{animation:cnt 1.5s cubic-bezier(.2,.7,.2,1) .6s both}
@keyframes cnt{from{transform:translateY(0)}to{transform:translateY(-%(travel)spx)}}
.phone{animation:phonein 1.2s cubic-bezier(.16,1,.3,1) both}
@keyframes phonein{from{opacity:0;transform:translateY(60px)}to{opacity:1;transform:translateY(0)}}
.chip,.scan,.rip,.tip,.caret{animation-duration:%(L)ss;animation-iteration-count:infinite;animation-delay:%(hold)ss}
.chip{animation-timing-function:cubic-bezier(.16,1,.3,1)}
.c9{animation-name:c9}.c0{animation-name:c0}.c1{animation-name:c1}.c2{animation-name:c2}
@keyframes c9{0%%{opacity:0}%(q1)s,%(o0)s{opacity:1}%(o1)s,100%%{opacity:0}}
@keyframes c0{0%%,%(a0)s{opacity:0;transform:translateX(-12px)}%(a1)s,%(o0)s{opacity:1;transform:translateX(0)}%(o1)s,100%%{opacity:0;transform:translateX(0)}}
@keyframes c1{0%%,%(b0)s{opacity:0;transform:translateX(-12px)}%(b1)s,%(o0)s{opacity:1;transform:translateX(0)}%(o1)s,100%%{opacity:0;transform:translateX(0)}}
@keyframes c2{0%%,%(c0)s{opacity:0;transform:translateX(-12px)}%(c1)s,%(o0)s{opacity:1;transform:translateX(0)}%(o1)s,100%%{opacity:0;transform:translateX(0)}}
.ty{animation:ty %(L)ss steps(1) infinite}
@keyframes ty{0%%{opacity:0}.5%%,88%%{opacity:1}93%%,100%%{opacity:0}}
.caret{animation-name:caret}
@keyframes caret{0%%,%(k0)s{transform:translateX(0);animation-timing-function:steps(%(kn)d,end)}%(k1)s,100%%{transform:translateX(%(kw)spx)}}
.caret rect{animation:blink 1s steps(2,start) infinite}
@keyframes blink{to{visibility:hidden}}
.scan{animation-name:scan;animation-timing-function:linear}
@keyframes scan{0%%,%(s0)s{opacity:0;transform:translateY(0)}%(s0b)s{opacity:1;transform:translateY(0)}%(s1)s{opacity:1;transform:translateY(%(sy)spx)}%(s2)s,100%%{opacity:0;transform:translateY(%(sy)spx)}}
.rip,.tip{transform-box:fill-box;transform-origin:center}
.p1{animation-name:rip1}.p2{animation-name:rip2}
@keyframes rip1{0%%,%(c0)s{opacity:0;transform:scale(.3)}%(r1)s{opacity:1;transform:scale(.45)}%(r2)s{opacity:0;transform:scale(1.9)}%(r2b)s,%(o0)s{opacity:.9;transform:scale(1)}%(o1)s,100%%{opacity:0;transform:scale(1)}}
@keyframes rip2{0%%,%(r1)s{opacity:0;transform:scale(.3)}%(r3)s{opacity:.85;transform:scale(.45)}%(r4)s,100%%{opacity:0;transform:scale(1.9)}}
.tip{animation-name:tip}
@keyframes tip{0%%,%(t0)s{opacity:0;transform:scale(1.8)}%(c0)s,%(o0)s{opacity:1;transform:scale(1)}%(o1)s,100%%{opacity:0;transform:scale(1)}}
""" % dict(L=f(L), hold=f(HOLD, 1), travel=f(travel, 1), q1=P(0.25), a0=P(1.9), a1=P(2.4), b0=P(3.6), b1=P(4.1),
           c0=P(5.2), c1=P(5.7), o0=P(9.2), o1=P(9.7), k0=P(0.3), k1=P(0.3 + 0.05 * len(C["ask"])), kn=len(C["ask"]),
           kw=f(askw, 1), s0=P(1.9), s0b=P(2.0), s1=P(3.3), s2=P(3.5), sy=f(sh + 140),
           r1=P(5.3), r2=P(6.1), r2b=P(6.5), r3=P(5.65), r4=P(6.5), t0=P(4.8)))
    d.css.append(REDUCED)
    return d.render()


# ----------------------------------------------------------------------------------------------
# the token comparison: 1280 x 938
# ----------------------------------------------------------------------------------------------
def tokens(T):
    W, H = 1280, 960
    rows = MEASURED
    plain = lambda s: s.replace("’", "'").replace("“", "'").replace("”", "'")  # noqa: E731
    d = Art(W, H, "What one look at the phone costs the AI, in tokens",
            "One real screen, a Xiaomi POCO X6 Pro home screen, counted with Claude Opus 5.5. "
            + " ".join("%s: %s tokens." % (plain(r["label"]), num(r["tokens"])) for r in rows)
            + " Looks that fit in a 200,000-token context window: " + ", ".join(
                "%s %s" % (num(r["fits"]), " ".join(x for x in r["cap"] if x)) for r in rows if r["fits"]) + ".")
    o = d.body
    d.card(W, H)
    o.append('<g clip-path="url(#card)">')
    o.append('<rect width="%d" height="%d" fill="%s"/>' % (W, H, T["bg"]))
    o.append(d.glow(1200, -80, 430, T["accent"], T["glow"] * 0.6))
    o.append(d.grain(T, W, H))
    X0 = 64
    L = 12.0
    P = lambda t: pct(t, L)  # noqa: E731
    css, dyn = [], []

    def colour(r):
        return T["accent"] if r["ours"] else T["neutral"]

    def rolled(i, s, x, y, key, size, fill, t0, t1, **kw):
        svg, w, travel = d.roll(s, x, y, key, size, fill, "n%d" % i, **kw)
        # a number never RESTS on a wrong value: it shows the true figure, snaps back only as the roll starts, and rolls up to it
        css.append(".n%d{animation-name:n%d}@keyframes n%d{0%%,%s{transform:translateY(-%spx)}%s{transform:translateY(0)}%s,100%%{transform:translateY(-%spx)}}"
                   % (i, i, i, P(t0), f(travel, 1), P(t0 + 0.002 * L), P(t1), f(travel, 1)))
        return svg, w

    # ---- part A: the cost of one look ----
    o.append(d.t("What one look at the phone costs the AI", X0, 82, "sansb", 38, T["ink"], tracking=-0.02))
    o.append(d.t("Tokens are the units an AI reads and pays in. Fewer per look means a cheaper step, and room for more steps.",
                 X0, 118, "sans", 20.5, T["dim"]))
    BX, BMAX, BH = 500.0, 540.0, 26.0
    top = BY["raw"]["tokens"]
    T0, SPEED = 0.5, BMAX / 2.2                    # every bar grows at the same speed, so the short ones stop first
    ys = {"raw": 202, "shot": 260, "full": 352, "short": 410, "diff": 468}
    o.append(d.t("WITHOUT AGENTROID", X0, 166, "cap", 14.5, T["dim"], tracking=0.14))
    o.append(d.t("WITH AGENTROID", X0, 316, "cap", 14.5, T["accent"], tracking=0.14))
    o.append('<path d="M%d 288.5H%d" stroke="%s"/>' % (X0, W - X0, T["stroke"]))
    a_end = 0.0
    for i, r in enumerate(rows):
        y = ys[r["key"]]
        w = max(2.0, BMAX * r["tokens"] / top)
        o.append(d.t(r["label"], X0, y + 1, "cap", 23, T["ink"], tracking=-0.005))
        o.append(d.t(r["sub"], X0, y + 24, "mono", 14, T["faint"]))
        t1 = T0 + w / SPEED
        a_end = max(a_end, t1)
        dyn.append('<rect class="bar g%d" x="%s" y="%s" width="%s" height="%s" rx="%s" fill="%s"/>'
                   % (i, f(BX), f(y - 18), f(w, 2), f(BH), f(min(7, w / 2), 2), colour(r)))
        vx = BX + w + 14
        v, vw = rolled(i, num(r["tokens"]), vx, y + 4, "sansb", 29, T["ink"], T0, max(t1, T0 + 0.7), tracking=-0.01)
        lab = [v, d.t("tokens", vx + vw + 8, y + 3, "sans", 18, T["dim"])]
        note = {"full": "about %d× fewer than the raw description" % RATIO_FULL,
                "short": "about %d× fewer than the raw description" % RATIO}.get(r["key"])
        if note:
            nx = vx + vw + 8 + d.tw("tokens", "sans", 18) + 18
            nw = d.tw(note, "cap", 16.5) + 28
            strong = r["key"] == "short"
            lab.append('<rect x="%s" y="%s" width="%s" height="32" rx="16" fill="%s" fill-opacity="%s" stroke="%s"/>'
                       % (f(nx), f(y - 21), f(nw), T["accent"], "1" if strong else "0", T["accent"] if strong else T["stroke"]))
            lab.append(d.t(note, nx + 14, y + 0.5, "cap", 16.5, T["onAccent"] if strong else T["dim"]))
        dyn.append('<g class="val v%d">%s</g>' % (i, "".join(lab)))
        css.append(".g%d{animation-name:g%d}@keyframes g%d{0%%,%s{transform:scaleX(0)}%s,100%%{transform:scaleX(1)}}" % (i, i, i, P(T0), P(t1)))
        css.append(".v%d{animation-name:v%d}@keyframes v%d{0%%,%s{opacity:0}%s,100%%{opacity:1}}" % (i, i, i, P(T0 - 0.2), P(T0)))

    # ---- part B: how many looks fit ----
    o.append('<path d="M%d 522.5H%d" stroke="%s"/>' % (X0, W - X0, T["stroke"]))
    o.append(d.t("How many looks fit before the AI runs out of room", X0, 574, "sansb", 30, T["ink"], tracking=-0.018))
    o.append(d.t("One small phone is one look at the screen, in a 200,000-token context window (the AI’s working memory).",
                 X0, 606, "sans", 19.5, T["dim"]))
    TW, TH, PXc, PYc, NR = 7.0, 10.0, 10.0, 13.0, 16
    d.defs.append('<rect id="tl" width="%s" height="%s" rx="1.7"/>' % (f(TW), f(TH)))
    d.defs.append('<g id="tc">%s</g>' % "".join('<use href="#tl" y="%s"/>' % f(k * PYc) for k in range(NR)))
    GY = 686.0                                    # top of the tile field
    bx = {"raw": 64.0, "shot": 250.0, "full": 432.0, "short": 664.0}
    B0 = a_end + 0.35
    TSPEED = (55 * PXc) / 2.6
    for i, r in enumerate(rows):
        if not r["fits"]:
            continue
        x, n = bx[r["key"]], r["fits"]
        full, rem = divmod(n, NR)
        cols = full + (1 if rem else 0)
        tiles = ['<use href="#tc" x="%s" y="%s"/>' % (f(x + c * PXc), f(GY)) for c in range(full)]
        tiles += ['<use href="#tl" x="%s" y="%s"/>' % (f(x + full * PXc), f(GY + (NR - 1 - k) * PYc)) for k in range(rem)]
        bw = cols * PXc
        t1 = B0 + bw / TSPEED
        # the tiles are complete as drawn; while animating, a cover in the page colour slides off them
        dyn.append('<g fill="%s">%s</g>' % (colour(r), "".join(tiles)))
        dyn.append('<rect class="cov k%d" x="%s" y="%s" width="%s" height="%s" fill="%s" transform="scale(0 1)"/>'
                   % (i, f(x - 1), f(GY - 2), f(bw + 2), f(NR * PYc + 2), T["bg"]))
        nv, nw_ = rolled(10 + i, num(n), x, 660, "sansb", 44, T["accent"] if r["ours"] else T["ink"], B0, max(t1, B0 + 0.7), tracking=-0.02)
        cap = [nv]
        if r["key"] == "short":
            cap.append(d.t(r["cap"][0], x + nw_ + 14, 643, "cap", 18, T["ink"]))
            cap.append(d.t("against %d for the raw description" % BY["raw"]["fits"], x + nw_ + 14, 664, "sans", 16.5, T["dim"]))
        else:
            o.append(d.t(r["cap"][0], x + nw_ + 12, 643 if r["cap"][1] else 654, "cap", 16.5, T["dim"]))
            if r["cap"][1]:
                o.append(d.t(r["cap"][1], x + nw_ + 12, 663, "cap", 16.5, T["dim"]))
        dyn.append('<g class="val w%d">%s</g>' % (i, "".join(cap)))
        css.append(".k%d{animation-name:k%d}@keyframes k%d{0%%,%s{transform:scaleX(1)}%s,100%%{transform:scaleX(0)}}" % (i, i, i, P(B0), P(t1)))
        css.append(".w%d{animation-name:w%d}@keyframes w%d{0%%,%s{opacity:0}%s,100%%{opacity:1}}" % (i, i, i, P(B0 - 0.2), P(B0)))
    o.append('<g class="dyn">%s</g>' % "".join(dyn))

    # ---- the small print, kept readable ----
    o.append(d.t("One real screen: a POCO X6 Pro home screen. Counted with Anthropic’s token counter on Claude Opus 5.5, 14 Nov 2026.",
                 X0, H - 44, "sans", 16.5, T["faint"]))
    o.append(d.t("Sizes of one screen, not a whole-task benchmark. On this screen the short answer holds only the clock widget; finding an app icon takes the full list.",
                 X0, H - 22, "sans", 16.5, T["faint"]))
    o.append('</g>')
    d.css.append("""
.bar,.val,.cov,.dyn,[class^="n"]{animation-duration:%(L)ss;animation-iteration-count:infinite;animation-delay:-6s;animation-timing-function:linear}
[class^="n"]{animation-timing-function:cubic-bezier(.2,.7,.2,1)}
.bar{transform-box:fill-box;transform-origin:left center}
.cov{transform-box:fill-box;transform-origin:right center}
.dyn{animation-name:dyn}
@keyframes dyn{0%%,%(f0)s{opacity:1}%(f1)s,99.99%%{opacity:0}100%%{opacity:1}}
%(rows)s
""" % dict(L=f(L), f0=P(11.2), f1=P(11.7), rows="\n".join(css)))
    d.css.append(REDUCED)
    return d.render()


# ----------------------------------------------------------------------------------------------
# how the parts connect: 1280 x 392
# ----------------------------------------------------------------------------------------------
def how(T):
    W, H = 1280, 392
    d = Art(W, H, "How Agentroid connects an AI agent to an Android phone",
            "Three parts in a row. Your AI agent (Claude Code, on your PC) asks Agentroid to look, tap or type. "
            "Agentroid, an MCP server and a small daemon on your PC, checks each action and makes no AI calls of its own, "
            "then drives your Android phone over adb, by USB or Wi-Fi. Coming back, the phone's raw screen description is "
            "%s tokens; Agentroid hands the AI a short list of %s tokens." % (num(BY["raw"]["tokens"]), num(BY["short"]["tokens"])))
    o = d.body
    d.card(W, H)
    o.append('<g clip-path="url(#card)">')
    o.append('<rect width="%d" height="%d" fill="%s"/>' % (W, H, T["bg"]))
    o.append(d.glow(640, 430, 420, T["accent"], T["glow"] * 0.5))
    o.append(d.grain(T, W, H))
    NY, NH, NW = 112.0, 150.0, 320.0
    xs = [64.0, 480.0, 896.0]
    UP, DN = NY + 44, NY + 106                     # the two lanes: requests go right, answers come back left
    L = 8.0
    P = lambda t: pct(t, L)  # noqa: E731

    # ---- the lanes, drawn under the boxes ----
    for k, (xa, xb) in enumerate(((xs[0] + NW, xs[1]), (xs[1] + NW, xs[2]))):
        o.append('<path d="M%s %sH%s" stroke="%s" stroke-width="2" fill="none"/>' % (f(xa), f(UP), f(xb - 9), T["faint"]))
        o.append('<path d="M%s %sl-9 -6v12z" fill="%s"/>' % (f(xb), f(UP), T["faint"]))
        o.append('<circle class="pk q%d" cx="%s" cy="%s" r="5.5" fill="%s"/>' % (k, f((xa + xb) / 2), f(UP), T["ink"]))
    # coming back: the raw description is thick, the short list is thin
    xa, xb = xs[1] + NW, xs[2]
    o.append('<path d="M%s %sH%s" stroke="%s" stroke-width="22" fill="none"/>' % (f(xa), f(DN), f(xb), T["neutral"]))
    d.defs.append('<clipPath id="band"><rect x="%s" y="%s" width="%s" height="22"/></clipPath>' % (f(xa), f(DN - 11), f(xb - xa)))
    o.append('<g clip-path="url(#band)"><g class="flow" fill="none" stroke="%s" stroke-width="2.5">%s</g></g>'
             % (T["bg"], "".join('<path d="M%s %sl-6 6l6 6"/>' % (f(xa + 16 + 24 * j), f(DN - 6)) for j in range(0 if STILL else -1, 4 if STILL else 6))))
    xa, xb = xs[0] + NW, xs[1]
    o.append('<path d="M%s %sH%s" stroke="%s" stroke-width="3" fill="none"/>' % (f(xa + 9), f(DN), f(xb), T["accent"]))
    o.append('<path d="M%s %sl9 -6v12z" fill="%s"/>' % (f(xa), f(DN), T["accent"]))
    o.append('<circle class="pk q2" cx="%s" cy="%s" r="5.5" fill="%s"/>' % (f((xa + xb) / 2), f(DN), T["accent"]))

    # ---- the three boxes ----
    box = [("Your AI agent", T["ink"], "Claude Code, on your PC", "decides what to do next"),
           ("Agentroid", T["accent"], "an MCP server + a small daemon", "checks each action · 0 AI calls"),
           ("Your Android phone", T["ink"], "over adb, by USB or Wi-Fi", "a real phone, or an emulator")]
    for k, (title, col, sub, third) in enumerate(box):
        x = xs[k]
        o.append(d.soft_shadow(T, x, NY, NW, NH, 18, spread=24, drop=10, k=0.55))
        o.append('<rect x="%s" y="%s" width="%s" height="%s" rx="18" fill="%s" stroke="%s" stroke-width="%s"/>'
                 % (f(x), f(NY), f(NW), f(NH), T["surface"], T["accent"] if k == 1 else T["stroke"], "1.5" if k == 1 else "1"))
        tx = x + 24
        if k == 2:
            # the real screenshot, small, in the same thin bezel as the banner
            pw, ph = 54.0, 54.0 * SHOT_H / SHOT_W
            px, py = x + 20, NY + (NH - ph) / 2
            d.defs.append('<clipPath id="th"><rect x="%s" y="%s" width="%s" height="%s" rx="7"/></clipPath>' % (f(px), f(py), f(pw), f(ph)))
            o.append('<rect x="%s" y="%s" width="%s" height="%s" rx="9.5" fill="#0B0C0E"/>' % (f(px - 2.5), f(py - 2.5), f(pw + 5), f(ph + 5)))
            o.append('<image href="%s" x="%s" y="%s" width="%s" height="%s" clip-path="url(#th)"/>' % (thumb_uri(162), f(px), f(py), f(pw), f(ph)))
            o.append('<circle class="rip" cx="%s" cy="%s" r="7" fill="none" stroke="#3DDC84" stroke-width="2"/>' % (f(px + pw * 0.3), f(py + ph * 0.6)))
            tx = px + pw + 20
        o.append(d.t(title, tx, NY + 52, "sansb", 24 if k != 2 else 22, col, tracking=-0.01))
        o.append(d.t(sub, tx, NY + 84, "sans", 17 if k != 2 else 16, T["dim"]))
        o.append(d.t(third, tx, NY + 112, "mono", 13.5 if k != 2 else 12.5, T["faint"]))

    # ---- what travels on each lane, in plain words ----
    g1, g2 = (xs[0] + NW + xs[1]) / 2, (xs[1] + NW + xs[2]) / 2
    o.append(d.t("look · tap · type", g1, 70, "cap", 18, T["ink"], anchor="middle"))
    o.append(d.t("what the AI asks for", g1, 93, "sans", 15, T["dim"], anchor="middle"))
    o.append(d.t("checked actions", g2, 70, "cap", 18, T["ink"], anchor="middle"))
    o.append(d.t("payments it spots: refused", g2, 93, "sans", 15, T["dim"], anchor="middle"))
    a, aw, tr1 = d.roll(num(BY["short"]["tokens"]), g1, 312, "sansb", 30, T["accent"], "ra", anchor="start")
    o.append('<g transform="translate(%s 0)">%s</g>' % (f(-aw / 2, 1), a))
    o.append(d.t("tokens: Agentroid’s short list", g1, 338, "sans", 15, T["dim"], anchor="middle"))
    b, bw, tr2 = d.roll(num(BY["raw"]["tokens"]), g2, 312, "sansb", 30, T["ink"], "rb", anchor="start")
    o.append('<g transform="translate(%s 0)">%s</g>' % (f(-bw / 2, 1), b))
    o.append(d.t("tokens: Android’s raw description", g2, 338, "sans", 15, T["dim"], anchor="middle"))
    o.append(d.t("Everything runs on your own PC. Agentroid needs no AI model and no API key of its own.", W / 2, H - 20, "sans", 16, T["faint"], anchor="middle"))
    o.append('</g>')
    span = xs[1] - xs[0] - NW
    d.css.append("""
.pk,.flow,.rip,.ra,.rb{animation-duration:%(L)ss;animation-iteration-count:infinite;animation-timing-function:linear}
.q0{animation-name:q0}.q1{animation-name:q1}.q2{animation-name:q2}
@keyframes q0{0%%{transform:translateX(-%(h)spx);opacity:0}3%%{opacity:1}%(a)s{transform:translateX(%(h)spx);opacity:1}%(a2)s,100%%{transform:translateX(%(h)spx);opacity:0}}
@keyframes q1{0%%,%(b0)s{transform:translateX(-%(h)spx);opacity:0}%(b1)s{opacity:1}%(b)s{transform:translateX(%(h)spx);opacity:1}%(b2)s,100%%{transform:translateX(%(h)spx);opacity:0}}
@keyframes q2{0%%,%(c0)s{transform:translateX(%(h)spx);opacity:0}%(c1)s{opacity:1}%(c)s{transform:translateX(-%(h)spx);opacity:1}%(c2)s,100%%{transform:translateX(-%(h)spx);opacity:0}}
.flow{animation-name:flow;animation-duration:1.1s}
@keyframes flow{to{transform:translateX(-24px)}}
.rip{transform-box:fill-box;transform-origin:center;animation-name:hrip}
@keyframes hrip{0%%,%(b)s{opacity:0;transform:scale(.4)}%(b2)s{opacity:1;transform:scale(.7)}%(r)s{opacity:0;transform:scale(2.2)}%(r2)s,100%%{opacity:1;transform:scale(1)}}
.ra,.rb{animation-timing-function:cubic-bezier(.2,.7,.2,1)}
.rb{animation-name:rb}.ra{animation-name:ra}
@keyframes rb{0%%,%(r)s{transform:translateY(-%(tr2)spx)}%(rq)s{transform:translateY(0)}%(c0)s,100%%{transform:translateY(-%(tr2)spx)}}
@keyframes ra{0%%,%(c0)s{transform:translateY(-%(tr1)spx)}%(cq)s{transform:translateY(0)}%(c2)s,100%%{transform:translateY(-%(tr1)spx)}}
""" % dict(L=f(L), h=f(span / 2 - 12, 1), a=P(1.2), a2=P(1.45), b0=P(1.5), b1=P(1.7), b=P(2.7), b2=P(2.95),
           r=P(3.6), rq=P(3.6 + 0.002 * L), cq=P(4.6 + 0.002 * L), r2=P(4.0), c0=P(4.6), c1=P(4.8), c=P(5.8), c2=P(6.05), tr1=f(tr1, 1), tr2=f(tr2, 1)))
    d.css.append(REDUCED)
    return d.render()


def write(path, svg):
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(svg + "\n")
    print("%-18s %6.1f KB" % (os.path.basename(path), os.path.getsize(path) / 1024))


def main():
    for name, T in THEMES.items():
        write(os.path.join(MEDIA, "banner-%s.svg" % name), banner(T))
        write(os.path.join(MEDIA, "tokens-%s.svg" % name), tokens(T))
        write(os.path.join(MEDIA, "how-%s.svg" % name), how(T))
    p = PHONE
    print("phone screen box: x=%s y=%s w=%s h=%s (visible to y=640); bezel %s px" % (f(p["x"]), f(p["y"]), f(p["w"]), f(p["h"]), f(p["bezel"])))


if __name__ == "__main__":
    main()
