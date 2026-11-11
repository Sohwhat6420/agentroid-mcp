package dev.agentroid.helper.core;

import java.util.Map;

/** The source-choice report over the three facts (plus whether the IME is the current keyboard). */
public class SourceChoiceTest {

    private static Map<String, Object> r(boolean helper, boolean a11y, boolean ime, boolean sel) {
        return SourceChoice.report(helper, a11y, ime, sel);
    }

    public void testEverythingUpUsesTheHelper() {
        Map<String, Object> m = r(true, true, true, true);
        T.eq("helper", m.get("tree_source"), "tree");
        T.eq("helper", m.get("gesture_path"), "gestures");
        T.eq("helper", m.get("screenshot_path"), "screenshot");
        T.eq("ime", m.get("type_path"), "typing");
        T.ok(T.arr(m.get("reasons")).isEmpty(), "nothing to explain");
        T.eq(Boolean.TRUE, m.get("helper_available"), "echoes helper_available");
        T.eq(Boolean.TRUE, m.get("a11y_bound"), "echoes a11y_bound");
        T.eq(Boolean.TRUE, m.get("ime_enabled"), "echoes ime_enabled");
    }

    public void testNoHelperFallsBackToUiautomatorAndAdb() {
        Map<String, Object> m = r(false, false, false, false);
        T.eq("uiautomator", m.get("tree_source"), "tree");
        T.eq("adb_input", m.get("gesture_path"), "gestures");
        T.eq("adb_screencap", m.get("screenshot_path"), "screenshot");
        T.eq("adb_input", m.get("type_path"), "typing");
        T.ok(!T.arr(m.get("reasons")).isEmpty(), "says why");
    }

    public void testHelperUpButAccessibilityUnboundFallsBack() {
        Map<String, Object> m = r(true, false, true, true);
        T.eq("uiautomator", m.get("tree_source"), "tree needs a11y");
        T.eq("adb_input", m.get("gesture_path"), "gestures need a11y");
        T.eq("ime", m.get("type_path"), "the IME does not need the a11y service");
    }

    public void testImeDisabledOrNotSelectedFallsBackToSetText() {
        Map<String, Object> off = r(true, true, false, false);
        T.eq("helper", off.get("tree_source"), "tree unaffected");
        T.eq("a11y_set_text", off.get("type_path"), "IME off");
        Map<String, Object> notSel = r(true, true, true, false);
        T.eq("a11y_set_text", notSel.get("type_path"), "IME enabled but not selected");
        T.ok(String.valueOf(notSel.get("reasons")).contains("not the current keyboard"), "reason names the cause");
    }

    public void testStatusMethodCarriesTheReport() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        Object s = rig.req(1, "status", null);
        T.eq("helper", T.at(s, "result", "sources", "tree_source"), "bound a11y -> helper tree");
        rig.ports.setA11y(null);
        rig.env.imeSelected = false;
        Object s2 = rig.req(2, "status", null);
        T.eq("uiautomator", T.at(s2, "result", "sources", "tree_source"), "a11y unbound -> uiautomator");
        T.eq(Boolean.FALSE, T.at(s2, "result", "sources", "a11y_bound"), "a11y_bound false");
        T.eq("0.1-test", T.at(s2, "result", "helper", "version"), "version");
    }
}
