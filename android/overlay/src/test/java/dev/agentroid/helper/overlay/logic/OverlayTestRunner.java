package dev.agentroid.helper.overlay.logic;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

/** Runs every public {@code test*} method of the overlay test classes (no JUnit offline). Exit 0 = all passed. */
public final class OverlayTestRunner {
    static final Class<?>[] CLASSES = {OverlayClassifierTest.class, DotTimerTest.class, HumanTouchLogTest.class, LogicPurityTest.class};

    public static void main(String[] args) throws Exception {
        int pass = 0, fail = 0;
        for (Class<?> c : CLASSES) {
            for (Method m : c.getDeclaredMethods()) {
                if (!m.getName().startsWith("test") || !Modifier.isPublic(m.getModifiers())) continue;
                String name = c.getSimpleName() + "." + m.getName();
                try {
                    Object o = c.getDeclaredConstructor().newInstance();
                    m.invoke(o);
                    System.out.println("PASS " + name);
                    pass++;
                } catch (InvocationTargetException e) {
                    System.out.println("FAIL " + name + ": " + e.getCause());
                    fail++;
                }
            }
        }
        System.out.println("overlay JVM tests: " + pass + " passed, " + fail + " failed");
        System.exit(fail == 0 ? 0 : 1);
    }

    static void ok(boolean c, String m) {
        if (!c) throw new AssertionError(m);
    }

    static void eq(Object e, Object a, String m) {
        if (e == null ? a != null : !e.equals(a)) throw new AssertionError(m + ": expected <" + e + "> but was <" + a + ">");
    }
}
