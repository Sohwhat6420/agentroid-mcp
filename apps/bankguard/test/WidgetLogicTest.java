import dev.agentroid.bankguard.ClockLogic;
import dev.agentroid.bankguard.WidgetLogic;

import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;

/** Plain-JVM tests for WidgetLogic and the day widget's calendar choice. Exit code 0 when all pass. */
public final class WidgetLogicTest {
    static int run, failed;

    static void check(String name, boolean ok) {
        run++;
        if (!ok) { failed++; System.out.println("FAIL " + name); } else System.out.println("ok   " + name);
    }

    static Set<String> set(String... s) { return new HashSet<String>(Arrays.asList(s)); }

    public static void main(String[] a) {
        // Year: 20 Nov 2026 is day 324 of 365; the original showed 41D and the red dot at row 9, column 28.
        check("2026 has 365 days, 2028 366, 2100 365, 2000 366", WidgetLogic.daysIn(2026) == 365
                && WidgetLogic.daysIn(2028) == 366 && WidgetLogic.daysIn(2100) == 365 && WidgetLogic.daysIn(2000) == 366);
        check("41 days left on 20 Nov 2026", WidgetLogic.remaining(324, 365) == 41);
        check("0 left on 31 Dec", WidgetLogic.remaining(365, 365) == 0 && WidgetLogic.remaining(366, 366) == 0);
        check("ten rows of 37 for both year lengths", WidgetLogic.yearRows(365) == 10 && WidgetLogic.yearRows(366) == 10);
        check("today is dot 323: row 9, column 28 (1-based)", WidgetLogic.dot(323, 324) == WidgetLogic.TODAY
                && 323 / WidgetLogic.YEAR_COLUMNS == 8 && 323 % WidgetLogic.YEAR_COLUMNS == 27);
        check("the day before is past, the day after future", WidgetLogic.dot(322, 324) == WidgetLogic.PAST
                && WidgetLogic.dot(324, 324) == WidgetLogic.FUTURE);
        check("1 Jan: the first dot red, none past", WidgetLogic.dot(0, 1) == WidgetLogic.TODAY
                && WidgetLogic.dot(1, 1) == WidgetLogic.FUTURE);

        // Battery: the original's bands and phrases.
        check("bands at 25/50/75", WidgetLogic.face(25) == WidgetLogic.FACE_LOW && WidgetLogic.face(26) == WidgetLogic.FACE_HALF
                && WidgetLogic.face(50) == WidgetLogic.FACE_HALF && WidgetLogic.face(51) == WidgetLogic.FACE_MOST
                && WidgetLogic.face(75) == WidgetLogic.FACE_MOST && WidgetLogic.face(76) == WidgetLogic.FACE_FULL);
        check("30%: WEAK, BUT STILL MOVING (as on the original)",
                "WEAK, BUT STILL MOVING".equals(WidgetLogic.phrase(30, false)));
        check("plugged in beats the level", WidgetLogic.phrase(10, true).startsWith("MMM"));
        check("low, most, full phrases", WidgetLogic.phrase(5, false).startsWith("CHARGE ME")
                && "STILL GOING STRONG!".equals(WidgetLogic.phrase(60, false))
                && "FULL POWER, BABY!".equals(WidgetLogic.phrase(100, false)));
        check("percent from level and scale", WidgetLogic.percent(31, 100) == 31 && WidgetLogic.percent(155, 255) == 61
                && WidgetLogic.percent(-1, 100) == -1 && WidgetLogic.percent(50, 0) == -1);

        // Earbuds.
        check("an audio-class device counts", WidgetLogic.looksLikeBuds("XYZ-123", WidgetLogic.MAJOR_AUDIO_VIDEO));
        check("Galaxy Buds by name", WidgetLogic.looksLikeBuds("Ethan's Galaxy Buds3 Pro", 0));
        check("a car or a watch does not", !WidgetLogic.looksLikeBuds("Galaxy Watch7", 0x0700)
                && !WidgetLogic.looksLikeBuds("MY CAR", 0x0200) && !WidgetLogic.looksLikeBuds(null, 0));
        check("no permission: Tap to enable / CONNECT",
                Arrays.equals(WidgetLogic.budsLines(false, true, true, "x", 50), new String[] {"Tap to enable", "CONNECT"}));
        check("Bluetooth off: Bluetooth Off / CONNECT (as on the original)",
                Arrays.equals(WidgetLogic.budsLines(true, false, true, null, -1), new String[] {"Bluetooth Off", "CONNECT"}));
        check("none connected", "No Buds Found".equals(WidgetLogic.budsLines(true, true, true, null, -1)[0])
                && "No Earbuds".equals(WidgetLogic.budsLines(true, true, false, null, -1)[0]));
        check("connected: 80% over the name",
                Arrays.equals(WidgetLogic.budsLines(true, true, true, "Galaxy Buds3", 80), new String[] {"80%", "GALAXY BUDS3"}));
        check("connected, battery unknown", "Connected".equals(WidgetLogic.budsLines(true, true, true, "Buds", -1)[0]));

        // The day widget's calendar: NewThing's list first (Google before Samsung), then the default, then null.
        String g = "com.google.android.calendar", s = "com.samsung.android.calendar";
        check("One UI with Google Calendar too: Google, as the original",
                g.equals(ClockLogic.chooseCalendar(s, set(g, s))));
        check("only Samsung Calendar", s.equals(ClockLogic.chooseCalendar(s, set(s))));
        check("a default not on the list is used when nothing listed is installed",
                "com.example.cal".equals(ClockLogic.chooseCalendar("com.example.cal", set("com.example.cal"))));
        check("the chooser is not an app", ClockLogic.chooseCalendar("android", set("android")) == null);
        check("no calendar: null (settings fallback)",
                ClockLogic.chooseCalendar(null, Collections.<String>emptySet()) == null);

        System.out.println(run + " checks, " + failed + " failed");
        System.exit(failed == 0 ? 0 : 1);
    }
}
