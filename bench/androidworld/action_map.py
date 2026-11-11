"""AndroidWorld action space -> Agentroid MCP tool calls.

Pure: no I/O, no android_world import. An action is a dict (or an object with the same attributes)
shaped like android_world's ``JSONAction``; ``plan()`` turns one into the ordered Agentroid tool
calls that reproduce what android_world's own ``execute_adb_action`` does, and says what the
harness itself must do (store an answer, end the episode).

Source of truth for the semantics: google-research/android_world, ``env/json_action.py``,
``env/actuation.py`` and ``env/adb_utils.py`` (Apache-2.0), read 26 Oct 2026. The map is in
ACTION-SPACE.md; every line there that says "unverified" is something the emulator run (U-A2)
must confirm.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any, Mapping, Sequence

ACTION_TYPES = (
    "answer", "click", "double_tap", "input_text", "keyboard_enter", "long_press", "navigate_back",
    "navigate_home", "open_app", "scroll", "status", "swipe", "unknown", "wait",
)
SCROLL_DIRECTIONS = ("left", "right", "down", "up")

# android_world sleeps 1.0 s after the click of an input_text and after its clear.
INPUT_PAUSE_S = 1.0
# actuation.py swipe() duration for the `swipe` action.
SWIPE_MS = 500
# Upper bound on backspaces when clear_text has to be done by key presses (see ACTION-SPACE.md).
MAX_CLEAR_KEYS = 60

# android_world adb_utils._PATTERN_TO_ACTIVITY, package part only, in the source's dict order.
# get_adb_activity lower-cases the name and takes the FIRST key for which re.match(key, name)
# succeeds, so order is part of the behaviour.
_PATTERN_TO_PACKAGE: tuple[tuple[str, str], ...] = (
    ("google chrome|chrome", "com.android.chrome"),
    ("google chat", "com.google.android.apps.dynamite"),
    ("settings|system settings", "com.android.settings"),
    ("youtube|yt", "com.google.android.youtube"),
    ("google play|play store|gps", "com.android.vending"),
    ("gmail|gemail|google mail|google email|google mail client", "com.google.android.gm"),
    ("google maps|gmaps|maps|google map", "com.google.android.apps.maps"),
    ("google photos|gphotos|photos|google photo|google pics|google images", "com.google.android.apps.photos"),
    ("google calendar|gcal", "com.google.android.calendar"),
    ("camera", "com.android.camera2"),
    ("audio recorder", "com.dimowner.audiorecorder"),
    ("google drive|gdrive|drive", "com.google.android.apps.docs"),
    ("google keep|gkeep|keep", "com.google.android.keep"),
    ("grubhub", "com.grubhub.android"),
    ("tripadvisor", "com.tripadvisor.tripadvisor"),
    ("starbucks", "com.starbucks.mobilecard"),
    ("google docs|gdocs|docs", "com.google.android.apps.docs.editors.docs"),
    ("google sheets|gsheets|sheets", "com.google.android.apps.docs.editors.sheets"),
    ("google slides|gslides|slides", "com.google.android.apps.docs.editors.slides"),
    ("google voice|voice", "com.google.android.apps.googlevoice"),
    ("clock", "com.google.android.deskclock"),
    ("google search|google", "com.google.android.googlequicksearchbox"),
    ("contacts", "com.google.android.contacts"),
    ("facebook|fb", "com.facebook.katana"),
    ("whatsapp|wa", "com.whatsapp"),
    ("instagram|ig", "com.instagram.android"),
    ("twitter|tweet", "com.twitter.android"),
    ("snapchat|sc", "com.snapchat.android"),
    ("telegram|tg", "org.telegram.messenger"),
    ("linkedin", "com.linkedin.android"),
    ("spotify|spot", "com.spotify.music"),
    ("netflix", "com.netflix.mediaclient"),
    ("amazon shopping|amazon|amzn", "com.amazon.mShop.android.shopping"),
    ("tiktok|tt", "com.zhiliaoapp.musically"),
    ("discord", "com.discord"),
    ("reddit", "com.reddit.frontpage"),
    ("pinterest", "com.pinterest"),
    ("android world", "com.example.androidworld"),
    ("files", "com.google.android.documentsui"),
    ("markor", "net.gsantner.markor"),
    ("clipper", "ca.zgrs.clipper"),
    ("messages", "com.google.android.apps.messaging"),
    ("simple sms messenger|simple sms", "com.simplemobiletools.smsmessenger"),
    ("dialer|phone", "com.google.android.dialer"),
    ("simple calendar pro|simple calendar", "com.simplemobiletools.calendar.pro"),
    ("simple gallery pro|simple gallery", "com.simplemobiletools.gallery.pro"),
    ("miniwob", "com.google.androidenv.miniwob"),
    ("simple draw pro", "com.simplemobiletools.draw.pro"),
    ("pro expense|pro expense app", "com.arduia.expense"),
    ("broccoli|broccoli app|broccoli recipe app|recipe app", "com.flauschcode.broccoli"),
    ("caa|caa test|context aware access", "com.google.ccc.hosted.contextawareaccess.thirdpartyapp"),
    ("osmand", "net.osmand"),
    ("tasks|tasks app|tasks.org:", "org.tasks"),
    ("open tracks sports tracker|activity tracker|open tracks|opentracks", "de.dennisguse.opentracks"),
    ("joplin|joplin app", "net.cozic.joplin"),
    ("vlc|vlc app|vlc player", "org.videolan.vlc"),
    ("retro music|retro|retro player", "code.name.monkey.retromusic"),
)

# android_world launches these with an android.intent.action.VIEW on a built-in URI rather than by
# activity; the URIs are not in this table, so the map refuses the ones with no package row
# ("contacts" has one and resolves to it; calendar, browser, email and gallery are refused).
_VIEW_INTENT_NAMES = ("calendar", "browser", "contacts", "email", "gallery")

_PACKAGE_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_]*(\.[A-Za-z0-9_]+)+$")


class ActionError(ValueError):
    """The action is not valid under android_world's own rules, or cannot be mapped."""


