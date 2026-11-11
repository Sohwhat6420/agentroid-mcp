package dev.agentroid.helper.core;

import java.util.Arrays;
import java.util.List;
import java.util.Map;

/** Tiny assertion helpers (no JUnit offline: there is none in s4-apk/libs or the SDK). */
final class T {
    private T() { }

    static void ok(boolean cond, String msg) {
        if (!cond) throw new AssertionError(msg);
    }

    static void eq(Object expected, Object actual, String msg) {
        boolean same = expected == null ? actual == null : expected.equals(actual);
        if (!same) throw new AssertionError(msg + ": expected <" + expected + "> but was <" + actual + ">");
    }

    static void bytesEq(byte[] expected, byte[] actual, String msg) {
        if (!Arrays.equals(expected, actual)) {
            throw new AssertionError(msg + ": expected " + hex(expected) + " but was " + hex(actual));
        }
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x & 0xff));
        return s.toString();
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> obj(Object o) {
        if (!(o instanceof Map)) throw new AssertionError("expected an object but was " + o);
        return (Map<String, Object>) o;
    }

    @SuppressWarnings("unchecked")
    static List<Object> arr(Object o) {
        if (!(o instanceof List)) throw new AssertionError("expected an array but was " + o);
        return (List<Object>) o;
    }

    /** Walks a path of keys through nested objects. */
    static Object at(Object root, String... path) {
        Object cur = root;
        for (String k : path) cur = obj(cur).get(k);
        return cur;
    }

    interface Body { void run() throws Exception; }

    /** Asserts the body throws exactly this exception type. */
    static <E extends Throwable> E throwsA(Class<E> type, Body b, String msg) {
        try {
            b.run();
        } catch (Throwable t) {
            if (type.isInstance(t)) return type.cast(t);
            throw new AssertionError(msg + ": threw " + t + " instead of " + type.getSimpleName());
        }
        throw new AssertionError(msg + ": nothing was thrown");
    }
}
