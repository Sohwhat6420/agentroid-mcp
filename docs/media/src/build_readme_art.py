"""Builds the README's hero card and the social banner from one real screen.

    pip install fonttools
    cargo run -q -p agentroid-core --example readme_tree_sizes > docs/media/src/tree_sizes.txt
    python docs/media/src/build_readme_art.py
    node docs/media/src/render_banner.mjs

Writes docs/media/hero-dark.svg and hero-light.svg (the animated card at the top of the README) and
docs/media/src/banner-1280x640.svg and banner-1200x627.svg, which render_banner.mjs turns into
assets/banner-1280x640.png (GitHub social preview) and assets/banner-linkedin-1200x627.png.

Every number and every line of text drawn from the phone comes from tree_sizes.txt, the output of
crates/core/examples/readme_tree_sizes.rs run on the recorded POCO X6 Pro home screen
(crates/core/tests/redteam/recorded/poco-launcher-2026-10-20.xml). The budget figures come from the
tests named next to them in the README. Nothing is typed in by hand except labels.

The motion is CSS inside the SVG (no script), like the profile cards at github.com/Sohwhat6420/Sohwhat6420:
four scenes cross-fade on one 16 s cycle, and under prefers-reduced-motion the card holds still on the
digest scene, the default.
"""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from artlib import Doc, GLASS_PAD, REDUCED, THEMES, esc, f, font, glass  # noqa: E402

MEDIA = os.path.dirname(HERE)
ROOT = os.path.dirname(os.path.dirname(MEDIA))
SIZES = os.path.join(HERE, "tree_sizes.txt")
RAW = os.path.join(ROOT, "crates", "core", "tests", "redteam", "recorded", "poco-launcher-2026-10-20.xml")


# ----------------------------------------------------------------------------------------------
# the measured text
# ----------------------------------------------------------------------------------------------
def sections(text):
    out, cur = {}, None
    for line in text.split("\n"):
        m = re.match(r"^== (.+) ==$", line)
        if m:
            cur = m.group(1)
            out[cur] = []
        elif cur is not None:
            out[cur].append(line)
    return {k: "\n".join(v).strip("\n") for k, v in out.items()}


def load():
    s = sections(open(SIZES, encoding="utf-8").read())
    sizes = {}
    for line in s["sizes (characters)"].split("\n"):
        m = re.match(r"^(.+?)\s{2,}(\d+)\s", line)
        if m:
            sizes[m.group(1).strip()] = int(m.group(2))
    raw = open(RAW, encoding="utf-8").read()
    assert len(raw) == sizes["raw uiautomator dump"], "tree_sizes.txt is stale: re-run the example"
    return s, sizes, raw


S, SIZE, RAW_XML = load()
N_RAW = SIZE["raw uiautomator dump"]
N_FULL = SIZE["ui_tree full listing"]
N_DIG = SIZE["ui_tree digest (default)"]
N_SINCE = SIZE["ui_tree since=, nothing changed"]


def grp(n):
    return "{:,}".format(n)


def pct(n):
    v = n / N_RAW * 100
    return "%s%%" % (("%.1f" % v) if v < 99.95 else "100")


def clip_line(s, n):
    return s if len(s) <= n else s[: n - 1] + "…"


# ----------------------------------------------------------------------------------------------
# the hero card
# ----------------------------------------------------------------------------------------------
CYCLE = 20.0          # seconds: four scenes of 5 s
OUT, IN = 1.5, 2.5    # % of the cycle: the old scene fades out first, then the new one fades in (no overlap)
XF = 2.0              # % of the cycle for the bar highlight's cross-fade (old and new sit in the same place)


def scene_kf(i, n=4):
    """Scene i of n: fades in over IN after its boundary, holds, fades out over OUT before the next one."""
    a, b = 100.0 * i / n, 100.0 * (i + 1) / n
    kf = "0%{opacity:0}"
    if i:
        kf += "%s%%{opacity:0}" % f(a, 2)
    kf += "%s%%{opacity:1}%s%%{opacity:1}%s%%{opacity:0}" % (f(a + IN, 2), f(b - OUT, 2), f(b, 2))
    if b < 100:
        kf += "100%{opacity:0}"
    return kf


def hl_kf(i, n=4):
    """The bar row highlight: a true cross-fade round each boundary. Row 0 wraps round 0 %."""
    a, b = 100.0 * i / n, 100.0 * (i + 1) / n
    if i == 0:
        return "0%%{opacity:1}%s%%{opacity:1}%s%%{opacity:0}%s%%{opacity:0}100%%{opacity:1}" % (
            f(b - XF / 2, 2), f(b + XF / 2, 2), f(100 - XF / 2, 2))
    end = "%s%%{opacity:0}100%%{opacity:0}" % f(b + XF / 2, 2) if b < 100 else "100%{opacity:0}"
    return "0%%{opacity:0}%s%%{opacity:0}%s%%{opacity:1}%s%%{opacity:1}%s" % (
        f(a - XF / 2, 2), f(a + XF / 2, 2), f(b - XF / 2, 2), end)


