package dev.agentroid.helper.core;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** Fake phone side for the JVM tests. */
final class Fakes {
    private Fakes() { }

    static class Node implements UiNode {
        String cls = "android.view.View", pkg = "com.example", id, text, desc;
        int[] bounds = {0, 0, 10, 10};
        boolean clickable, longClickable, focusable, focused, editable, enabled = true, visible = true;
        boolean checkable, checked, scrollable, important = true, password;
        double[] range;
        final List<Node> kids = new ArrayList<Node>();
        boolean nullChild;     // report one extra child that is null
        static int released;   // how many nodes were released

        Node cls(String c) { cls = c; return this; }
        Node id(String i) { id = i; return this; }
        Node text(String t) { text = t; return this; }
        Node desc(String d) { desc = d; return this; }
        Node click() { clickable = true; return this; }
        Node add(Node n) { kids.add(n); return this; }

        public String className() { return cls; }
        public String packageName() { return pkg; }
        public String viewId() { return id; }
        public String text() { return text; }
        public String contentDescription() { return desc; }
        public int[] bounds() { return bounds; }
        public boolean clickable() { return clickable; }
        public boolean longClickable() { return longClickable; }
        public boolean focusable() { return focusable; }
        public boolean focused() { return focused; }
        public boolean editable() { return editable; }
        public boolean enabled() { return enabled; }
        public boolean visible() { return visible; }
        public boolean checkable() { return checkable; }
        public boolean checked() { return checked; }
        public boolean scrollable() { return scrollable; }
        public boolean important() { return important; }
        public boolean password() { return password; }
        public double[] range() { return range; }
        public int childCount() { return kids.size() + (nullChild ? 1 : 0); }
        public UiNode child(int i) { return i < kids.size() ? kids.get(i) : null; }
        public void release() { released++; }
    }

    static class Window implements UiWindow {
        int id = 1, layer = 0;
        String type = "APPLICATION";
        boolean active = true, focused = true;
        int[] bounds = {0, 0, 1080, 2400};
        Node root;
        Window(Node root) { this.root = root; }
        public int id() { return id; }
        public String type() { return type; }
        public int layer() { return layer; }
        public boolean active() { return active; }
        public boolean focused() { return focused; }
        public int[] bounds() { return bounds; }
        public UiNode root() { return root; }
    }

    static class A11y implements Ports.A11y {
        final List<String> calls = Collections.synchronizedList(new ArrayList<String>());
        List<Window> windows = new ArrayList<Window>();
        RuntimeException boom;

        public List<? extends UiWindow> windows() { calls.add("windows"); return windows; }
        public void tap(double x, double y) {
            calls.add("tap " + x + " " + y);
            if (boom != null) throw boom;
        }
        public void swipe(double x1, double y1, double x2, double y2, long ms) {
            calls.add("swipe " + x1 + " " + y1 + " " + x2 + " " + y2 + " " + ms);
        }
        public Map<String, Object> setValue(String by, String selector, double value) {
            calls.add("set_value " + by + " " + selector + " " + value);
            Map<String, Object> m = new LinkedHashMap<String, Object>();
            m.put("requested", value);
            m.put("now", value);
            return m;
        }
        public Ports.Screenshot screenshot() {
            calls.add("screenshot");
            return new Ports.Screenshot(2, 2, new byte[] {1, 2, 3});
        }
    }

    static class Ime implements Ports.Ime {
        final List<byte[]> committed = new ArrayList<byte[]>();
        public void commitText(String text) {
            committed.add(text.getBytes(StandardCharsets.UTF_8));
        }
    }

    static class Env implements HelperEnv {
        boolean imeEnabled = true, imeSelected = true;
        public String helperVersion() { return "0.1-test"; }
        public int sdkInt() { return 36; }
        public boolean imeEnabled() { return imeEnabled; }
        public boolean imeSelected() { return imeSelected; }
    }

    /** Everything wired together the way LinkService wires it. */
    static class Rig {
        final StubChallenge challenge = new StubChallenge();
        final LinkSession session = new LinkSession(challenge);
        final Dispatcher dispatcher = new Dispatcher(session);
        final PortRegistry ports = new PortRegistry();
        final A11y a11y = new A11y();
        final Ime ime = new Ime();
        final Env env = new Env();

        Rig() {
            ports.setA11y(a11y);
            ports.setIme(ime);
            HelperMethods.register(dispatcher, ports, env);
        }

        Rig authed() {
            Object r = call("{\"jsonrpc\":\"2.0\",\"id\":\"auth\",\"method\":\"auth.challenge\",\"params\":{\"nonce\":\""
                    + NONCE + "\"}}");
            T.ok(T.obj(r).containsKey("result"), "rig auth should pass: " + Json.write(r));
            return this;
        }

        /** Sends one JSON text as a UTF-8 frame, returns the parsed response object. */
        Object call(String json) { return callBytes(json.getBytes(StandardCharsets.UTF_8)); }

        Object callBytes(byte[] frame) {
            byte[] out = dispatcher.handleFrame(frame);
            try {
                return Json.parse(Frames.decodeUtf8Strict(out), TreeSerializer.MAX_JSON_NESTING);
            } catch (Exception e) {
                throw new AssertionError("response is not valid UTF-8 JSON: " + e);
            }
        }

        Object req(Object id, String method, String paramsJson) {
            String idJson = id instanceof String ? "\"" + id + "\"" : String.valueOf(id);
            return call("{\"jsonrpc\":\"2.0\",\"id\":" + idJson + ",\"method\":\"" + method + "\""
                    + (paramsJson == null ? "" : ",\"params\":" + paramsJson) + "}");
        }
    }

    static final String NONCE = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    static long errCode(Object response) {
        Object e = T.obj(response).get("error");
        if (e == null) throw new AssertionError("expected an error but got " + Json.write(response));
        return ((Long) T.obj(e).get("code")).longValue();
    }
}
