"""Writes the SYNTHETIC getevent fixtures for task 2.15 (no phone, no personal data).

No real POCO capture exists in this folder (fixtures/getevent/poco-x6-19oct.txt was never handed to
this environment), so every file here is synthetic, in the formats of `getevent -pli` and `getevent -lt`
(system/core/toolbox/getevent.c). Device names are placeholders. Deterministic: run it again and
the files come out byte-identical.

    python gen_fixtures.py        (writes next to this file)
"""
import os
import random

HERE = os.path.dirname(os.path.abspath(__file__))
rng = random.Random(215)


def dev(n, name, bus, keys=(), abs_=(), rel=(), props=()):
    out = [f"add device {n}: /dev/input/event{n}",
           f"  bus:      {bus:04x}", "  vendor    0000", "  product   0000", "  version   0000",
           f'  name:     "{name}"', '  location: ""', '  id:       ""', "  version:  1.0.1", "  events:"]
    if keys:
        out.append("    KEY (0001): " + "  ".join(f"{k:<20}" for k in keys).rstrip())
    if rel:
        out.append("    REL (0002): " + "  ".join(f"{k:<20}" for k in rel).rstrip())
    for i, a in enumerate(abs_):
        lead = "    ABS (0003): " if i == 0 else " " * 16
        out.append(f"{lead}{a:<21} : value 0, min 0, max 4095, fuzz 0, flat 0, resolution 0")
    out.append("  input props:")
    out += [f"    {p}" for p in props] if props else ["    <none>"]
    return out


MT = ["ABS_MT_SLOT", "ABS_MT_TOUCH_MAJOR", "ABS_MT_POSITION_X", "ABS_MT_POSITION_Y", "ABS_MT_TRACKING_ID", "ABS_MT_DISTANCE"]

BASE_MAP = (
    dev(0, "synthetic-pmic-pwrkey", 0, keys=["KEY_POWER"])
    + dev(1, "synthetic-gpio-keys", 0x19, keys=["KEY_VOLUMEDOWN", "KEY_VOLUMEUP"])
    + dev(2, "synthetic-proximity", 0, abs_=["ABS_DISTANCE"])
    + dev(3, "synthetic-accelerometer", 0, abs_=["ABS_X", "ABS_Y", "ABS_Z"], props=["INPUT_PROP_ACCELEROMETER"])
    + dev(4, "synthetic-touchpad-decoy", 0x18, keys=["BTN_TOUCH", "BTN_TOOL_FINGER"], abs_=["ABS_MT_POSITION_X", "ABS_MT_POSITION_Y", "ABS_MT_TRACKING_ID"], props=["INPUT_PROP_POINTER"])
    + dev(5, "synthetic-touchscreen", 0x18, keys=["KEY_WAKEUP", "BTN_TOUCH", "BTN_TOOL_FINGER"], abs_=MT, props=["INPUT_PROP_DIRECT"])
    + dev(6, "synthetic-stylus-digitiser", 0x18, keys=["BTN_TOUCH", "BTN_TOOL_PEN", "BTN_STYLUS"], abs_=["ABS_X", "ABS_Y", "ABS_PRESSURE", "ABS_DISTANCE"], props=["INPUT_PROP_DIRECT"])
    # pre-existing virtual devices that LOOK like touchscreens (every touch rule but the virtual one)
    + dev(7, "uinput-touch-injector", 0x06, keys=["BTN_TOUCH"], abs_=MT, props=["INPUT_PROP_DIRECT"])
    + dev(8, "hid-generic-panel", 0x06, keys=["BTN_TOUCH"], abs_=MT, props=["INPUT_PROP_DIRECT"])
)

# after scrcpy starts in UHID mode: its own device, plus a hot-plugged look-alike on a USB bus
HOT_MAP = BASE_MAP + dev(9, "scrcpy", 0x06, keys=["KEY_A", "KEY_B", "BTN_LEFT", "BTN_RIGHT", "BTN_TOUCH"], rel=["REL_X", "REL_Y", "REL_WHEEL"], abs_=MT, props=["INPUT_PROP_DIRECT"]) \
    + dev(10, "panel-0001", 0x03, keys=["BTN_TOUCH"], abs_=MT, props=["INPUT_PROP_DIRECT"])


