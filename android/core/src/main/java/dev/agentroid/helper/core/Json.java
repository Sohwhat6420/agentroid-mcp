package dev.agentroid.helper.core;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Small strict JSON reader/writer (pure Java, no org.json so it runs on a plain JVM).
 * Values: Map (LinkedHashMap), List (ArrayList), String, Long, Double, Boolean, null.
 * Strict on purpose: duplicate keys, trailing text, raw control characters, lone surrogate escapes,
 * leading zeros and nesting deeper than {@link #MAX_DEPTH} are all refused.
 */
public final class Json {
    public static final int MAX_DEPTH = 64;

    private Json() { }

    public static final class ParseException extends Exception {
        public ParseException(String m) { super(m); }
    }

    // ------------------------------------------------------------------ parse

    public static Object parse(String s) throws ParseException {
        return parse(s, MAX_DEPTH);
    }

    /** For trusted-size output we produced ourselves (tests parse tree responses, which nest deeper than requests may). */
    public static Object parse(String s, int maxDepth) throws ParseException {
        P p = new P(s, maxDepth);
        p.ws();
        Object v = p.value(0);
        p.ws();
        if (p.i != s.length()) throw new ParseException("trailing characters at " + p.i);
        return v;
    }

    private static final class P {
        final String s;
        final int maxDepth;
        int i;
        P(String s, int maxDepth) { this.s = s; this.maxDepth = maxDepth; }

        void ws() {
            while (i < s.length()) {
                char c = s.charAt(i);
                if (c == ' ' || c == '\t' || c == '\n' || c == '\r') i++; else break;
            }
        }

        Object value(int depth) throws ParseException {
            if (depth > maxDepth) throw new ParseException("nesting too deep");
            if (i >= s.length()) throw new ParseException("unexpected end");
            char c = s.charAt(i);
            switch (c) {
                case '{': return object(depth);
                case '[': return array(depth);
                case '"': return string();
                case 't': lit("true"); return Boolean.TRUE;
                case 'f': lit("false"); return Boolean.FALSE;
                case 'n': lit("null"); return null;
                default:
                    if (c == '-' || (c >= '0' && c <= '9')) return number();
                    throw new ParseException("unexpected character at " + i);
            }
        }

        void lit(String w) throws ParseException {
            if (!s.startsWith(w, i)) throw new ParseException("bad literal at " + i);
            i += w.length();
        }

        Map<String, Object> object(int depth) throws ParseException {
            Map<String, Object> m = new LinkedHashMap<String, Object>();
            i++;
            ws();
            if (peek() == '}') { i++; return m; }
            while (true) {
                ws();
                if (peek() != '"') throw new ParseException("object key expected at " + i);
                String k = string();
                if (m.containsKey(k)) throw new ParseException("duplicate key '" + k + "'");
                ws();
                if (peek() != ':') throw new ParseException("':' expected at " + i);
                i++;
                ws();
                m.put(k, value(depth + 1));
                ws();
                char c = peek();
                i++;
                if (c == ',') continue;
                if (c == '}') return m;
                throw new ParseException("',' or '}' expected at " + (i - 1));
            }
        }

        List<Object> array(int depth) throws ParseException {
            List<Object> l = new ArrayList<Object>();
            i++;
            ws();
            if (peek() == ']') { i++; return l; }
            while (true) {
                ws();
                l.add(value(depth + 1));
                ws();
                char c = peek();
                i++;
                if (c == ',') continue;
                if (c == ']') return l;
                throw new ParseException("',' or ']' expected at " + (i - 1));
            }
        }

        char peek() throws ParseException {
            if (i >= s.length()) throw new ParseException("unexpected end");
            return s.charAt(i);
        }

        String string() throws ParseException {
            i++; // opening quote
            StringBuilder b = new StringBuilder();
            while (true) {
                if (i >= s.length()) throw new ParseException("unterminated string");
                char c = s.charAt(i++);
                if (c == '"') break;
                if (c < 0x20) throw new ParseException("raw control character in string");
                if (c != '\\') { b.append(c); continue; }
                if (i >= s.length()) throw new ParseException("unterminated escape");
                char e = s.charAt(i++);
                switch (e) {
                    case '"': b.append('"'); break;
                    case '\\': b.append('\\'); break;
                    case '/': b.append('/'); break;
                    case 'b': b.append('\b'); break;
                    case 'f': b.append('\f'); break;
                    case 'n': b.append('\n'); break;
                    case 'r': b.append('\r'); break;
                    case 't': b.append('\t'); break;
                    case 'u': b.append((char) hex4()); break;
                    default: throw new ParseException("bad escape");
                }
            }
            // Escapes can build a lone surrogate; raw input cannot (it was UTF-8 decoded strictly upstream).
            for (int k = 0; k < b.length(); k++) {
                char c = b.charAt(k);
                if (Character.isHighSurrogate(c)) {
                    if (k + 1 < b.length() && Character.isLowSurrogate(b.charAt(k + 1))) k++;
                    else throw new ParseException("lone surrogate in string");
                } else if (Character.isLowSurrogate(c)) {
                    throw new ParseException("lone surrogate in string");
                }
            }
            return b.toString();
        }

        int hex4() throws ParseException {
            if (i + 4 > s.length()) throw new ParseException("short \\u escape");
            int v = 0;
            for (int k = 0; k < 4; k++) {
                int d = Character.digit(s.charAt(i + k), 16);
                if (d < 0) throw new ParseException("bad \\u escape");
                v = v * 16 + d;
            }
            i += 4;
            return v;
        }

        Object number() throws ParseException {
            int st = i;
            if (peek() == '-') i++;
            if (i >= s.length()) throw new ParseException("bad number");
            if (s.charAt(i) == '0') {
                i++;
            } else if (s.charAt(i) >= '1' && s.charAt(i) <= '9') {
                while (i < s.length() && Character.isDigit(s.charAt(i))) i++;
            } else {
                throw new ParseException("bad number at " + st);
            }
            boolean integral = true;
            if (i < s.length() && s.charAt(i) == '.') {
                integral = false;
                i++;
                int d0 = i;
                while (i < s.length() && Character.isDigit(s.charAt(i))) i++;
                if (i == d0) throw new ParseException("bad fraction at " + st);
            }
            if (i < s.length() && (s.charAt(i) == 'e' || s.charAt(i) == 'E')) {
                integral = false;
                i++;
                if (i < s.length() && (s.charAt(i) == '+' || s.charAt(i) == '-')) i++;
                int d0 = i;
                while (i < s.length() && Character.isDigit(s.charAt(i))) i++;
                if (i == d0) throw new ParseException("bad exponent at " + st);
            }
            String t = s.substring(st, i);
            if (integral) {
                try { return Long.valueOf(Long.parseLong(t)); } catch (NumberFormatException ignored) { /* falls to double */ }
            }
            double d = Double.parseDouble(t);
            if (Double.isInfinite(d) || Double.isNaN(d)) throw new ParseException("number out of range");
            return Double.valueOf(d);
        }
    }

    // ------------------------------------------------------------------ write

    public static String write(Object v) {
        StringBuilder b = new StringBuilder();
        write(b, v);
        return b.toString();
    }

    @SuppressWarnings("unchecked")
    private static void write(StringBuilder b, Object v) {
        if (v == null) { b.append("null"); return; }
        if (v instanceof String) { str(b, (String) v); return; }
        if (v instanceof Boolean) { b.append(((Boolean) v).booleanValue() ? "true" : "false"); return; }
        if (v instanceof Integer || v instanceof Long || v instanceof Short || v instanceof Byte) {
            b.append(((Number) v).longValue());
            return;
        }
        if (v instanceof Double || v instanceof Float) {
            double d = ((Number) v).doubleValue();
            if (Double.isNaN(d) || Double.isInfinite(d)) throw new IllegalArgumentException("NaN/Infinity is not JSON");
            if (d == Math.rint(d) && Math.abs(d) < 1e15) b.append((long) d); else b.append(d);
            return;
        }
        if (v instanceof Map) {
            b.append('{');
            boolean first = true;
            for (Map.Entry<Object, Object> e : ((Map<Object, Object>) v).entrySet()) {
                if (!first) b.append(',');
                first = false;
                str(b, String.valueOf(e.getKey()));
                b.append(':');
                write(b, e.getValue());
            }
            b.append('}');
            return;
        }
        if (v instanceof Iterable) {
            b.append('[');
            boolean first = true;
            for (Object o : (Iterable<Object>) v) {
                if (!first) b.append(',');
                first = false;
                write(b, o);
            }
            b.append(']');
            return;
        }
        throw new IllegalArgumentException("not JSON-serialisable: " + v.getClass().getName());
    }

    /** Writes a string. Lone surrogates (possible in Android text) become U+FFFD so UTF-8 encoding is never lossy by accident. */
    private static void str(StringBuilder b, String s) {
        b.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"': b.append("\\\""); break;
                case '\\': b.append("\\\\"); break;
                case '\n': b.append("\\n"); break;
                case '\r': b.append("\\r"); break;
                case '\t': b.append("\\t"); break;
                case '\b': b.append("\\b"); break;
                case '\f': b.append("\\f"); break;
                default:
                    if (c < 0x20) {
                        b.append(String.format("\\u%04x", (int) c));
                    } else if (Character.isHighSurrogate(c)) {
                        if (i + 1 < s.length() && Character.isLowSurrogate(s.charAt(i + 1))) {
                            b.append(c).append(s.charAt(i + 1));
                            i++;
                        } else {
                            b.append('�');
                        }
                    } else if (Character.isLowSurrogate(c)) {
                        b.append('�');
                    } else {
                        b.append(c);
                    }
            }
        }
        b.append('"');
    }
}