@dataclass(frozen=True)
class ToolCall:
    """One Agentroid MCP tool call."""

    name: str
    args: dict[str, Any]
    # seconds the harness waits after this call (android_world's own pauses)
    sleep_after_s: float = 0.0


@dataclass
class Plan:
    """What one android_world action becomes."""

    calls: list[ToolCall] = field(default_factory=list)
    # `answer`: the text the harness stores in env.interaction_cache (no device call)
    answer: str | None = None
    # `status`: "complete" or "infeasible" (ends the episode, no device call)
    status: str | None = None
    # notes for the run log: where this mapping departs from android_world
    notes: list[str] = field(default_factory=list)


def package_for(app_name: str) -> str:
    """android_world's get_adb_activity/launch_app resolution, as a package name."""
    if not app_name or not app_name.strip():
        raise ActionError("open_app needs an app_name")
    lower = app_name.lower()
    for pattern, package in _PATTERN_TO_PACKAGE:
        if re.match(pattern, lower):
            return package
    if lower in _VIEW_INTENT_NAMES:
        raise ActionError(
            f"{app_name!r} is launched by android_world with a VIEW intent on a built-in URI, which this map does not carry"
        )
    # android_world falls back to `monkey -p <name> 1`, i.e. it treats the name as a package
    if _PACKAGE_RE.match(app_name.strip()):
        return app_name.strip()
    raise ActionError(f"unknown app {app_name!r}: not in android_world's table and not a package name")


def _get(a: Any, key: str) -> Any:
    if isinstance(a, Mapping):
        return a.get(key)
    return getattr(a, key, None)


