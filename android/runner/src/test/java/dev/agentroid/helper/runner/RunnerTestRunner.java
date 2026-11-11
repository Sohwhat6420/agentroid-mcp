package dev.agentroid.helper.runner;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.List;

/**
 * Hand-rolled runner for the :runner JVM tests (the same pattern as core's TestRunner; there is no JUnit offline).
 * Runs every public no-arg test* method, prints PASS/FAIL, exits 1 if anything failed or nothing ran. Arguments
 * filter by class name (used for the mutant runs).
 */
public final class RunnerTestRunner {
    private static final String[] CLASSES = {
        "SegmentFixturesTest", "VerifierTest", "LocalRefusalTest", "RunnerBehaviourTest", "LinkDownTest", "HumanTouchTest", "HumanTouchFlagTest", "OverheadTest", "RunnerMethodsTest", "HumanTouchWiringTest",
    };

    public static void main(String[] args) throws Exception {
        int pass = 0;
        List<String> failed = new ArrayList<String>();
        for (String cn : CLASSES) {
            if (args.length > 0) {
                boolean hit = false;
                for (String a : args) hit |= cn.contains(a);
                if (!hit) continue;
            }
            Class<?> c = Class.forName("dev.agentroid.helper.runner." + cn);
            List<Method> ms = new ArrayList<Method>(Arrays.asList(c.getDeclaredMethods()));
            Collections.sort(ms, new Comparator<Method>() {
                public int compare(Method a, Method b) { return a.getName().compareTo(b.getName()); }
            });
            for (Method m : ms) {
                if (!m.getName().startsWith("test") || m.getParameterTypes().length != 0) continue;
                String name = cn + "." + m.getName();
                try {
                    m.setAccessible(true);
                    m.invoke(c.getDeclaredConstructor().newInstance());
                    System.out.println("PASS " + name);
                    pass++;
                } catch (InvocationTargetException e) {
                    System.out.println("FAIL " + name + "  <- " + e.getCause());
                    failed.add(name);
                } catch (Throwable t) {
                    System.out.println("FAIL " + name + "  <- runner: " + t);
                    failed.add(name);
                }
            }
        }
        System.out.println();
        System.out.println("runner tests passed: " + pass + ", failed: " + failed.size());
        for (String f : failed) System.out.println("  failed: " + f);
        if (!failed.isEmpty() || pass == 0) System.exit(1);
    }
}
