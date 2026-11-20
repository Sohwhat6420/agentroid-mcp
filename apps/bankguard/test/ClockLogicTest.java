import dev.agentroid.bankguard.ClockLogic;

import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;

/** Plain-JVM tests for ClockLogic. Exit code 0 when all pass. */
public final class ClockLogicTest {
    static int run, failed;

    static void check(String name, boolean ok) {
        run++;
        if (!ok) { failed++; System.out.println("FAIL " + name); } else System.out.println("ok   " + name);
    }

    static Set<String> set(String... s) { return new HashSet<String>(Arrays.asList(s)); }

    public static void main(String[] a) {
        String sec = "com.sec.android.app.clockpackage", goog = "com.google.android.deskclock";
        check("One UI: Samsung Clock (the alarm handler)", sec.equals(ClockLogic.choose(sec, set(sec))));
        check("HyperOS: the phone's own clock", "com.android.deskclock".equals(
                ClockLogic.choose("com.android.deskclock", set("com.android.deskclock"))));
        check("an alarm handler not in NewThing's list still wins", "com.example.clock".equals(
                ClockLogic.choose("com.example.clock", set("com.example.clock", sec))));
        check("two clock apps (system would ask): NewThing's order, Google first",
                goog.equals(ClockLogic.choose("android", set(sec, goog))));
        check("no alarm handler: the first of the list that is installed", sec.equals(ClockLogic.choose(null, set(sec))));
        check("an alarm handler without a launcher entry is skipped",
                sec.equals(ClockLogic.choose("com.example.headless", set(sec))));
        check("no clock app at all: null (settings fallback)",
                ClockLogic.choose(null, Collections.<String>emptySet()) == null);
        check("the list is NewThing's order", ClockLogic.KNOWN[0].equals(goog) && ClockLogic.KNOWN[1].equals(sec)
                && ClockLogic.KNOWN.length == 8);

        System.out.println(run + " checks, " + failed + " failed");
        System.exit(failed == 0 ? 0 : 1);
    }
}
