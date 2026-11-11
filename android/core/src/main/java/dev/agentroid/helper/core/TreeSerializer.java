/*
 * Apache-2.0 section 4(b) notice: the per-node walk in node() follows the S4 spike's node(), which was adapted from
 * google/artemis HierarchyDumper.java (packages/artemis-accessibility-helper, com.artemis.helper; Copyright Google LLC,
 * Apache License 2.0; upstream has no NOTICE file). It has been modified by the Agentroid authors (new wire shape,
 * depth cap, password redaction, UiNode interface). The licence text is in the LICENSE file at the repository root.
 */
package dev.agentroid.helper.core;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Node tree to the wire shape. Shape (keys in this order; "?" = only when present):
 * <pre>
 * { "windows": [ { "windowId":int, "type":str, "layer":int, "active":bool, "focused":bool, "bounds"?:{l,t,r,b}, "root":NODE } ],
 *   "nodes":int, "clickable":int, "truncated":bool }
 * NODE = { "cls":str, "pkg":str, "id"?:str, "text"?:str, "desc"?:str, "bounds":{l,t,r,b},
 *          "clickable":bool, "longClickable":bool, "focusable":bool, "focused":bool, "editable":bool, "enabled":bool,
 *          "visible":bool, "checkable":bool, "checked":bool, "scrollable":bool, "important":bool,
 *          "password"?:true (then no "text"), "range"?:{min,max,current}, "truncated"?:true, "children"?:[NODE] }
 * </pre>
 * The scaffold's schemas folder has no tree schema, so this is the S4 spike's proven node shape with the additions
 * noted above. The test suite checks serialiser output against a hand-written validator of exactly this shape.
 */
public final class TreeSerializer {
    private TreeSerializer() { }

    /**
     * Deepest node level kept. A node at level d sits 2d+5 JSON levels down (object, array, object per level, plus its
     * bounds object), so 60 gives at most 125 levels: inside serde_json's default recursion limit of 128, which a Rust
     * hub would otherwise hit on a deep tree. Deeper nodes are cut and the parent is marked "truncated". The S4 spike
     * used 80; its trees were read by a Python/Java peer with no such limit.
     */
    public static final int MAX_DEPTH = 60;
    /** What a reader of the output must allow. */
    public static final int MAX_JSON_NESTING = 128;
    public static final int MAX_NODES = 50000;

    private static final class Count {
        int nodes;
        int clickable;
        boolean truncated;
    }

    public static Map<String, Object> serialize(List<? extends UiWindow> windows) {
        Count c = new Count();
        List<Object> ws = new ArrayList<Object>();
        for (UiWindow w : windows) {
            Map<String, Object> m = new LinkedHashMap<String, Object>();
            m.put("windowId", w.id());
            m.put("type", w.type());
            m.put("layer", w.layer());
            m.put("active", w.active());
            m.put("focused", w.focused());
            int[] b = w.bounds();
            if (b != null) m.put("bounds", rect(b));
            UiNode root = w.root();
            if (root != null) {
                try {
                    m.put("root", node(root, 0, c));
                } finally {
                    root.release();
                }
            } else {
                continue; // a window with no readable root has nothing to report
            }
            ws.add(m);
        }
        Map<String, Object> out = new LinkedHashMap<String, Object>();
        out.put("windows", ws);
        out.put("nodes", c.nodes);
        out.put("clickable", c.clickable);
        out.put("truncated", c.truncated);
        return out;
    }

    private static Map<String, Object> node(UiNode n, int depth, Count c) {
        c.nodes++;
        Map<String, Object> o = new LinkedHashMap<String, Object>();
        o.put("cls", str(n.className()));
        o.put("pkg", str(n.packageName()));
        String id = n.viewId();
        if (id != null) o.put("id", id);
        boolean password = n.password();
        String text = n.text();
        if (text != null && !password) o.put("text", text);
        String desc = n.contentDescription();
        if (desc != null) o.put("desc", desc);
        int[] b = n.bounds();
        o.put("bounds", rect(b == null ? new int[] {0, 0, 0, 0} : b));
        boolean click = n.clickable();
        if (click) c.clickable++;
        o.put("clickable", click);
        o.put("longClickable", n.longClickable());
        o.put("focusable", n.focusable());
        o.put("focused", n.focused());
        o.put("editable", n.editable());
        o.put("enabled", n.enabled());
        o.put("visible", n.visible());
        o.put("checkable", n.checkable());
        o.put("checked", n.checked());
        o.put("scrollable", n.scrollable());
        o.put("important", n.important());
        if (password) o.put("password", Boolean.TRUE);
        double[] r = n.range();
        if (r != null && r.length == 3) {
            Map<String, Object> rm = new LinkedHashMap<String, Object>();
            rm.put("min", r[0]);
            rm.put("max", r[1]);
            rm.put("current", r[2]);
            o.put("range", rm);
        }
        int cc = n.childCount();
        if (cc > 0) {
            if (depth >= MAX_DEPTH) {
                o.put("truncated", Boolean.TRUE);
                c.truncated = true;
            } else {
                List<Object> kids = new ArrayList<Object>();
                for (int i = 0; i < cc; i++) {
                    if (c.nodes >= MAX_NODES) {
                        o.put("truncated", Boolean.TRUE);
                        c.truncated = true;
                        break;
                    }
                    UiNode ch = n.child(i);
                    if (ch == null) continue;
                    try {
                        kids.add(node(ch, depth + 1, c));
                    } finally {
                        ch.release();
                    }
                }
                if (!kids.isEmpty()) o.put("children", kids);
            }
        }
        return o;
    }

    private static Map<String, Object> rect(int[] b) {
        Map<String, Object> r = new LinkedHashMap<String, Object>();
        r.put("l", b[0]);
        r.put("t", b[1]);
        r.put("r", b[2]);
        r.put("b", b[3]);
        return r;
    }

    private static String str(String s) { return s == null ? "" : s; }
}
