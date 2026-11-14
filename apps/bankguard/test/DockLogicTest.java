import dev.agentroid.bankguard.DockLogic;

import java.util.Arrays;
import java.util.HashSet;
import java.util.List;

/** Plain-JVM tests for DockLogic. Exit code 0 when all pass. */
public final class DockLogicTest {
    static int run, failed;

    static void check(String name, boolean ok) {
        run++;
        if (!ok) { failed++; System.out.println("FAIL " + name); } else System.out.println("ok   " + name);
    }

    public static void main(String[] a) {
        List<String> l = DockLogic.parse("com.a.x, com.b.y,com.a.x ../evil com.c.z;rm com.d.w");
        check("parse: valid, ordered, no repeats", l.equals(Arrays.asList("com.a.x", "com.b.y", "com.d.w")));
        check("parse: null is empty", DockLogic.parse(null).isEmpty());
        check("parse: at most six", DockLogic.parse("a.a,b.b,c.c,d.d,e.e,f.f,g.g").size() == DockLogic.MAX_APPS);
        check("join round-trips", DockLogic.parse(DockLogic.join(l)).equals(l));

        check("toggle off closes the gap", DockLogic.toggle(l, "com.b.y") && l.equals(Arrays.asList("com.a.x", "com.d.w")));
        check("toggle on goes to the end", DockLogic.toggle(l, "com.b.y") && l.equals(Arrays.asList("com.a.x", "com.d.w", "com.b.y")));
        List<String> full = DockLogic.parse("a.a,b.b,c.c,d.d,e.e,f.f");
        check("toggle on a full dock does nothing", !DockLogic.toggle(full, "g.g") && full.size() == 6 && !full.contains("g.g"));
        check("toggle off a full dock still works", DockLogic.toggle(full, "a.a") && full.size() == 5);
        check("toggle rejects a bad name", !DockLogic.toggle(full, "../x") && full.size() == 5);

        List<String> left = DockLogic.installed(Arrays.asList("com.a.x", "com.gone.app", "com.b.y"),
                new HashSet<String>(Arrays.asList("com.b.y", "com.a.x")));
        check("uninstalled apps drop out, order kept", left.equals(Arrays.asList("com.a.x", "com.b.y")));

        check("request codes differ per slot", DockLogic.requestCode(5, 0) != DockLogic.requestCode(5, 1));
        check("request codes differ per widget", DockLogic.requestCode(5, 7) != DockLogic.requestCode(6, 0));

        System.out.println(run + " checks, " + failed + " failed");
        System.exit(failed == 0 ? 0 : 1);
    }
}
