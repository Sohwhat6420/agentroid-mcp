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
CYCLE = 16.0          # seconds: four scenes of 4 s
FADE = 2.2            # % of the cycle for each cross-fade


def scene_kf(i, n=4):
    """Opacity keyframes for scene i of n: in over FADE, held, out over FADE. Scene 0 wraps round 0 %."""
    a, b = 100.0 * i / n, 100.0 * (i + 1) / n
    if i == 0:
        return "0%%{opacity:1}%s%%{opacity:1}%s%%{opacity:0}%s%%{opacity:0}100%%{opacity:1}" % (
            f(b - FADE, 2), f(b, 2), f(100 - FADE, 2))
    return "0%%{opacity:0}%s%%{opacity:0}%s%%{opacity:1}%s%%{opacity:1}%s%%{opacity:0}100%%{opacity:0}" % (
        f(a - FADE, 2), f(a, 2), f(b - FADE, 2), f(b, 2))


def seg_kf(i, n=4):
    """A progress segment fills during its own scene and stays full until the cycle restarts."""
    a, b = 100.0 * i / n, 100.0 * (i + 1) / n
    pre = "0%{transform:scaleX(0)}" + ("%s%%{transform:scaleX(0)}" % f(a, 2) if i else "")
    return pre + "%s%%{transform:scaleX(1)}99.4%%{transform:scaleX(1)}100%%{transform:scaleX(0)}" % f(b, 2)


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

    # ---- head
    a, _ = doc.text("ANDROID  ·  MCP  ·  RUST  ·  PREVIEW 0.1.0", 32, 46, "cap", 12, T["gold"], tracking=0.12)
    t, _ = doc.text("Agentroid", 30, 100, "serif", 54, T["text"])
    st, _ = doc.text("The most phone control for the fewest tokens.", 32, 132, "sans", 18, T["dim"])
    r1, _ = doc.text("27 tools in under 5,000 tokens.", W - 32, 74, "sansb", 16, T["text"], anchor="end")
    r2, _ = doc.text("No model calls of its own.", W - 32, 98, "sans", 16, T["dim"], anchor="end")
    b.append('<g class="rise">%s%s%s</g><g class="rise" style="animation-delay:.2s">%s%s</g>' % (a, t, st, r1, r2))

    # ---- left: the screen as text, four ways
    px, py, pw, ph = 32, 178, 548, 206
    lab, _ = doc.text("one real screen, four ways", px, py - 14, "mono", 11, T["dim"])
    b.append(lab)
    b.append('<rect x="%s" y="%s" width="%s" height="%s" rx="12" fill="%s" stroke="%s"/>'
             % (px, py, pw, ph, T["chip"], T["chipStroke"]))
    doc.defs.append('<clipPath id="pc"><rect x="%s" y="%s" width="%s" height="%s" rx="12"/></clipPath>'
                    % (px + 1, py + 1, pw - 2, ph - 2))
    # progress: four segments across the top of the panel
    sw = (pw - 28 - 3 * 6) / 4
    segs = []
    for i in range(4):
        sx = px + 14 + i * (sw + 6)
        segs.append('<rect x="%s" y="%s" width="%s" height="3" rx="1.5" fill="%s" opacity=".35"/>'
                    % (f(sx, 1), py + 12, f(sw, 1), T["line"]))
        segs.append('<rect x="%s" y="%s" width="%s" height="3" rx="1.5" fill="%s" class="pg pg%d"/>'
                    % (f(sx, 1), py + 12, f(sw, 1), T["gold"], i))
    b.append("".join(segs))

    raw_lines = [RAW_XML[k:k + 74] for k in range(0, 74 * 6, 74)]
    full_lines = S["ui_tree, level full"].split("\n")
    dig_lines = S["ui_tree, level digest (the default)"].split("\n")
    since_lines = S["ui_tree since=s1, same screen"].split("\n")
    scenes = [
        ("raw uiautomator dump", "what an agent reads without Agentroid: one %s-character line" % grp(N_RAW),
         raw_lines + ["… %s more characters" % grp(N_RAW - 74 * 6)]),
        ("ui_tree, level full", "one line per element, with a stable ref (r2bb170) to tap",
         [clip_line(x, 76) for x in full_lines[:6]] + ["… %d elements in all" % len(full_lines)]),
        ("ui_tree, level digest (the default)", "cut at 400 characters, with a note on how to ask for more",
         [clip_line(x, 76) for x in dig_lines]),
        ("ui_tree since=s1", "the same screen read again: one line back",
         [clip_line(x, 76) for x in since_lines]),
    ]
    for i, (title, note, lines) in enumerate(scenes):
        g = []
        tt, _ = doc.text(title, px + 16, py + 40, "monob", 12.5, T["text"])
        nn, _ = doc.text(note, px + 16, py + 58, "sans", 13, T["dim"])
        g += [tt, nn]
        for k, line in enumerate(lines[:7]):
            colour = T["faint"] if line.startswith("…") or line.startswith("[cut") else T["acc"]
            if line.startswith("[cut"):
                colour = T["gold"]
            ln, _ = doc.text(line, px + 16, py + 84 + k * 16.5, "mono", 11, colour)
            g.append(ln)
        b.append('<g clip-path="url(#pc)"><g class="sc sc%d">%s</g></g>' % (i, "".join(g)))

    # ---- right: the same four, as bars on one linear scale
    bx, bw = 612, 316
    lab2, _ = doc.text("characters, same screen", bx, py - 14, "mono", 11, T["dim"])
    b.append(lab2)
    rows = [("raw dump", N_RAW), ("ui_tree, full", N_FULL), ("digest (default)", N_DIG), ("since=, no change", N_SINCE)]
    row_h = ph / 4
    for i, (name, n) in enumerate(rows):
        y0 = py + i * row_h
        w = max(2.0, bw * n / N_RAW)

        def row(colour_t, colour_n, colour_bar, op_track):
            nm, _ = doc.text(name, bx, y0 + 22, "sans", 14, colour_t)
            val, _ = doc.text(grp(n), bx + bw, y0 + 24, "num", 26, colour_n, anchor="end")
            pc_, pcw = doc.text(pct(n), bx + bw - font("num").width(grp(n), 26) - 10, y0 + 24, "mono", 11, colour_t,
                                anchor="end")
            trk = '<rect x="%s" y="%s" width="%s" height="6" rx="3" fill="%s" opacity="%s"/>' % (
                bx, f(y0 + 34, 1), bw, T["line"], op_track)
            bar = '<rect x="%s" y="%s" width="%s" height="6" rx="3" fill="%s"/>' % (bx, f(y0 + 34, 1), f(w, 2), colour_bar)
            return nm + val + pc_ + trk + bar

        b.append(row(T["faint"], T["faint"], T["line"], ".35"))
        b.append('<g class="hl hl%d">%s</g>' % (i, row(T["text"], T["text"], T["gold"], ".35")))

    # ---- foot
    ft, _ = doc.text("Recorded on a Xiaomi POCO X6 Pro, 20 Oct 2026. Reproduce: cargo run -p agentroid-core --example "
                     "readme_tree_sizes", 32, H - 22, "mono", 10.5, T["faint"])
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
        css.append("@keyframes p%d{%s}" % (i, seg_kf(i)))
        css.append(".sc%d,.hl%d{animation:o%d %ss linear infinite}" % (i, i, i, f(CYCLE)))
        css.append(".pg%d{animation:p%d %ss linear infinite}" % (i, i, f(CYCLE)))
    doc.css.append("\n".join(css))
    # reduced motion: every animation stops and the card holds on the digest, the default
    doc.css.append(REDUCED)
    return doc


