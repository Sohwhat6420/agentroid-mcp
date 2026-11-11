"""Offline tests for action_map.py. Run: python -B test_action_map.py (from this folder)."""

from __future__ import annotations

import sys
import unittest

import action_map as am
from action_map import ActionError, plan


def el(x0, y0, x1, y1, text=None):
    return {"bbox_pixels": {"x_min": x0, "y_min": y0, "x_max": x1, "y_max": y1}, "text": text}


ELS = [el(0, 0, 100, 100), el(100, 200, 300, 400, "hello"), el(0, 500, 1080, 2000)]
SCREEN = (1080, 2400)


def names(p):
    return [(c.name, c.args) for c in p.calls]


class Clicks(unittest.TestCase):
    def test_click_index_taps_the_centre_of_the_element(self):
        p = plan({"action_type": "click", "index": 1}, ELS)
        self.assertEqual(names(p), [("tap", {"x": 200, "y": 300, "space": "device"})])

    def test_click_xy_passes_the_point(self):
        p = plan({"action_type": "click", "x": 10, "y": 20})
        self.assertEqual(names(p), [("tap", {"x": 10, "y": 20, "space": "device"})])

    def test_index_out_of_range_and_both_forms_are_refused(self):
        with self.assertRaises(ActionError):
            plan({"action_type": "click", "index": 3}, ELS)
        with self.assertRaises(ActionError):
            plan({"action_type": "click", "index": 1, "x": 1, "y": 1}, ELS)
        with self.assertRaises(ActionError):
            plan({"action_type": "click"}, ELS)

    def test_index_zero_is_a_real_index_for_click(self):
        self.assertEqual(names(plan({"action_type": "click", "index": 0}, ELS))[0][1]["x"], 50)

    def test_long_press_and_double_tap(self):
        self.assertEqual(plan({"action_type": "long_press", "x": 1, "y": 2}).calls[0].name, "long_press")
        d = plan({"action_type": "double_tap", "x": 1, "y": 2})
        self.assertEqual([c.name for c in d.calls], ["tap", "tap"])
        self.assertTrue(d.notes)

    def test_element_without_bounds_is_refused(self):
        with self.assertRaises(ActionError):
            plan({"action_type": "click", "index": 0}, [{"bbox_pixels": None}])


class InputText(unittest.TestCase):
    def test_taps_target_pauses_types_and_always_submits(self):
        p = plan({"action_type": "input_text", "text": "abc", "index": 0}, ELS)  # element 0 holds no text
        self.assertEqual([c.name for c in p.calls], ["tap", "type_text"])
        self.assertEqual(p.calls[0].sleep_after_s, 1.0)
        self.assertEqual(p.calls[1].args, {"text": "abc", "submit": True})

    def test_a_field_that_holds_text_is_cleared_by_default(self):
        p = plan({"action_type": "input_text", "text": "x", "index": 1}, ELS)  # element 1 holds "hello"
        self.assertEqual([c.name for c in p.calls], ["tap", "key"] + ["key"] * 7 + ["type_text"])
        self.assertEqual(p.calls[1].args, {"name": "KEYCODE_MOVE_END"})
        self.assertEqual(sum(1 for c in p.calls if c.args == {"name": "delete"}), len("hello") + 2)
        self.assertTrue(any("defaulted" in n for n in p.notes))

    def test_clear_text_false_still_appends(self):
        p = plan({"action_type": "input_text", "text": "x", "index": 1, "clear_text": False}, ELS)
        self.assertEqual([c.name for c in p.calls], ["tap", "type_text"])

    def test_whitespace_only_text_and_a_point_target_are_not_cleared_by_default(self):
        els = [el(0, 0, 100, 100, "   ")]
        self.assertEqual([c.name for c in plan({"action_type": "input_text", "text": "x", "index": 0}, els).calls], ["tap", "type_text"])
        self.assertEqual([c.name for c in plan({"action_type": "input_text", "text": "x", "x": 5, "y": 5}, ELS).calls], ["tap", "type_text"])

    def test_clear_text_deletes_the_existing_text_first(self):
        p = plan({"action_type": "input_text", "text": "x", "index": 1, "clear_text": True}, ELS)
        kinds = [c.name for c in p.calls]
        self.assertEqual(kinds[0], "tap")
        self.assertEqual(p.calls[1].args, {"name": "KEYCODE_MOVE_END"})
        # "hello" is 5 characters: 5 + 2 backspaces
        self.assertEqual(kinds.count("key"), 1 + 7)
        self.assertEqual(p.calls[-2].sleep_after_s, 1.0)
        self.assertEqual(kinds[-1], "type_text")
        self.assertTrue(p.notes)

    def test_clear_text_without_a_known_field_uses_the_cap(self):
        p = plan({"action_type": "input_text", "text": "x", "clear_text": True})
        self.assertEqual([c.name for c in p.calls].count("key"), 1 + am.MAX_CLEAR_KEYS)

    def test_no_target_types_into_the_focused_field(self):
        p = plan({"action_type": "input_text", "text": "abc"})
        self.assertEqual([c.name for c in p.calls], ["type_text"])

    def test_empty_text_does_nothing(self):
        p = plan({"action_type": "input_text", "text": "", "index": 1}, ELS)
        self.assertEqual(p.calls, [])
        self.assertTrue(p.notes)