class Stream:
    def __init__(self, t0):
        self.t = t0  # microseconds
        self.lines = []

    def ev(self, node, ty, code, value):
        s, us = divmod(self.t, 1_000_000)
        self.lines.append(f"[{s:8d}.{us:06d}] /dev/input/event{node}: {ty:<12} {code:<20} {value}")

    def syn(self, node):
        self.ev(node, "EV_SYN", "SYN_REPORT", "00000000")

    def adv(self, ms):
        self.t += int(ms * 1000)

    def note(self, text):
        self.lines.append(f"# {text}")


tid = [0x40]


def next_tid():
    tid[0] += 1
    return f"{tid[0]:08x}"


def finger(s, node=5, moves=0, btn=True, fingers=1):
    for _ in range(fingers):
        s.ev(node, "EV_ABS", "ABS_MT_TRACKING_ID", next_tid())
        s.ev(node, "EV_ABS", "ABS_MT_POSITION_X", f"{rng.randrange(50, 1150):08x}")
        s.ev(node, "EV_ABS", "ABS_MT_POSITION_Y", f"{rng.randrange(50, 2650):08x}")
    if btn:
        s.ev(node, "EV_KEY", "BTN_TOUCH", "DOWN")
        s.ev(node, "EV_KEY", "BTN_TOOL_FINGER", "DOWN")
    s.syn(node)
    for _ in range(moves):
        s.adv(8)
        s.ev(node, "EV_ABS", "ABS_MT_POSITION_Y", f"{rng.randrange(50, 2650):08x}")
        s.syn(node)
    s.adv(rng.randrange(40, 120))
    for _ in range(fingers):
        s.ev(node, "EV_ABS", "ABS_MT_TRACKING_ID", "ffffffff")
    if btn:
        s.ev(node, "EV_KEY", "BTN_TOUCH", "UP")
        s.ev(node, "EV_KEY", "BTN_TOOL_FINGER", "UP")
    s.syn(node)


def pen_hover(s, contact):
    s.ev(6, "EV_KEY", "BTN_TOOL_PEN", "DOWN")
    s.ev(6, "EV_ABS", "ABS_DISTANCE", "00000009")
    s.syn(6)
    s.adv(30)
    s.ev(6, "EV_KEY", "BTN_STYLUS", "DOWN")  # side button while hovering: never a touch
    s.syn(6)
    s.adv(20)
    s.ev(6, "EV_KEY", "BTN_STYLUS", "UP")
    s.syn(6)
    if contact:
        s.adv(40)
        s.ev(6, "EV_ABS", "ABS_DISTANCE", "00000000")
        s.ev(6, "EV_ABS", "ABS_PRESSURE", "00000300")
        s.ev(6, "EV_KEY", "BTN_TOUCH", "DOWN")
        s.syn(6)
        s.adv(90)
        s.ev(6, "EV_KEY", "BTN_TOUCH", "UP")
        s.syn(6)
    s.adv(30)
    s.ev(6, "EV_KEY", "BTN_TOOL_PEN", "UP")
    s.syn(6)


def noise(s):
    kind = rng.randrange(7)
    if kind == 0:
        s.ev(1, "EV_KEY", "KEY_VOLUMEUP", "DOWN"); s.syn(1); s.adv(80)
        s.ev(1, "EV_KEY", "KEY_VOLUMEUP", "UP"); s.syn(1)
    elif kind == 1:
        s.ev(0, "EV_KEY", "KEY_POWER", "DOWN"); s.syn(0); s.adv(90)
        s.ev(0, "EV_KEY", "KEY_POWER", "UP"); s.syn(0)
    elif kind == 2:
        s.ev(2, "EV_ABS", "ABS_DISTANCE", "00000000"); s.syn(2); s.adv(400)
        s.ev(2, "EV_ABS", "ABS_DISTANCE", "00000005"); s.syn(2)
    elif kind == 3:
        for _ in range(3):
            for a in ("ABS_X", "ABS_Y", "ABS_Z"):
                s.ev(3, "EV_ABS", a, f"{rng.randrange(0, 4095):08x}")
            s.syn(3); s.adv(20)
    elif kind == 4:
        finger(s, node=4)  # touchpad decoy
    elif kind == 5:
        pen_hover(s, contact=False)
    else:
        # hover over the touchscreen (a tracking id with a distance): not a contact
        s.ev(5, "EV_ABS", "ABS_MT_TRACKING_ID", next_tid())
        s.ev(5, "EV_ABS", "ABS_MT_DISTANCE", "00000004")
        s.syn(5); s.adv(60)
        s.ev(5, "EV_ABS", "ABS_MT_TRACKING_ID", "ffffffff")
        s.syn(5)


