package dev.agentroid.helper.core;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * Hand-rolled runner: runs every public no-arg method named test* on each listed class, prints PASS/FAIL per test,
 * and exits 1 if anything failed (or if zero tests ran).
 */
public final class TestRunner {
    /** Short names are in this package; 2.12 adds the :link and :keeper pure-Java tests by full name. */
    private static final String[] CLASSES = {
        "FramingTest", "DispatcherTest", "TreeSerializerTest", "ImePathTest", "AuthGateTest",
        "ApiGateTest", "SourceChoiceTest", "SmallPartsTest", "ChecklistAuditTest",
        "dev.agentroid.helper.link.net.IdentityTest", "dev.agentroid.helper.link.net.LinkTest",
        "dev.agentroid.helper.link.net.FencingTest", "dev.agentroid.helper.link.net.TimingTest",
        "dev.agentroid.helper.link.net.AddressTest", "dev.agentroid.helper.link.net.LinkProtoTest",
        "dev.agentroid.helper.link.net.PairUriTest",
        "dev.agentroid.helper.keeper.logic.KeeperTest",
    };

    public static void main(String[] args) throws Exception {
        int pass = 0;
        List<String> failed = new ArrayList<String>();
        // optional filter: run only classes whose name contains one of the arguments (for the mutant runs)
        for (String cn0 : CLASSES) {
            if (args.length > 0) {
                boolean hit = false;
                for (String a : args) hit |= cn0.contains(a);
                if (!hit) continue;
            }
            String cn = cn0.contains(".") ? cn0.substring(cn0.lastIndexOf('.') + 1) : cn0;
            Class<?> c = Class.forName(cn0.contains(".") ? cn0 : "dev.agentroid.helper.core." + cn0);
            List<Method> ms = new ArrayList<Method>(Arrays.asList(c.getDeclaredMethods()));
            Collections.sort(ms, new java.util.Comparator<Method>() {
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
                    Throwable t = e.getCause();
                    System.out.println("FAIL " + name + "  <- " + t);
                    failed.add(name);
                } catch (Throwable t) {
                    System.out.println("FAIL " + name + "  <- runner: " + t);
                    failed.add(name);
                }
            }
        }
        System.out.println();
        System.out.println("tests passed: " + pass + ", failed: " + failed.size());
        for (String f : failed) System.out.println("  failed: " + f);
        if (!failed.isEmpty() || pass == 0) System.exit(1);
    }
}
