package dev.agentroid.helper.runner;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.List;
import java.util.Map;

/**
 * The canonical byte form of a segment (schemas/segment.json, "description"). Rust twin: crates/segments/src/canonical.rs.
 * The two agree byte for byte; schemas/fixtures/segment/canonical.json pins it and both suites check it.
 *
 * Rules: no whitespace; object members sorted by the UTF-8 bytes of their keys; strings escape " and \ and
 * U+0000..U+001F only (\b \t \n \f \r short, others backslash-u00xx lower-case), everything else raw UTF-8;
 * numbers are 64-bit integers only (a Double is refused); true, false, null.
 * Input values are what dev.agentroid.helper.core.Json.parse returns: Map, List, String, Long, Double, Boolean, null.
 */
public final class Canonical {
    private Canonical() { }

    public static final String DOMAIN = "agentroid-segment/v1\n";

    /** The bytes the hub signs for a segment object. */
    public static byte[] signedBytes(Map<String, Object> segment) {
        return (DOMAIN + write(segment)).getBytes(StandardCharsets.UTF_8);
    }

    /** Canonical text of a JSON value. Throws IllegalArgumentException for a non-integer number or a non-JSON value. */
    public static String write(Object v) {
        StringBuilder b = new StringBuilder();
        write(b, v, "");
        return b.toString();
    }

    private static final Comparator<String> UTF8_ORDER = new Comparator<String>() {
        public int compare(String x, String y) {
            byte[] a = x.getBytes(StandardCharsets.UTF_8);
            byte[] b = y.getBytes(StandardCharsets.UTF_8);
            int n = Math.min(a.length, b.length);
            for (int i = 0; i < n; i++) {
                int d = (a[i] & 0xff) - (b[i] & 0xff);
                if (d != 0) return d;
            }
            return a.length - b.length;
        }
    };

    @SuppressWarnings("unchecked")
    private static void write(StringBuilder b, Object v, String path) {
        if (v == null) { b.append("null"); return; }
        if (v instanceof Boolean) { b.append(((Boolean) v).booleanValue() ? "true" : "false"); return; }
        if (v instanceof Long || v instanceof Integer) { b.append(((Number) v).longValue()); return; }
        if (v instanceof Number) throw new IllegalArgumentException((path.isEmpty() ? "/" : path) + ": only i64 integers are allowed in a segment");
        if (v instanceof String) { str(b, (String) v); return; }
        if (v instanceof Map) {
            Map<String, Object> m = (Map<String, Object>) v;
            List<String> keys = new ArrayList<String>(m.keySet());
            Collections.sort(keys, UTF8_ORDER);
            b.append('{');
            for (int i = 0; i < keys.size(); i++) {
                if (i > 0) b.append(',');
                str(b, keys.get(i));
                b.append(':');
                write(b, m.get(keys.get(i)), path + "/" + keys.get(i));
            }
            b.append('}');
            return;
        }
        if (v instanceof List) {
            List<Object> a = (List<Object>) v;
            b.append('[');
            for (int i = 0; i < a.size(); i++) {
                if (i > 0) b.append(',');
                write(b, a.get(i), path + "/" + i);
            }
            b.append(']');
            return;
        }
        throw new IllegalArgumentException(path + ": not a JSON value: " + v.getClass().getName());
    }

    private static void str(StringBuilder b, String s) {
        b.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (Character.isHighSurrogate(c)) {
                if (i + 1 >= s.length() || !Character.isLowSurrogate(s.charAt(i + 1))) throw new IllegalArgumentException("lone surrogate");
                b.append(c).append(s.charAt(++i));
                continue;
            }
            if (Character.isLowSurrogate(c)) throw new IllegalArgumentException("lone surrogate");
            switch (c) {
                case '"': b.append("\\\""); break;
                case '\\': b.append("\\\\"); break;
                case '\b': b.append("\\b"); break;
                case '\t': b.append("\\t"); break;
                case '\n': b.append("\\n"); break;
                case '\f': b.append("\\f"); break;
                case '\r': b.append("\\r"); break;
                default:
                    if (c < 0x20) {
                        b.append("\\u00").append(Character.forDigit(c >> 4, 16)).append(Character.forDigit(c & 0xf, 16));
                    } else {
                        b.append(c);
                    }
            }
        }
        b.append('"');
    }
}
