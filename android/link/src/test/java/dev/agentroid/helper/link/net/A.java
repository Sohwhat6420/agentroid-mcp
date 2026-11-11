package dev.agentroid.helper.link.net;

/** Assertion helpers for the link tests (no JUnit offline; run by core's TestRunner). */
final class A {
    private A() { }

    static void ok(boolean cond, String msg) { if (!cond) throw new AssertionError(msg); }

    static void eq(Object expected, Object actual, String msg) {
        boolean same = expected == null ? actual == null : expected.equals(actual);
        if (!same) throw new AssertionError(msg + ": expected <" + expected + "> but was <" + actual + ">");
    }

    static void eq(long expected, long actual, String msg) {
        if (expected != actual) throw new AssertionError(msg + ": expected <" + expected + "> but was <" + actual + ">");
    }

    interface Body { void run() throws Exception; }

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
