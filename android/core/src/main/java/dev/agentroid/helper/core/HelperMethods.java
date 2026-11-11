package dev.agentroid.helper.core;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;

/**
 * The helper's method table. Kinds are decided here, in one place:
 * <pre>
 *   STATUS   ping, status
 *   AUTH     auth.challenge {nonce}
 *   OBSERVE  tree, screenshot
 *   ACT      tap {x,y}, swipe {x1,y1,x2,y2,ms?}, set_value {id|text|desc, value}, type_text {text}
 * </pre>
 * Params are validated strictly: unknown keys, wrong types, NaN and out-of-range numbers are INVALID_PARAMS.
 */
public final class HelperMethods {
    private HelperMethods() { }

    public static final int MAX_TYPE_TEXT_CHARS = 10000;
    private static final double MAX_COORD = 100000;
    private static final Pattern NONCE = Pattern.compile("[0-9a-f]{32,128}");

    public static void register(final Dispatcher d, final PortRegistry ports, final HelperEnv env) {
        final LinkSession session = d.session();

        d.register("ping", Dispatcher.Kind.STATUS, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p);
                return map("pong", Boolean.TRUE, "t", Long.valueOf(System.currentTimeMillis()));
            }
        });

        d.register("status", Dispatcher.Kind.STATUS, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p);
                Map<String, Object> link = new LinkedHashMap<String, Object>();
                link.put("state", session.state().wire());
                link.put("auth", session.challenge().algorithm());
                link.put("notification_visible", session.notificationVisible());
                if (session.state() == LinkSession.State.NEEDS_REPAIR) link.put("fix", session.fix());
                Map<String, Object> out = new LinkedHashMap<String, Object>();
                out.put("helper", map("version", env.helperVersion(), "sdk", Integer.valueOf(env.sdkInt())));
                out.put("link", link);
                out.put("sources", SourceChoice.report(true, ports.a11yBound(), env.imeEnabled(), env.imeSelected()));
                return out;
            }
        });

        d.register("auth.challenge", Dispatcher.Kind.AUTH, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p, "nonce");
                String nonce = str(p, "nonce");
                if (!NONCE.matcher(nonce).matches()) throw invalid("nonce must be 32..128 lower-case hex characters");
                Challenge.Outcome o = session.authenticate(nonce);
                if (!o.ok) {
                    Map<String, Object> data = new LinkedHashMap<String, Object>();
                    data.put("state", "needs_repair");
                    data.put("fix", LinkSession.REPAIR_FIX);
                    data.put("reason", o.reason);
                    throw new HandlerException(ErrorCodes.NEEDS_REPAIR, "needs_repair: challenge failed", data);
                }
                Map<String, Object> out = new LinkedHashMap<String, Object>();
                out.put("state", session.state().wire());
                out.put("alg", session.challenge().algorithm());
                if (o.signature != null) out.put("signature", o.signature);
                return out;
            }
        });

        d.register("tree", Dispatcher.Kind.OBSERVE, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p);
                try {
                    return TreeSerializer.serialize(ports.a11y().windows());
                } catch (Ports.PortException e) {
                    throw device(e);
                }
            }
        });

        d.register("screenshot", Dispatcher.Kind.OBSERVE, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p);
                try {
                    Ports.Screenshot s = ports.a11y().screenshot();
                    return map("format", "png", "width", Integer.valueOf(s.width), "height", Integer.valueOf(s.height),
                            "png_b64", Payload.b64(s.png));
                } catch (Ports.PortException e) {
                    throw device(e);
                }
            }
        });

        d.register("tap", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p, "x", "y");
                double x = coord(p, "x");
                double y = coord(p, "y");
                try {
                    ports.a11y().tap(x, y);
                } catch (Ports.PortException e) {
                    throw device(e);
                }
                return map("done", Boolean.TRUE);
            }
        });

        d.register("swipe", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p, "x1", "y1", "x2", "y2", "ms");
                double x1 = coord(p, "x1");
                double y1 = coord(p, "y1");
                double x2 = coord(p, "x2");
                double y2 = coord(p, "y2");
                long ms = 300;
                if (p.containsKey("ms")) {
                    double v = num(p, "ms");
                    if (v != Math.rint(v) || v < 1 || v > 10000) throw invalid("ms must be an integer from 1 to 10000");
                    ms = (long) v;
                }
                try {
                    ports.a11y().swipe(x1, y1, x2, y2, ms);
                } catch (Ports.PortException e) {
                    throw device(e);
                }
                return map("done", Boolean.TRUE);
            }
        });

        d.register("set_value", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p, "id", "text", "desc", "value");
                String by = null;
                for (String k : new String[] {"id", "text", "desc"}) {
                    if (p.containsKey(k)) {
                        if (by != null) throw invalid("give exactly one of id, text, desc");
                        by = k;
                    }
                }
                if (by == null) throw invalid("give exactly one of id, text, desc");
                String selector = str(p, by);
                if (selector.isEmpty()) throw invalid(by + " must not be empty");
                double value = num(p, "value");
                try {
                    return ports.a11y().setValue(by, selector, value);
                } catch (Ports.PortException e) {
                    throw device(e);
                }
            }
        });

        d.register("type_text", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                only(p, "text");
                String text = str(p, "text");
                if (text.isEmpty()) throw invalid("text must not be empty");
                if (text.length() > MAX_TYPE_TEXT_CHARS) throw invalid("text longer than " + MAX_TYPE_TEXT_CHARS + " characters");
                try {
                    ports.ime().commitText(text);
                } catch (Ports.PortException e) {
                    throw device(e);
                }
                // The text itself is never echoed or logged; only counts come back.
                return map("committed_chars", Integer.valueOf(text.codePointCount(0, text.length())),
                        "committed_utf8_bytes", Integer.valueOf(text.getBytes(StandardCharsets.UTF_8).length));
            }
        });
    }

    // ------------------------------------------------------------------ param helpers

    private static void only(Map<String, Object> p, String... allowed) throws HandlerException {
        Set<String> ok = new HashSet<String>(Arrays.asList(allowed));
        for (String k : p.keySet()) {
            if (!ok.contains(k)) throw invalid("unknown param '" + k + "'");
        }
    }

    private static HandlerException invalid(String m) { return new HandlerException(ErrorCodes.INVALID_PARAMS, m); }

    private static HandlerException device(Ports.PortException e) {
        return new HandlerException(ErrorCodes.DEVICE_ERROR, e.reason + ": " + e.getMessage(), map("reason", e.reason));
    }

    private static String str(Map<String, Object> p, String k) throws HandlerException {
        Object v = p.get(k);
        if (!(v instanceof String)) throw invalid(k + " must be a string");
        return (String) v;
    }

    private static double num(Map<String, Object> p, String k) throws HandlerException {
        Object v = p.get(k);
        if (!(v instanceof Long) && !(v instanceof Double)) throw invalid(k + " must be a number");
        double d = ((Number) v).doubleValue();
        if (Double.isNaN(d) || Double.isInfinite(d)) throw invalid(k + " must be finite");
        return d;
    }

    private static double coord(Map<String, Object> p, String k) throws HandlerException {
        double d = num(p, k);
        if (d < 0 || d > MAX_COORD) throw invalid(k + " must be between 0 and " + (long) MAX_COORD);
        return d;
    }

    private static Map<String, Object> map(Object... kv) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        for (int i = 0; i + 1 < kv.length; i += 2) m.put((String) kv[i], kv[i + 1]);
        return m;
    }

    /** Used by tests to know the registered method names. */
    public static List<String> methodNames() {
        return Arrays.asList("ping", "status", "auth.challenge", "tree", "screenshot", "tap", "swipe", "set_value", "type_text");
    }
}
