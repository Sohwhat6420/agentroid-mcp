package dev.agentroid.helper.runner;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;

/** Assertions and paths for the runner tests (no JUnit offline). */
final class A {
    private A() { }

    static void ok(boolean c, String msg) {
        if (!c) throw new AssertionError(msg);
    }

    static void eq(Object expected, Object actual, String msg) {
        boolean same = expected == null ? actual == null : expected.equals(actual);
        if (!same) throw new AssertionError(msg + ": expected <" + expected + "> but was <" + actual + ">");
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

    /** scaffold/schemas/fixtures/segment (system property runner.fixtures). */
    static Path fixtures() {
        return Paths.get(System.getProperty("runner.fixtures", "../schemas/fixtures/segment"));
    }

    /** Where tests may write (system property runner.out). */
    static Path out() throws IOException {
        Path p = Paths.get(System.getProperty("runner.out", "runner/out"));
        Files.createDirectories(p);
        return p;
    }

    static String read(Path p) throws IOException {
        return new String(Files.readAllBytes(p), StandardCharsets.UTF_8);
    }
}
