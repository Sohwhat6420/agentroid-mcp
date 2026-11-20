package dev.agentroid.bankguard;

import java.util.Set;

/**
 * Which app a tap on a dot clock opens. Pure, so it is tested on a plain JVM (test/ClockLogicTest.java).
 *
 * The owner's original clock (NewThing Widgets Pro "Time Date Hello", TimeDateLargeR, v2.7) sends the whole widget to
 * the clock app: its ClockIntentHelper tries the clock packages below, in this order, and falls back to the Date and
 * time settings. Ours opens the same app, and the way a launcher icon does (the app's own launch intent), because:
 * - ACTION_SHOW_ALARMS (what the first build used) needs com.android.alarm.permission.SET_ALARM on One UI: Samsung
 *   Clock's AlarmCTSHandleActivity refuses it, so a tap did nothing (S24 log, 20 Nov 2026, four taps, four refusals);
 * - NewThing's own probe (an implicit MAIN for the package) finds no default activity in Samsung Clock, so on One UI
 *   it lands on the settings fallback unless a clock app was picked in its settings.
 * The phone's own alarm handler comes first, so a phone with one clock app opens that one whatever it is called.
 */
public final class ClockLogic {
    private ClockLogic() { }

    /** NewThing's list (ClockIntentHelper.getAutoDetectedClockIntent), in its order. */
    public static final String[] KNOWN = {
        "com.google.android.deskclock", "com.sec.android.app.clockpackage", "com.oneplus.deskclock", "com.miui.clock",
        "com.android.deskclock", "com.htc.android.worldclock", "com.lge.clock", "com.sonyericsson.organizer",
    };

    /** The package Android names when it would ask the user to choose (its ResolverActivity). */
    static final String CHOOSER = "android";

    /**
     * @param alarmApp   the package that opens alarms by default (ACTION_SHOW_ALARMS), or null; CHOOSER when the
     *                   system would ask
     * @param launchable the packages among alarmApp and KNOWN that have a launcher entry
     * @return the clock app to open, or null for the settings fallback
     */
    public static String choose(String alarmApp, Set<String> launchable) {
        if (alarmApp != null && !CHOOSER.equals(alarmApp) && launchable.contains(alarmApp)) return alarmApp;
        for (String p : KNOWN) if (launchable.contains(p)) return p;
        return null;
    }

    /**
     * NewThing's calendar list (CalendarIntentHelper.getAutoDetectedCalendarIntent), in its order. Its day widget
     * (Calander03WidgetR) opens the first of these that is installed, then the phone's default calendar
     * (MAIN + APP_CALENDAR), then Date and time settings.
     */
    public static final String[] CALENDARS = {
        "com.google.android.calendar", "com.samsung.android.calendar", "com.oneplus.calendar", "com.miui.calendar",
        "com.android.calendar", "com.htc.calendar", "com.lge.calendar", "com.sonyericsson.calendar",
    };

    /**
     * The calendar the day widget opens: the first of CALENDARS with a launcher entry, as the original (which tries
     * its list before the default); else the default calendar app if it has one; else null (the settings fallback).
     */
    public static String chooseCalendar(String defaultApp, Set<String> launchable) {
        for (String p : CALENDARS) if (launchable.contains(p)) return p;
        if (defaultApp != null && !CHOOSER.equals(defaultApp) && launchable.contains(defaultApp)) return defaultApp;
        return null;
    }
}