# ----------------------------------------------------------------------------------------------
# the banner: GitHub's own palette, like the chart at the top of human-llm-council's README
# ----------------------------------------------------------------------------------------------
GH = dict(bg="#0d1117", text="#e6edf3", dim="#9198a1", grid="#30363d", stem="#656c76", blue="#3987e5",
          orange="#d95926", tile="#161b22", tileStroke="#30363d", gold="#d9b77a")


def banner(W, H):
    """The 1280x640 composition, scaled to fit W x H and centred on the page colour."""
    BW, BH = 1280, 640
    s = min(W / BW, H / BH)
    ox, oy = (W - BW * s) / 2, (H - BH * s) / 2
    doc = Doc(W, H, "Agentroid: the most phone control for the fewest tokens",
              "Agentroid banner. One real Android screen as the raw uiautomator dump, %s characters; the full "
              "ui_tree listing, %s; the default digest, %s; and a since= diff with nothing changed, %s. Below: 27 "
              "tools in under 5,000 tokens, a 400-character default digest, refs kept 100%% over 200 re-layouts, "
              "0 model calls in the server." % (grp(N_RAW), grp(N_FULL), grp(N_DIG), grp(N_SINCE)))
    b = doc.body
    b.append('<rect width="%d" height="%d" fill="%s"/>' % (W, H, GH["bg"]))
    g = []
    X0 = 72
    e, _ = doc.text("AGENTROID  ·  MCP SERVER FOR ANDROID", X0, 84, "cap", 17, GH["gold"], tracking=0.12)
    t, _ = doc.text("The most phone control for the fewest tokens", X0 - 2, 146, "sansb", 50, GH["text"])
    st, _ = doc.text("An AI agent sees and drives an Android phone over adb. Agentroid keeps what it reads small.",
                     X0, 190, "sans", 23, GH["dim"])
    g += [e, t, st]

    # chart: one real screen, four ways, lollipops on one linear scale
    cx0, cx1 = 420, 1150
    cy0 = 250
    cap, _ = doc.text("One real screen (POCO X6 Pro home screen), in characters", X0, cy0, "sans", 19, GH["dim"])
    g.append(cap)
    rows = [("Raw uiautomator dump", N_RAW, False), ("ui_tree, full listing", N_FULL, False),
            ("ui_tree digest (default)", N_DIG, True), ("since= diff, no change", N_SINCE, False)]
    for k in range(0, N_RAW + 1, 10000):
        x = cx0 + (cx1 - cx0) * k / N_RAW
        g.append('<line x1="%s" y1="%s" x2="%s" y2="%s" stroke="%s"/>' % (f(x, 1), cy0 + 16, f(x, 1), cy0 + 196, GH["grid"]))
        tk, _ = doc.text("{:,}".format(k), x, cy0 + 218, "sans", 15, GH["dim"], anchor="middle")
        g.append(tk)
    for i, (name, n, hot) in enumerate(rows):
        y = cy0 + 44 + i * 44
        lab, _ = doc.text(name, cx0 - 24, y + 7, "sansb" if hot else "sans", 21, GH["text"], anchor="end")
        x = cx0 + (cx1 - cx0) * n / N_RAW
        g.append(lab)
        g.append('<line x1="%s" y1="%s" x2="%s" y2="%s" stroke="%s" stroke-width="2.5"/>' % (cx0, y, f(x, 1), y, GH["stem"]))
        if hot:
            g.append('<path d="M%s %sl11 11-11 11-11-11z" fill="%s" stroke="%s" stroke-width="2"/>'
                     % (f(x, 1), y - 11, GH["orange"], GH["bg"]))
        else:
            g.append('<circle cx="%s" cy="%s" r="10" fill="%s" stroke="%s" stroke-width="2"/>' % (f(x, 1), y, GH["blue"], GH["bg"]))
        val = "%s  (%s)" % (grp(n), pct(n)) if i else grp(n)
        if x > cx1 - 200:
            v, _ = doc.text(val, x, y - 18, "sansb", 19, GH["text"], anchor="end")
        else:
            v, _ = doc.text(val, x + 22, y + 7, "sansb", 19, GH["text"])
        g.append(v)

    # tiles: the budget
    tiles = [("27 tools", "under 5,000 tokens, all of them"), ("400 chars", "of screen text by default"),
             ("100%", "refs kept over 200 re-layouts"), ("0", "model calls in the server")]
    ty, th, gap = 486, 92, 18
    tw = (BW - 2 * X0 - 3 * gap) / 4
    for i, (big, small) in enumerate(tiles):
        x = X0 + i * (tw + gap)
        g.append('<rect x="%s" y="%s" width="%s" height="%s" rx="12" fill="%s" stroke="%s"/>'
                 % (f(x, 1), ty, f(tw, 1), th, GH["tile"], GH["tileStroke"]))
        bg_, _ = doc.text(big, x + 22, ty + 48, "num", 38, GH["text"])
        sm, _ = doc.text(small, x + 22, ty + 76, "sans", 17, GH["dim"])
        g += [bg_, sm]
    ft, _ = doc.text("Preview 0.1.0  ·  Apache-2.0  ·  github.com/Sohwhat6420/agentroid-mcp", X0, BH - 34, "mono", 16, GH["dim"])
    g.append(ft)
    b.append('<g transform="translate(%s %s) scale(%s)">%s</g>' % (f(ox, 2), f(oy, 2), f(s, 5), "".join(g)))
    return doc


def write(path, doc):
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(doc.render())
    print("%7.1f KB  %s" % (os.path.getsize(path) / 1024, os.path.relpath(path, ROOT)))


def main():
    for name, T in THEMES.items():
        write(os.path.join(MEDIA, "hero-%s.svg" % name), hero(T))
    write(os.path.join(HERE, "banner-1280x640.svg"), banner(1280, 640))
    write(os.path.join(HERE, "banner-1200x627.svg"), banner(1200, 627))


if __name__ == "__main__":
    main()