class Keys(unittest.TestCase):
    def test_navigation_and_enter(self):
        self.assertEqual(names(plan({"action_type": "navigate_back"})), [("key", {"name": "back"})])
        self.assertEqual(names(plan({"action_type": "navigate_home"})), [("key", {"name": "home"})])
        self.assertEqual(names(plan({"action_type": "keyboard_enter"})), [("key", {"name": "enter"})])

    def test_wait_is_one_second(self):
        self.assertEqual(names(plan({"action_type": "wait"})), [("wait", {"seconds": 1})])


class Scrolls(unittest.TestCase):
    def test_page_scroll_uses_the_scroll_tool_with_the_same_direction_word(self):
        for d in am.SCROLL_DIRECTIONS:
            self.assertEqual(names(plan({"action_type": "scroll", "direction": d})), [("scroll", {"direction": d})])

    def test_scroll_in_an_element_swipes_centre_to_the_edge(self):
        # element 2 spans x 0..1080, y 500..2000; centre (540, 1250)
        down = plan({"action_type": "scroll", "direction": "down", "index": 2}, ELS, SCREEN).calls[0].args
        self.assertEqual((down["x1"], down["y1"], down["x2"], down["y2"]), (540, 1250, 540, 500))
        up = plan({"action_type": "scroll", "direction": "up", "index": 2}, ELS, SCREEN).calls[0].args
        self.assertEqual((up["x2"], up["y2"]), (540, 2000))
        right = plan({"action_type": "scroll", "direction": "right", "index": 2}, ELS, SCREEN).calls[0].args
        self.assertEqual((right["x2"], right["y2"]), (0, 1250))
        left = plan({"action_type": "scroll", "direction": "left", "index": 2}, ELS, SCREEN).calls[0].args
        self.assertEqual((left["x2"], left["y2"]), (1080, 1250))

    def test_scroll_index_zero_is_element_zero_unlike_android_world(self):
        p = plan({"action_type": "scroll", "direction": "down", "index": 0}, ELS, SCREEN)
        self.assertEqual(p.calls[0].name, "swipe", "android_world would page-scroll here; we honour the named element")

    def test_scroll_without_an_index_is_a_page_scroll(self):
        for action in ({"action_type": "scroll", "direction": "down"}, {"action_type": "scroll", "direction": "down", "index": None}):
            self.assertEqual(plan(action, ELS, SCREEN).calls[0].name, "scroll")

    def test_scroll_region_is_clamped_to_the_screen(self):
        els = [el(0, 0, 1, 1), el(-50, -50, 2000, 3000)]
        a = plan({"action_type": "scroll", "direction": "down", "index": 1}, els, SCREEN).calls[0].args
        self.assertEqual((a["x1"], a["y1"], a["x2"], a["y2"]), (540, 1200, 540, 0))

    def test_swipe_is_the_inverse_whole_screen_gesture_500ms(self):
        a = plan({"action_type": "swipe", "direction": "down"}, None, SCREEN).calls[0].args
        self.assertEqual((a["x1"], a["y1"], a["x2"], a["y2"], a["ms"]), (539, 0, 539, 2399, 500))
        a = plan({"action_type": "swipe", "direction": "left"}, None, SCREEN).calls[0].args
        self.assertEqual((a["x1"], a["x2"]), (0, 1079))
        a = plan({"action_type": "swipe", "direction": "right"}, None, SCREEN).calls[0].args
        self.assertEqual((a["x1"], a["x2"]), (1079, 0))
        with self.assertRaises(ActionError):
            plan({"action_type": "swipe", "direction": "down"})

    def test_bad_direction_is_refused(self):
        with self.assertRaises(ActionError):
            plan({"action_type": "scroll", "direction": "sideways"})