def _box(el: Any) -> tuple[int, int, int, int]:
    """(x_min, y_min, x_max, y_max) of an android_world UIElement's bbox_pixels (object or dict)."""
    b = _get(el, "bbox_pixels")
    if b is None:
        raise ActionError("the element has no pixel bounds")
    vals = [_get(b, k) for k in ("x_min", "y_min", "x_max", "y_max")]
    if any(v is None for v in vals):
        raise ActionError("the element has no pixel bounds")
    return int(vals[0]), int(vals[1]), int(vals[2]), int(vals[3])


def _center(el: Any) -> tuple[int, int]:
    x0, y0, x1, y1 = _box(el)
    return int((x0 + x1) / 2), int((y0 + y1) / 2)


def _target(action: Any, elements: Sequence[Any] | None, what: str) -> tuple[int, int]:
    """The tap point of click/double_tap/long_press: element centre, else the given x, y."""
    index = _get(action, "index")
    if index is not None:
        if elements is None or not 0 <= int(index) < len(elements):
            raise ActionError(f"{what}: element index {index} is outside the screen's element list")
        return _center(elements[int(index)])
    x, y = _get(action, "x"), _get(action, "y")
    if x is None or y is None:
        raise ActionError(f"{what}: neither an element index nor x and y were given")
    return int(x), int(y)


def _pt(x: int, y: int) -> dict[str, Any]:
    return {"x": x, "y": y, "space": "device"}


