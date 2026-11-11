"""Unknown -> test: does a warm `claude -p` stream-json session take image blocks, and does
every turn end with its own `result` event? Two turns, one PNG. Pass: turn 1 reads the number, turn 2 remembers it.

    python policy-v3/probe_stream_json.py [model]
"""

from __future__ import annotations

import io
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import claude_session as cs  # noqa: E402


def png_with_number(n: int) -> bytes:
    from PIL import Image, ImageDraw, ImageFont

    im = Image.new("RGB", (360, 200), (240, 240, 240))
    d = ImageDraw.Draw(im)
    try:
        f = ImageFont.truetype("arial.ttf", 120)
    except OSError:
        f = ImageFont.load_default()
    d.text((90, 30), str(n), fill=(200, 0, 0), font=f)
    b = io.BytesIO()
    im.save(b, format="PNG")
    return b.getvalue()


def main() -> int:
    model = sys.argv[1] if len(sys.argv) > 1 else "claude-sonnet-5-5"
    s = cs.ClaudeSession(model, "Reply with one JSON object only.", timeout_s=120)
    try:
        t0 = time.monotonic()
        a = s.ask('What number is drawn in the image? {"n": <number>}', [png_with_number(73)])
        t1 = time.monotonic()
        b = s.ask('What was the number, plus one? {"n": <number>}')
        t2 = time.monotonic()
    finally:
        s.close()
    ok = '73' in a and '74' in b
    print(json.dumps({"pass": ok, "turn1": a, "turn2": b, "turn1_s": round(t1 - t0, 1), "turn2_s": round(t2 - t1, 1),
                      "usage": [r.get("usage") for r in s.log], "cost_usd": [r.get("cost_usd") for r in s.log]}, indent=1))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