def human_touch(s, i):
    kinds = ["tap"] * 11 + ["swipe"] * 3 + ["pen", "pen", "wake", "mt_only", "pinch", "tap"]
    k = kinds[i]
    s.note(f"human touch {i + 1}: {k}")
    if k == "tap":
        finger(s)
    elif k == "swipe":
        finger(s, moves=12)
    elif k == "pen":
        pen_hover(s, contact=True)
    elif k == "wake":
        s.ev(5, "EV_KEY", "KEY_WAKEUP", "DOWN"); s.syn(5); s.adv(15)
        s.ev(5, "EV_KEY", "KEY_WAKEUP", "UP"); s.syn(5)
    elif k == "mt_only":
        finger(s, btn=False)
    elif k == "pinch":
        finger(s, moves=6, fingers=2)


def write(name, lines):
    with open(os.path.join(HERE, name), "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines) + "\n")


def human_stream():
    s = Stream(t0=20_000_000_000)
    s.note("SYNTHETIC getevent -lt: 20 human touches with button, sensor, hover and touchpad noise between them")
    for i in range(20):
        for _ in range(rng.randrange(1, 4)):
            noise(s); s.adv(rng.randrange(200, 900))
        human_touch(s, i)
        s.adv(rng.randrange(2500, 7000))
    return s.lines


def injected_stream():
    s = Stream(t0=30_000_000_000)
    s.note("SYNTHETIC getevent -lt: 500 agent actions, none of them human")
    n = 0
    # input tap/swipe and dispatchGesture go through InputManager / the a11y gesture injector:
    # nothing is written to any evdev node, so only background sensor and key noise appears.
    for path in ("input", "dispatchGesture", "scrcpy-inject"):
        for _ in range(125):
            n += 1
            s.note(f"action {n}: {path}")
            if rng.random() < 0.3:
                noise_safe(s)
            s.adv(rng.randrange(150, 600))
    # scrcpy UHID: scrcpy adds its own uhid device mid-stream; events land on it and on the
    # pre-existing virtual look-alikes (bus 0006), never on event5/event6.
    s.lines += ["add device 9: /dev/input/event9", '  name:     "scrcpy"']
    s.lines += ["add device 10: /dev/input/event10", '  name:     "panel-0001"']
    for i in range(125):
        n += 1
        node = (9, 7, 8, 10)[i % 4]
        s.note(f"action {n}: uhid on event{node}")
        if node == 9 and i % 8 == 0:
            s.ev(9, "EV_KEY", "BTN_LEFT", "DOWN"); s.ev(9, "EV_REL", "REL_X", "00000004"); s.syn(9); s.adv(20)
            s.ev(9, "EV_KEY", "BTN_LEFT", "UP"); s.syn(9)
        else:
            finger(s, node=node)
        s.adv(rng.randrange(150, 600))
    assert n == 500
    return s.lines


def noise_safe(s):
    # background noise only: no touchpad, no hover (those are covered in the human file)
    kind = rng.randrange(3)
    if kind == 0:
        s.ev(1, "EV_KEY", "KEY_VOLUMEDOWN", "DOWN"); s.syn(1); s.adv(80)
        s.ev(1, "EV_KEY", "KEY_VOLUMEDOWN", "UP"); s.syn(1)
    elif kind == 1:
        s.ev(2, "EV_ABS", "ABS_DISTANCE", "00000000"); s.syn(2)
    else:
        for a in ("ABS_X", "ABS_Y", "ABS_Z"):
            s.ev(3, "EV_ABS", a, f"{rng.randrange(0, 4095):08x}")
        s.syn(3)


if __name__ == "__main__":
    write("synthetic-map.txt", ["# SYNTHETIC getevent -pli: placeholder names, no identifiers"] + BASE_MAP)
    write("synthetic-map-after-scrcpy.txt", ["# SYNTHETIC getevent -pli after scrcpy --keyboard=uhid --mouse=uhid and a hot-plugged look-alike"] + HOT_MAP)
    write("synthetic-human-20.txt", human_stream())
    write("synthetic-injected-500.txt", injected_stream())