def seg_kf(i, n=4):
    """A progress segment fills during its own scene and stays full; all reset at once when the cycle restarts."""
    a, b = 100.0 * i / n, 100.0 * (i + 1) / n
    kf = "0%{transform:scaleX(0)}"
    if i:
        kf += "%s%%{transform:scaleX(0)}" % f(a, 2)
    kf += "%s%%{transform:scaleX(1)}" % f(b, 2)
    if b < 100:
        kf += "99.99%{transform:scaleX(1)}100%{transform:scaleX(0)}"
    return kf


BAR_GOLD = {"dark": "#d9b77a", "light": "#a8802f"}   # bars are not text: 3:1 is enough, so light gets a brighter gold
RAIL_OP = {"dark": ".35", "light": ".7"}


def hero(T):
    W, H = 960, 432
    doc = Doc(W, H, "Agentroid: the most phone control for the fewest tokens",
              "Agentroid, an MCP server that lets an AI agent see and drive an Android phone. One real screen, a "
              "Xiaomi POCO X6 Pro home screen recorded on 20 Oct 2026, shown four ways in turn: the raw uiautomator "
              "dump, %s characters; Agentroid's full ui_tree listing, %s characters; the digest the agent gets by "
              "default, %s characters; and a since= diff when nothing changed, %s characters. 27 tools in under "
              "5,000 tokens, and no model calls of its own." % (grp(N_RAW), grp(N_FULL), grp(N_DIG), grp(N_SINCE)))
    doc.pad = GLASS_PAD
    doc.body.append(glass(doc, T, 1, 1, W - 2, H - 2, 18))
    b = doc.body
    gold_bar = BAR_GOLD[T["name"]]

    # ---- head
    a, _ = doc.text("ANDROID  ·  MCP  ·  RUST  ·  PREVIEW 0.1.0", 32, 46, "cap", 12, T["gold"], tracking=0.12)
    t, _ = doc.text("Agentroid", 30, 100, "serif", 54, T["text"])
    st, _ = doc.text("The most phone control for the fewest tokens.", 32, 132, "sans", 18, T["dim"])
    r1, _ = doc.text("27 tools in under 5,000 tokens.", W - 32, 76, "sansb", 16, T["text"], anchor="end")
    r2, _ = doc.text("No model calls of its own.", W - 32, 100, "sans", 16, T["dim"], anchor="end")
    b.append('<g class="rise">%s%s%s</g><g class="rise" style="animation-delay:.2s">%s%s</g>' % (a, t, st, r1, r2))

    # ---- left: the screen as text, four ways
    px, py, pw, ph = 32, 178, 548, 206
    lab, _ = doc.text("one real screen, four ways", px, py - 14, "mono", 12, T["dim"])
    b.append(lab)
    b.append('<rect x="%s" y="%s" width="%s" height="%s" rx="12" fill="%s" stroke="%s"/>'
             % (px, py, pw, ph, T["chip"], T["chipStroke"]))
    doc.defs.append('<clipPath id="pc"><rect x="%s" y="%s" width="%s" height="%s" rx="12"/></clipPath>'
                    % (px + 1, py + 1, pw - 2, ph - 2))
    # progress rail: four neutral segments; the one playing is drawn in the text colour
    sw = (pw - 28 - 3 * 6) / 4
    segs = []
    for i in range(4):
        sx = px + 14 + i * (sw + 6)
        r = 'x="%s" y="%s" width="%s" height="3" rx="1.5"' % (f(sx, 1), py + 12, f(sw, 1))
        segs.append('<rect %s fill="%s" opacity="%s"/>' % (r, T["line"], RAIL_OP[T["name"]]))
        segs.append('<rect %s fill="%s" opacity=".6" class="pg pg%d"/>' % (r, T["dim"], i))
        segs.append('<g class="hl hl%d"><rect %s fill="%s" class="pg pg%d"/></g>' % (i, r, T["text"], i))
    b.append('<g class="rail">%s</g>' % "".join(segs))

    CUT = 70
    raw_lines = [RAW_XML[k:k + CUT] for k in range(0, CUT * 6, CUT)]
    full_lines = S["ui_tree, level full"].split("\n")
    dig_lines = S["ui_tree, level digest (the default)"].split("\n")
    since_lines = S["ui_tree since=s1, same screen"].split("\n")
    scenes = [
        ("raw uiautomator dump", "what an agent reads without Agentroid: one %s-character line" % grp(N_RAW),
         raw_lines + ["… %s characters in all, on one line" % grp(N_RAW)], None),
        ("ui_tree, level full", "one line per element (%d here), each tappable one with a stable ref" % len(full_lines),
         [clip_line(x, CUT) for x in full_lines[:7]], (1, 1, 1, 1, 1, .5, .18)),
        ("ui_tree, level digest (the default)", "cut at 400 characters, with a note on how to ask for more",
         [clip_line(x, CUT) for x in dig_lines], None),
        ("ui_tree since=s1", "the same screen read again: one line back",
         [clip_line(x, CUT) for x in since_lines], None),
    ]
    for i, (title, note, lines, fades) in enumerate(scenes):
        g = []
        tt, _ = doc.text(title, px + 16, py + 40, "monob", 13, T["text"])
        nn, _ = doc.text(note, px + 16, py + 60, "sans", 14, T["dim"])
        g += [tt, nn]
        for k, line in enumerate(lines[:7]):
            key, colour = "mono", T["acc"]
            if line.startswith("…"):
                colour = T["faint"]
            elif line.startswith("[cut"):
                key, colour = "monob", T["text"]
            op = fades[k] if fades else None
            ln, _ = doc.text(line, px + 16, py + 86 + k * 17.5, key, 12, colour, opacity=op if op != 1 else None)
            g.append(ln)
        b.append('<g clip-path="url(#pc)"><g class="sc sc%d">%s</g></g>' % (i, "".join(g)))

    # ---- right: the same four, as bars on one linear scale; rows span the panel's height exactly
    bx, bw = 612, 316
    lab2, _ = doc.text("characters, same screen", bx, py - 14, "mono", 12, T["dim"])
    b.append(lab2)
    rows = [("raw dump", N_RAW), ("ui_tree, full", N_FULL), ("digest (default)", N_DIG), ("since=, no change", N_SINCE)]
    trk_y, trk_h = 28, 6
    row_h = (ph - trk_y - trk_h) / 3
    for i, (name, n) in enumerate(rows):
        y0 = py + i * row_h
        w = max(2.0, bw * n / N_RAW)

        def row(colour_t, colour_bar, show_pct):
            nm, _ = doc.text(name, bx, y0 + 12, "sans", 15, colour_t)
            val, _ = doc.text(grp(n), bx + bw, y0 + 16, "num", 26, colour_t, anchor="end")
            out = nm + val
            if show_pct:
                pc_, _ = doc.text(pct(n), bx + bw - font("num").width(grp(n), 26) - 10, y0 + 16, "mono", 12,
                                  colour_t, anchor="end")
                out += pc_
            out += '<rect x="%s" y="%s" width="%s" height="%s" rx="3" fill="%s" opacity="%s"/>' % (
                bx, f(y0 + trk_y, 1), bw, trk_h, T["line"], RAIL_OP[T["name"]])
            out += '<rect x="%s" y="%s" width="%s" height="%s" rx="3" fill="%s"/>' % (
                bx, f(y0 + trk_y, 1), f(w, 2), trk_h, colour_bar)
            return out

        b.append(row(T["faint"], T["line"], False))
        b.append('<g class="hl hl%d">%s</g>' % (i, row(T["text"], gold_bar, True)))

    # ---- foot
    ft, _ = doc.text("Recorded on a Xiaomi POCO X6 Pro, 20 Oct 2026  ·  readme_tree_sizes example", 32, H - 20,
                     "mono", 12, T["faint"])
    b.append(ft)

    # ---- motion
    css = [
        ".rise{animation:rise 1.2s cubic-bezier(.2,.7,.2,1) both}",
        "@keyframes rise{from{opacity:0;transform:translateY(14px)}to{opacity:1;transform:none}}",
        ".sc,.hl{opacity:0}.sc2,.hl2{opacity:1}",
        ".pg{transform-box:fill-box;transform-origin:0 50%;transform:scaleX(1)}",
    ]
    for i in range(4):
        css.append("@keyframes o%d{%s}" % (i, scene_kf(i)))
        css.append("@keyframes h%d{%s}" % (i, hl_kf(i)))
        css.append("@keyframes p%d{%s}" % (i, seg_kf(i)))
        css.append(".sc%d{animation:o%d %ss linear infinite}" % (i, i, f(CYCLE)))
        css.append(".hl%d{animation:h%d %ss linear infinite}" % (i, i, f(CYCLE)))
        css.append(".pg%d{animation:p%d %ss linear infinite}" % (i, i, f(CYCLE)))
    doc.css.append("\n".join(css))
    # reduced motion: every animation stops, the card holds on the digest (the default) and the rail goes
    doc.css.append(REDUCED + "@media (prefers-reduced-motion: reduce){.rail{display:none}}")
    return doc


