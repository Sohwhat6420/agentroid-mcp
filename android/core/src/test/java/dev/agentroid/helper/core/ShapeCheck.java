package dev.agentroid.helper.core;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Hand-written validator for the tree shape documented on {@link TreeSerializer}. Works on parsed JSON (the output
 * after a write/parse round trip), so it checks what the daemon would actually receive. Unknown keys are errors.
 */
final class ShapeCheck {
    private ShapeCheck() { }

    private static final Set<String> TOP = set("windows", "nodes", "clickable", "truncated");
    private static final Set<String> WIN = set("windowId", "type", "layer", "active", "focused", "bounds", "root");
    private static final Set<String> NODE_REQ = set("cls", "pkg", "bounds", "clickable", "longClickable", "focusable", "focused",
            "editable", "enabled", "visible", "checkable", "checked", "scrollable", "important");
    private static final Set<String> NODE_BOOLS = set("clickable", "longClickable", "focusable", "focused", "editable", "enabled",
            "visible", "checkable", "checked", "scrollable", "important");
    private static final Set<String> NODE_OPT = set("id", "text", "desc", "password", "range", "truncated", "children");

    private static Set<String> set(String... s) { return new HashSet<String>(Arrays.asList(s)); }

    static List<String> validate(Object tree) {
        List<String> errs = new ArrayList<String>();
        if (!(tree instanceof Map)) { errs.add("tree is not an object"); return errs; }
        Map<?, ?> m = (Map<?, ?>) tree;
        keys(m, TOP, TOP, "tree", errs);
        isInt(m.get("nodes"), "tree.nodes", errs);
        isInt(m.get("clickable"), "tree.clickable", errs);
        isBool(m.get("truncated"), "tree.truncated", errs);
        Object ws = m.get("windows");
        if (!(ws instanceof List)) { errs.add("tree.windows is not an array"); return errs; }
        int i = 0;
        for (Object w : (List<?>) ws) window(w, "windows[" + (i++) + "]", errs);
        return errs;
    }

    private static void window(Object w, String p, List<String> errs) {
        if (!(w instanceof Map)) { errs.add(p + " is not an object"); return; }
        Map<?, ?> m = (Map<?, ?>) w;
        Set<String> req = set("windowId", "type", "layer", "active", "focused", "root");
        keys(m, req, WIN, p, errs);
        isInt(m.get("windowId"), p + ".windowId", errs);
        isStr(m.get("type"), p + ".type", errs);
        isInt(m.get("layer"), p + ".layer", errs);
        isBool(m.get("active"), p + ".active", errs);
        isBool(m.get("focused"), p + ".focused", errs);
        if (m.containsKey("bounds")) bounds(m.get("bounds"), p + ".bounds", errs);
        node(m.get("root"), p + ".root", 0, errs);
    }

    private static void node(Object n, String p, int depth, List<String> errs) {
        if (!(n instanceof Map)) { errs.add(p + " is not an object"); return; }
        if (depth > TreeSerializer.MAX_DEPTH + 1) { errs.add(p + " is deeper than the limit"); return; }
        Map<?, ?> m = (Map<?, ?>) n;
        Set<String> all = new HashSet<String>(NODE_REQ);
        all.addAll(NODE_OPT);
        keys(m, NODE_REQ, all, p, errs);
        isStr(m.get("cls"), p + ".cls", errs);
        isStr(m.get("pkg"), p + ".pkg", errs);
        for (String b : NODE_BOOLS) isBool(m.get(b), p + "." + b, errs);
        bounds(m.get("bounds"), p + ".bounds", errs);
        for (String s : new String[] {"id", "text", "desc"}) if (m.containsKey(s)) isStr(m.get(s), p + "." + s, errs);
        if (m.containsKey("password")) {
            if (!Boolean.TRUE.equals(m.get("password"))) errs.add(p + ".password must be true when present");
            if (m.containsKey("text")) errs.add(p + " is a password field but carries text");
        }
        if (m.containsKey("truncated") && !Boolean.TRUE.equals(m.get("truncated"))) errs.add(p + ".truncated must be true when present");
        if (m.containsKey("range")) {
            Object r = m.get("range");
            if (!(r instanceof Map)) errs.add(p + ".range is not an object");
            else {
                keys((Map<?, ?>) r, set("min", "max", "current"), set("min", "max", "current"), p + ".range", errs);
                for (String k : new String[] {"min", "max", "current"}) isNum(((Map<?, ?>) r).get(k), p + ".range." + k, errs);
            }
        }
        if (m.containsKey("children")) {
            Object c = m.get("children");
            if (!(c instanceof List) || ((List<?>) c).isEmpty()) errs.add(p + ".children must be a non-empty array");
            else {
                int i = 0;
                for (Object ch : (List<?>) c) node(ch, p + ".children[" + (i++) + "]", depth + 1, errs);
            }
        }
    }

    private static void bounds(Object b, String p, List<String> errs) {
        if (!(b instanceof Map)) { errs.add(p + " is not an object"); return; }
        Set<String> k = set("l", "t", "r", "b");
        keys((Map<?, ?>) b, k, k, p, errs);
        for (String s : k) isInt(((Map<?, ?>) b).get(s), p + "." + s, errs);
    }

    private static void keys(Map<?, ?> m, Set<String> required, Set<String> allowed, String p, List<String> errs) {
        for (String r : required) if (!m.containsKey(r)) errs.add(p + " is missing '" + r + "'");
        for (Object k : m.keySet()) if (!allowed.contains(k)) errs.add(p + " has unknown key '" + k + "'");
    }

    private static void isInt(Object v, String p, List<String> errs) { if (!(v instanceof Long)) errs.add(p + " is not an integer: " + v); }
    private static void isNum(Object v, String p, List<String> errs) { if (!(v instanceof Long) && !(v instanceof Double)) errs.add(p + " is not a number: " + v); }
    private static void isStr(Object v, String p, List<String> errs) { if (!(v instanceof String)) errs.add(p + " is not a string: " + v); }
    private static void isBool(Object v, String p, List<String> errs) { if (!(v instanceof Boolean)) errs.add(p + " is not a boolean: " + v); }
}
