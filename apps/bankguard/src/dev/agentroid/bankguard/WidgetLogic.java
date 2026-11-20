package dev.agentroid.bankguard;

import java.util.Locale;

/**
 * The pure parts of the year, day, battery and earbuds widgets, so they are tested on a plain JVM
 * (test/WidgetLogicTest.java). Each copies what the owner's NewThing Widgets Pro original (v2.7) shows, read from that
 * app's resources and code and from the originals drawn side by side on the test phone (20 Nov 2026).
 */
public final class WidgetLogic {
    private WidgetLogic() { }

    // ---- Year (YearsDaysCountR): one dot a day, 37 to a row; past white, today red, the rest grey. ----

    /** The original's GridView has 37 columns. */
    public static final int YEAR_COLUMNS = 37;
    public static final int PAST = 0, TODAY = 1, FUTURE = 2;

    public static boolean leap(int year) { return (year % 4 == 0 && year % 100 != 0) || year % 400 == 0; }

    public static int daysIn(int year) { return leap(year) ? 366 : 365; }

    /** Rows for a year (10 for both 365 and 366 days). */
    public static int yearRows(int days) { return (days + YEAR_COLUMNS - 1) / YEAR_COLUMNS; }

    /** Dot i (0-based) on day dayOfYear (1-based). */
    public static int dot(int i, int dayOfYear) {
        int today = dayOfYear - 1;
        return i < today ? PAST : i == today ? TODAY : FUTURE;
    }

    /** "41D" on 20 Nov 2026: the days after today. */
    public static int remaining(int dayOfYear, int days) { return Math.max(0, days - dayOfYear); }

    // ---- Battery (BatteryWidgetEmojiR): four faces and five phrases. ----

    public static final int FACE_LOW = 0, FACE_HALF = 1, FACE_MOST = 2, FACE_FULL = 3;

    /** The original's bands: up to 25, up to 50, up to 75, above. */
    public static int face(int level) {
        if (level <= 25) return FACE_LOW;
        if (level <= 50) return FACE_HALF;
        if (level <= 75) return FACE_MOST;
        return FACE_FULL;
    }

    /** The original's phrases (its strings, upper-cased as it draws them). Plugged in beats the level. */
    public static String phrase(int level, boolean plugged) {
        if (plugged) return "MMM… TASTY VOLTS!";
        switch (face(level)) {
            case FACE_LOW: return "CHARGE ME — I’M DYING!";
            case FACE_HALF: return "WEAK, BUT STILL MOVING";
            case FACE_MOST: return "STILL GOING STRONG!";
            default: return "FULL POWER, BABY!";
        }
    }

    /** Percent from the BATTERY_CHANGED extras, or -1. */
    public static int percent(int level, int scale) {
        if (level < 0 || scale <= 0) return -1;
        return Math.round(level * 100f / scale);
    }

    // ---- Earbuds (EarBudsBattery2R). ----

    /** The original's name test for a pair of earbuds (EarBudsBattery2R.b), besides the Bluetooth audio class. */
    static final String[] BUDS_WORDS = {
        "earbuds", "headphones", "headset", "buds", "pods", "galaxy buds", "pixel buds", "jabra", "sony", "bose",
        "sennheiser", "audio", "sound", "music", "jbl", "skullcandy", "anker", "soundcore", "oneplus buds",
        "realme buds", "mi earbuds", "redmi earbuds", "nothing ear", "oppo enco", "vivo tws", "boat", "noise", "ptron",
        "boult", "wings", "lifestyle", "gaming headset",
    };

    /** Bluetooth major class AUDIO_VIDEO (BluetoothClass.Device.Major.AUDIO_VIDEO). */
    public static final int MAJOR_AUDIO_VIDEO = 0x0400;

    public static boolean looksLikeBuds(String name, int majorClass) {
        if (majorClass == MAJOR_AUDIO_VIDEO) return true;
        if (name == null) return false;
        String n = name.toLowerCase(Locale.ROOT);
        for (String w : BUDS_WORDS) if (n.contains(w)) return true;
        return false;
    }

    /**
     * The earbuds widget's two lines: {top (small, grey), bottom (large dots)}.
     * Nothing connected: the reason on top and CONNECT below, as the original. Connected: the battery on top and the
     * name below.
     */
    public static String[] budsLines(boolean permission, boolean bluetoothOn, boolean anyPaired, String connected,
                                     int level) {
        if (!permission) return new String[] {"Tap to enable", "CONNECT"};
        if (!bluetoothOn) return new String[] {"Bluetooth Off", "CONNECT"};
        if (connected == null) return new String[] {anyPaired ? "No Buds Found" : "No Earbuds", "CONNECT"};
        String name = connected.trim().isEmpty() ? "EARBUDS" : connected.trim().toUpperCase(Locale.ROOT);
        return new String[] {level >= 0 ? level + "%" : "Connected", name};
    }
}
