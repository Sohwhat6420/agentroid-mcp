package dev.agentroid.bankguard;

import java.util.ArrayList;
import java.util.List;

/** The app dock's list rules, with no Android imports (tested by test/DockLogicTest.java). */
public final class DockLogic {
    public static final int MAX_APPS = 6;

    private DockLogic() { }

    /** "a.b,c.d" to an ordered list: valid package names only, no repeats, at most MAX_APPS. */
    public static List<String> parse(String csv) {
        List<String> out = new ArrayList<String>();
        if (csv == null) return out;
        for (String p : csv.split("[,\\s]+")) {
            if (out.size() >= MAX_APPS) break;
            if (GuardLogic.validPackage(p) && !out.contains(p)) out.add(p);
        }
        return out;
    }

    public static String join(List<String> apps) {
        StringBuilder b = new StringBuilder();
        for (String s : apps) { if (b.length() > 0) b.append(','); b.append(s); }
        return b.toString();
    }

    /** Tap an app in the picker: in the list, it comes out (the rest close up); not in it, it goes on the end,
     *  unless the dock is full. Returns false when it was full and nothing changed. */
    public static boolean toggle(List<String> apps, String pkg) {
        if (apps.remove(pkg)) return true;
        if (apps.size() >= MAX_APPS || !GuardLogic.validPackage(pkg)) return false;
        apps.add(pkg);
        return true;
    }

    /** The apps still installed, in the owner's order (an uninstalled app drops out of the dock). */
    public static List<String> installed(List<String> apps, java.util.Set<String> installedPkgs) {
        List<String> out = new ArrayList<String>();
        for (String p : apps) if (installedPkgs.contains(p)) out.add(p);
        return out;
    }

    /** A PendingIntent request code unique per widget and slot. */
    public static int requestCode(int widgetId, int slot) { return 1_000 + widgetId * 8 + slot; }
}
