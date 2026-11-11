package dev.agentroid.helper.core;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * The source-choice report: which path each kind of work should take, from three facts. The laptop uses it to pick
 * the helper or fall back to uiautomator and adb input automatically (2.5 "uiautomator fallback with automatic
 * source choice"); the helper reports the facts, the fallback itself runs on the laptop over adb.
 */
public final class SourceChoice {
    private SourceChoice() { }

    /**
     * @param helperAvailable the helper app is installed and its link is up
     * @param a11yBound       the accessibility service is bound (tree, gestures, screenshots)
     * @param imeEnabled      the Agentroid input method is enabled in system settings
     * @param imeSelected     ...and is the current keyboard (commitText needs that)
     */
    public static Map<String, Object> report(boolean helperAvailable, boolean a11yBound, boolean imeEnabled, boolean imeSelected) {
        List<Object> reasons = new ArrayList<Object>();
        String tree;
        String gestures;
        String screenshot;
        if (!helperAvailable) {
            tree = "uiautomator";
            gestures = "adb_input";
            screenshot = "adb_screencap";
            reasons.add("helper not available: using uiautomator, adb input and screencap");
        } else if (!a11yBound) {
            tree = "uiautomator";
            gestures = "adb_input";
            screenshot = "adb_screencap";
            reasons.add("accessibility service not bound: using uiautomator, adb input and screencap");
        } else {
            tree = "helper";
            gestures = "helper";
            screenshot = "helper";
        }
        String type;
        if (helperAvailable && imeEnabled && imeSelected) {
            type = "ime";
        } else if (helperAvailable && a11yBound) {
            type = "a11y_set_text";
            if (!imeEnabled) reasons.add("IME not enabled: typing falls back to accessibility set-text, which may not take every Unicode character");
            else reasons.add("IME enabled but not the current keyboard: typing falls back to accessibility set-text");
        } else {
            type = "adb_input";
            reasons.add("no helper path for typing: adb input cannot type non-ASCII text");
        }
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("helper_available", helperAvailable);
        m.put("a11y_bound", a11yBound);
        m.put("ime_enabled", imeEnabled);
        m.put("ime_selected", imeSelected);
        m.put("tree_source", tree);
        m.put("gesture_path", gestures);
        m.put("screenshot_path", screenshot);
        m.put("type_path", type);
        m.put("reasons", reasons);
        return m;
    }
}