# ----------------------------------------------------------------------------------------------
# the banner: GitHub's own palette, like the chart at the top of human-llm-council's README
# ----------------------------------------------------------------------------------------------
GH = dict(bg="#0d1117", text="#e6edf3", dim="#9198a1", grid="#30363d", stem="#656c76", blue="#3987e5")


def banner(W, H, footer):
    """A wordmark, the claim, one lollipop chart of the same screen four ways, and (GitHub only) a footer.
    Laid out at the target size, not scaled, and centred vertically when there is no footer."""
    doc = Doc(W, H, "Agentroid: the most phone control for the fewest tokens",
              "Agentroid banner. The most phone control for the fewest tokens: 27 tools in under 5,000 tokens, no "
              "model calls of its own. One real Android screen as the raw uiautomator dump, %s characters; the "
              "full ui_tree listing, %s; the default digest, %s; and a since= diff with nothing changed, %s."
              % (grp(N_RAW), grp(N_FULL), grp(N_DIG), grp(N_SINCE)))
    b = doc.body
    b.append('<rect width="%d" height="%d" fill="%s"/>' % (W, H, GH["bg"]))
    X0 = 72
    content_h = 490            # from the wordmark's top (y 0) to the foot of the chart's gridlines
    oy = 62 if footer else (H - content_h) / 2
    g = []
    wm, _ = doc.text("Agentroid", X0 - 2, 46, "serif", 52, GH["text"])
    t, _ = doc.text("The most phone control for the fewest tokens", X0 - 2, 112, "sansb", 50, GH["text"])
    st, _ = doc.text("27 tools in under 5,000 tokens. No model calls of its own.", X0, 158, "sans", 25, GH["dim"])
    g += [wm, t, st]
    cy0 = 222
    cap, _ = doc.text("One real screen, a POCO X6 Pro home screen, in characters", X0, cy0, "sans", 20, GH["dim"])
    g.append(cap)
    cx0, cx1 = 400, W - 180
    rows = [("Raw uiautomator dump", N_RAW, False), ("ui_tree, full listing", N_FULL, False),
            ("ui_tree digest (default)", N_DIG, True), ("since= diff, no change", N_SINCE, False)]
    top, pitch = cy0 + 54, 60
    for k in range(0, N_RAW + 1, 10000):
        x = cx0 + (cx1 - cx0) * k / N_RAW
        g.append('<line x1="%s" y1="%s" x2="%s" y2="%s" stroke="%s"/>' % (f(x, 1), cy0 + 22, f(x, 1), top + 3 * pitch + 34, GH["grid"]))
    for i, (name, n, hot) in enumerate(rows):
        y = top + i * pitch
        x = cx0 + (cx1 - cx0) * n / N_RAW
        lab, _ = doc.text(name, cx0 - 26, y + 9, "sansb" if hot else "sans", 26, GH["text"] if hot else GH["dim"], anchor="end")
        g.append(lab)
        g.append('<line x1="%s" y1="%s" x2="%s" y2="%s" stroke="%s" stroke-width="2.5"/>' % (cx0, y, f(x, 1), y, GH["stem"]))
        g.append('<circle cx="%s" cy="%s" r="12" fill="%s" stroke="%s" stroke-width="2"/>' % (f(x, 1), y, GH["blue"], GH["bg"]))
        key, colour = ("sansb", GH["text"]) if hot else ("sans", GH["dim"])
        v, _ = doc.text(grp(n), x + 26, y + 9, key, 24, colour)
        g.append(v)
    b.append('<g transform="translate(0 %s)">%s</g>' % (f(oy, 2), "".join(g)))
    if footer:
        ft, _ = doc.text("Preview 0.1.0  ·  Apache-2.0  ·  github.com/Sohwhat6420/agentroid-mcp", X0, H - 30,
                         "mono", 16, GH["dim"])
        b.append(ft)
    return doc


def write(path, doc):
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(doc.render())
    print("%7.1f KB  %s" % (os.path.getsize(path) / 1024, os.path.relpath(path, ROOT)))


def main():
    for name, T in THEMES.items():
        write(os.path.join(MEDIA, "hero-%s.svg" % name), hero(T))
    write(os.path.join(HERE, "banner-1280x640.svg"), banner(1280, 640, True))
    write(os.path.join(HERE, "banner-1200x627.svg"), banner(1200, 627, False))


if __name__ == "__main__":
    main()