class Apps(unittest.TestCase):
    def test_open_app_resolves_the_name_like_android_world(self):
        for name, pkg in [
            ("Simple Calendar Pro", "com.simplemobiletools.calendar.pro"),
            ("settings", "com.android.settings"),
            ("Markor", "net.gsantner.markor"),
            ("Retro Music", "code.name.monkey.retromusic"),
            ("OpenTracks", "de.dennisguse.opentracks"),
            ("Joplin", "net.cozic.joplin"),
            ("Files", "com.google.android.documentsui"),
        ]:
            self.assertEqual(names(plan({"action_type": "open_app", "app_name": name})), [("open_app", {"package": pkg})], name)

    def test_first_key_in_dict_order_wins(self):
        # re.match is a prefix match: "google" alone is the search app, but "google maps" hits maps first
        self.assertEqual(am.package_for("Google Maps"), "com.google.android.apps.maps")
        self.assertEqual(am.package_for("google"), "com.google.android.googlequicksearchbox")

    def test_unknown_names_fall_back_to_a_package_or_fail(self):
        self.assertEqual(am.package_for("com.example.thing"), "com.example.thing")
        with self.assertRaises(ActionError):
            am.package_for("Unheard Of App")
        for view_only in ("calendar", "browser", "email", "gallery"):
            with self.assertRaises(ActionError):
                am.package_for(view_only)
        self.assertEqual(am.package_for("contacts"), "com.google.android.contacts")
        with self.assertRaises(ActionError):
            am.package_for("")

    def test_every_table_entry_is_a_package_name(self):
        for pattern, pkg in am._PATTERN_TO_PACKAGE:
            self.assertRegex(pkg, am._PACKAGE_RE, pattern)


class HarnessActions(unittest.TestCase):
    def test_answer_and_status_make_no_device_call(self):
        a = plan({"action_type": "answer", "text": 42})
        self.assertEqual((a.calls, a.answer), ([], "42"))
        s = plan({"action_type": "status", "goal_status": "complete"})
        self.assertEqual((s.calls, s.status), ([], "complete"))
        self.assertEqual(plan({"action_type": "status", "goal_status": "infeasible"}).status, "infeasible")
        with self.assertRaises(ActionError):
            plan({"action_type": "status", "goal_status": "done"})

    def test_unknown_and_invalid_types_are_refused(self):
        with self.assertRaises(ActionError):
            plan({"action_type": "unknown"})
        with self.assertRaises(ActionError):
            plan({"action_type": "teleport"})

    def test_attribute_style_actions_work_too(self):
        class A:
            action_type = "click"
            index = 1
            x = None
            y = None

        self.assertEqual(plan(A(), ELS).calls[0].args["x"], 200)

    def test_every_android_world_action_type_is_covered(self):
        for t in am.ACTION_TYPES:
            if t == "unknown":
                continue
            args = {"action_type": t, "x": 1, "y": 1, "text": "a", "direction": "up", "app_name": "settings", "goal_status": "complete"}
            plan(args, ELS, SCREEN)  # must not raise


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1], verbosity=1)