def plan(action: Any, elements: Sequence[Any] | None = None, screen: tuple[int, int] | None = None) -> Plan:
    """Map one android_world action to Agentroid tool calls.

    ``elements`` is android_world's ``state.ui_elements`` for the screen the agent chose the action
    on (indexes refer to it); ``screen`` is the logical (width, height), needed only for swipes.
    """
    kind = _get(action, "action_type")
    if kind not in ACTION_TYPES:
        raise ActionError(f"action_type {kind!r} is not an android_world action")
    index = _get(action, "index")
    if index is not None and (_get(action, "x") is not None or _get(action, "y") is not None):
        raise ActionError("an action takes an index or an x, y pair, not both")

    if kind == "answer":
        text = _get(action, "text")
        return Plan(answer="" if text is None else str(text))
    if kind == "status":
        status = _get(action, "goal_status")
        if status not in ("complete", "infeasible"):
            raise ActionError(f"status needs goal_status complete or infeasible, not {status!r}")
        return Plan(status=status)
    if kind == "unknown":
        raise ActionError("an unknown action cannot be executed")

    if kind == "click":
        x, y = _target(action, elements, "click")
        return Plan([ToolCall("tap", _pt(x, y))])
    if kind == "long_press":
        x, y = _target(action, elements, "long_press")
        return Plan([ToolCall("long_press", _pt(x, y))])
    if kind == "double_tap":
        x, y = _target(action, elements, "double_tap")
        return Plan(
            [ToolCall("tap", _pt(x, y)), ToolCall("tap", _pt(x, y))],
            notes=["double_tap is two tap calls (no double-tap tool); the gap between them is one MCP round trip, so unverified against Android's 300 ms double-tap window"],
        )

    if kind == "input_text":
        text = _get(action, "text")
        if text is None or text == "":
            return Plan(notes=["empty text: android_world logs a warning and does nothing"])
        calls: list[ToolCall] = []
        notes: list[str] = []
        has_target = index is not None or (_get(action, "x") is not None and _get(action, "y") is not None)
        if has_target:
            x, y = _target(action, elements, "input_text")
            calls.append(ToolCall("tap", _pt(x, y), INPUT_PAUSE_S))
        clear = _get(action, "clear_text")
        if clear is None and index is not None and elements is not None:
            # Default (7 Nov 2026): a field that already holds text is cleared first, as the policy prompt says input_text
            # does. android_world's own default (append) made a 22-character name pile up five times in one field.
            # {"clear_text": false} still appends.
            held = _get(elements[int(index)], "text")
            if isinstance(held, str) and held.strip():
                clear = True
                notes.append("clear_text defaulted on: the field already held text (send clear_text false to append)")
        if clear:
            # android_world: select-all (keycodes 113+29) then delete. The key tool takes one key per call.
            n = MAX_CLEAR_KEYS
            if index is not None and elements is not None:
                existing = _get(elements[int(index)], "text")
                if isinstance(existing, str):
                    n = min(MAX_CLEAR_KEYS, len(existing) + 2)
            calls.append(ToolCall("key", {"name": "KEYCODE_MOVE_END"}))
            calls.extend(ToolCall("key", {"name": "delete"}) for _ in range(n))
            calls[-1] = ToolCall("key", {"name": "delete"}, INPUT_PAUSE_S)
            notes.append("clear_text is MOVE_END plus backspaces (no select-all combo tool); text longer than 60 characters is only partly cleared")
        # android_world always presses Enter after typing
        calls.append(ToolCall("type_text", {"text": str(text), "submit": True}))
        return Plan(calls, notes=notes)

    if kind == "keyboard_enter":
        return Plan([ToolCall("key", {"name": "enter"})])
    if kind == "navigate_back":
        return Plan([ToolCall("key", {"name": "back"})])
    if kind == "navigate_home":
        return Plan([ToolCall("key", {"name": "home"})])
    if kind == "wait":
        return Plan([ToolCall("wait", {"seconds": 1})])
    if kind == "open_app":
        return Plan([ToolCall("open_app", {"package": package_for(_get(action, "app_name") or "")})])

    direction = _get(action, "direction")
    if direction not in SCROLL_DIRECTIONS:
        raise ActionError(f"direction must be one of {SCROLL_DIRECTIONS}, not {direction!r}")

    if kind == "scroll":
        # android_world tests `if action.index:`, so there index 0 silently means "no index". Deliberately NOT kept: the
        # policy names element 0 on purpose (a brightness slider was element 0 in the pilot) and gets a page scroll instead.
        if index is not None:
            if elements is None or not 0 <= int(index) < len(elements):
                raise ActionError(f"scroll: element index {index} is outside the screen's element list")
            x0, y0, x1, y1 = _box(elements[int(index)])
            if screen is not None:
                x0, y0, x1, y1 = max(0, x0), max(0, y0), min(screen[0], x1), min(screen[1], y1)
            cx, cy = (x0 + x1) // 2, (y0 + y1) // 2
            end = {"down": (cx, y0), "up": (cx, y1), "right": (x0, cy), "left": (x1, cy)}[direction]
            return Plan(
                [ToolCall("swipe", {"x1": cx, "y1": cy, "x2": end[0], "y2": end[1], "space": "device"})],
                notes=["scroll inside an element is a swipe from its centre to its edge, as android_world does"],
            )
        # Agentroid's scroll(direction) swipes the same way: `down` moves the finger up, revealing content below.
        return Plan(
            [ToolCall("scroll", {"direction": direction})],
            notes=["scroll without an index uses Agentroid's own page scroll (0.75h to 0.3h, 350 ms), not android_world's centre-to-edge swipe"],
        )

    # swipe: the inverse of scroll, on the whole screen, 500 ms
    if screen is None:
        raise ActionError("swipe needs the screen size")
    w, h = screen[0] - 1, screen[1] - 1  # last pixel, so Agentroid's frame clamp has nothing to report
    cx, cy = w // 2, h // 2
    # finger path for each direction, per actuation.py: down = top to bottom, up = bottom to top,
    # left = left edge to right edge, right = right edge to left edge
    path = {
        "down": (cx, 0, cx, h),
        "up": (cx, h, cx, 0),
        "left": (0, cy, w, cy),
        "right": (w, cy, 0, cy),
    }[direction]
    return Plan([ToolCall("swipe", {"x1": path[0], "y1": path[1], "x2": path[2], "y2": path[3], "ms": SWIPE_MS, "space": "device"})])
