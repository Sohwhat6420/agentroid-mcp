"""Builds the banner's background pictures: a photo-style phone on a desk, with the REAL phone screen put into it.

    python docs/media/src/build_banner_photo.py

Inputs (this folder):
  banner-photo-dark.png, banner-photo-light.png   generated pictures (ChatGPT image generation, 14 Nov 2026) of a
                                                  phone on a desk whose screen is flat magenta. The phone body and
                                                  the desk are generated art; nothing on them is a claim.
  phone-home-480.jpg                              the real Xiaomi POCO X6 Pro home screen (adb screencap, 14 Nov 2026)
Outputs (this folder):
  banner-bg-dark.jpg, banner-bg-light.jpg         1280 x 640, the real screen keyed into the magenta area
  banner-photo.json                               where the screen sits in the 1280 x 640 box (build_readme_art_r2.py reads it)

The magenta is keyed by colour distance, not an exact match (the generator's magenta is only near #FF00FF), and only
inside the screen's own box, so a faint pink cast elsewhere is never treated as screen. The camera punch-hole is not
magenta, so it stays. Needs pillow and numpy; starts no child process.
"""
import json
import os

import numpy as np
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
W, H = 1280, 640
SCALE = 0.66          # generated picture -> banner pixels
SCREEN_LEFT = 930.0   # where the screen's left edge lands in the banner
TOP_CROP = 8          # banner pixels cut from the top of the scaled picture
KEEP_FROM = 500       # source x: everything right of this keeps its shape; the calm part left of it is stretched
QUALITY = {"dark": 86, "light": 84}
GLOW_KEEP = {"dark": 0.38, "light": 1.0}   # share of the picture's green glow that stays


def key(img):
    """alpha (0..1) of 'this pixel is screen', and the screen's box."""
    r, g, b = (img[..., i].astype("float32") for i in range(3))
    mag = np.minimum(r, b) - g
    hard = (mag > 150) & (np.abs(r - b) < 70)
    ys, xs = np.where(hard)
    x0, x1, y0, y1 = xs.min(), xs.max() + 1, ys.min(), ys.max() + 1
    a = np.clip(mag / 235.0, 0, 1)
    box = np.zeros_like(a)
    box[max(y0 - 3, 0):y1 + 3, max(x0 - 3, 0):x1 + 3] = 1
    return a * box, (int(x0), int(y0), int(x1 - x0), int(y1 - y0))


def composite(name):
    src = np.asarray(Image.open(os.path.join(HERE, "banner-photo-%s.png" % name)).convert("RGB")).astype("float32")
    a, (sx, sy, sw, sh) = key(src)
    shot = Image.open(os.path.join(HERE, "phone-home-480.jpg")).convert("RGB")
    shot = shot.resize((sw, round(sw * shot.height / shot.width)), Image.LANCZOS).crop((0, 0, sw, sh))   # top-anchored
    layer = np.zeros_like(src)
    layer[sy:sy + sh, sx:sx + sw] = np.asarray(shot, dtype="float32")
    # what was under the magenta at soft edges: take the magenta back out, so no pink fringe is left on the bezel
    M = np.array([252.0, 4.0, 252.0], dtype="float32")
    a3 = a[..., None]
    under = np.where(a3 < 0.97, (src - a3 * M) / np.maximum(1 - a3, 0.03), 0)
    out = np.clip(under, 0, 255) * (1 - a3) + layer * a3
    # the faint pink the magenta screen cast on the desk and frame: pull red and blue back to green's level
    r, g, b = out[..., 0], out[..., 1], out[..., 2]
    spill = np.clip(np.minimum(r, b) - g - 3, 0, None) * (a < 0.02)
    near = np.zeros_like(spill)
    near[max(sy - 40, 0):, max(sx - 260, 0):sx + sw + 260] = 1
    spill *= near
    out[..., 0] -= spill * 0.85
    out[..., 2] -= spill * 0.85
    # the generated picture's neon under-glow reads as a gaming render: take most of the extra green out of the
    # desk and the frame (never the screen), so what is left is a quiet tint
    if GLOW_KEEP[name] < 1:
        r, g, b = out[..., 0], out[..., 1], out[..., 2]
        extra = np.clip(g - np.maximum(r, b), 0, None) * (a < 0.02)
        out[..., 1] -= extra * (1 - GLOW_KEEP[name])
    full = Image.fromarray(np.clip(out, 0, 255).astype("uint8"))

    sw_all, sh_all = round(full.width * SCALE), round(full.height * SCALE)
    scaled = full.resize((sw_all, sh_all), Image.LANCZOS)
    off = SCREEN_LEFT - sx * SCALE
    cut = round(KEEP_FROM * SCALE)
    join = round(cut + off)
    canvas = Image.new("RGB", (W, sh_all))
    canvas.paste(scaled.crop((0, 0, cut, sh_all)).resize((join, sh_all), Image.BICUBIC), (0, 0))
    canvas.paste(scaled.crop((cut, 0, sw_all, sh_all)), (join, 0))
    canvas = canvas.crop((0, TOP_CROP, W, TOP_CROP + H))
    p = os.path.join(HERE, "banner-bg-%s.jpg" % name)
    canvas.save(p, quality=QUALITY[name], optimize=True, progressive=True)
    box = dict(x=round(SCREEN_LEFT, 1), y=round(sy * SCALE - TOP_CROP, 1), w=round(sw * SCALE, 1), h=round(sh * SCALE, 1),
               r=round(32 * SCALE, 1), bezel=round(13 * SCALE, 1))
    print(p, os.path.getsize(p), box)
    return box


if __name__ == "__main__":
    boxes = {n: composite(n) for n in ("dark", "light")}
    with open(os.path.join(HERE, "banner-photo.json"), "w", encoding="utf-8", newline="\n") as fh:
        json.dump(boxes, fh, indent=2)
        fh.write("\n")
