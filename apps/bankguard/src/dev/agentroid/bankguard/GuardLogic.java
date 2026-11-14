package dev.agentroid.bankguard;

import java.util.Collections;
import java.util.LinkedHashSet;
import java.util.Set;
import java.util.regex.Pattern;

/**
 * The bank guard's decisions, with no Android imports, so they are tested on a plain JVM (test/GuardLogicTest.java).
 *
 * <p>One {@link #tick} per poll (about every 400 ms while the screen is on). While an app on the bank list is in front
 * it holds: USB debugging (adb_enabled) and Wireless debugging (adb_wifi_enabled) are 0. When the owner leaves the
 * bank it releases after a short grace and puts back <b>only what it switched off itself</b>. Wireless debugging is
 * put back only on the same Wi-Fi network it was switched off on: elsewhere Android would ask the owner to allow the
 * new network, so the guard leaves it off and says so instead.
 *
 * <p>The guard never switches debugging ON unless it switched it off; it only ever writes those two keys.
 */
public final class GuardLogic {
    public static final long GRACE_MS = 3_000;            // leave the bank this long before debugging comes back
    public static final long SCREEN_OFF_RELEASE_MS = 5 * 60_000L; // a bank left in front under a locked screen
    public static final long HOLD_STALE_MS = 10 * 60_000L; // a hold nobody refreshed for this long is void (HoldProvider)

    /** Packages that come in front of a bank without the owner leaving it: the shade, the keyboard, biometric and
     *  permission prompts. While one of these is in front the hold stays. */
    public static final Set<String> PASS_THROUGH = Collections.unmodifiableSet(new LinkedHashSet<String>(java.util.Arrays.asList(
            "com.android.systemui", "com.android.permissioncontroller", "com.google.android.permissioncontroller",
            "com.google.android.gms", "com.samsung.android.biometrics.app.setting", "com.samsung.android.samsungpass",
            "com.samsung.android.authfw", "com.android.intentresolver", "android")));

    private static final Pattern PKG = Pattern.compile("[A-Za-z][A-Za-z0-9_]*(\\.[A-Za-z][A-Za-z0-9_]*)+");
    private static final Pattern CLS = Pattern.compile("[A-Za-z_][A-Za-z0-9_$]*(\\.[A-Za-z_][A-Za-z0-9_$]*)*");

    /** What the phone looks like this tick. fg is "package/activity.Class" (or just a package), null when the
     *  foreground app is unknown (no event seen yet). */
    public static final class Seen {
        public final String fg;
        public final long now;
        public final boolean usbOn, wifiOn, onWifi, screenOn;
        public final long network; // a handle for the current Wi-Fi network, 0 when not on Wi-Fi
        public Seen(String fg, long now, boolean usbOn, boolean wifiOn, boolean onWifi, long network, boolean screenOn) {
            this.fg = fg; this.now = now; this.usbOn = usbOn; this.wifiOn = wifiOn; this.onWifi = onWifi;
            this.network = network; this.screenOn = screenOn;
        }
    }

    /** The persisted hold. Every field survives a process death (the service saves it after each change). */
    public static final class State {
        public boolean held;
        public String heldPkg;
        public boolean restoreUsb, restoreWifi;
        public long heldAt, lastBankAt, network, screenOffAt;
        /** Wireless debugging was off by us and could not go back (another network): the owner is told once. */
        public boolean wifiLeftOff;
        public State copy() {
            State s = new State();
            s.held = held; s.heldPkg = heldPkg; s.restoreUsb = restoreUsb; s.restoreWifi = restoreWifi;
            s.heldAt = heldAt; s.lastBankAt = lastBankAt; s.network = network; s.screenOffAt = screenOffAt;
            s.wifiLeftOff = wifiLeftOff;
            return s;
        }
    }

    /** What to write, and why, for one tick. A null Integer means: leave that key alone. */
    public static final class Action {
        public final Integer usb, wifi;
        public final String event; // hold, reassert, release, release_wifi_left_off, release_screen_off, null
        Action(Integer usb, Integer wifi, String event) { this.usb = usb; this.wifi = wifi; this.event = event; }
        public boolean writes() { return usb != null || wifi != null; }
        static final Action NONE = new Action(null, null, null);
    }

    private GuardLogic() { }

    public static boolean validPackage(String p) { return p != null && p.length() <= 200 && PKG.matcher(p).matches(); }

    /** A bank-list entry: a package, or one activity as "package/full.Class" (the test bank uses that). */
    public static boolean validEntry(String e) {
        if (e == null || e.length() > 300) return false;
        int slash = e.indexOf('/');
        if (slash < 0) return validPackage(e);
        return validPackage(e.substring(0, slash)) && CLS.matcher(e.substring(slash + 1)).matches();
    }

    /** The package part of "package/Class". */
    public static String pkgOf(String fg) {
        if (fg == null) return null;
        int slash = fg.indexOf('/');
        return slash < 0 ? fg : fg.substring(0, slash);
    }

    /** True when the foreground app (or that one activity of it) is on the list. */
    public static boolean isBank(Set<String> banks, String fg) {
        return fg != null && (banks.contains(fg) || banks.contains(pkgOf(fg)));
    }

    /** Parse a comma or whitespace separated list; invalid names are dropped. */
    public static Set<String> parseList(String s) {
        Set<String> out = new LinkedHashSet<String>();
        if (s == null) return out;
        for (String part : s.split("[,\\s]+")) if (validEntry(part)) out.add(part);
        return out;
    }

    /**
     * One poll. Mutates st and returns what to write. Order of the cases matters:
     * a bank in front always holds (and re-asserts if something switched debugging back on);
     * otherwise a hold is released once the owner has been out of the bank for GRACE_MS,
     * or after SCREEN_OFF_RELEASE_MS of screen-off.
     */
    public static Action tick(State st, Seen s, Set<String> banks) {
        boolean bankInFront = isBank(banks, s.fg);
        if (bankInFront && s.screenOn) {
            st.lastBankAt = s.now;
            st.screenOffAt = 0;
            if (!st.held) {
                st.held = true;
                st.heldPkg = pkgOf(s.fg);
                st.heldAt = s.now;
                st.restoreUsb = s.usbOn;
                st.restoreWifi = s.wifiOn;
                st.network = s.onWifi ? s.network : 0;
                st.wifiLeftOff = false;
                if (!s.usbOn && !s.wifiOn) return new Action(null, null, "hold");
                return new Action(s.usbOn ? 0 : null, s.wifiOn ? 0 : null, "hold");
            }
            st.heldPkg = pkgOf(s.fg);
            if (s.usbOn || s.wifiOn) return new Action(s.usbOn ? 0 : null, s.wifiOn ? 0 : null, "reassert");
            return Action.NONE;
        }
        if (!st.held) return Action.NONE;
        if (!s.screenOn) {
            if (st.screenOffAt == 0) st.screenOffAt = s.now;
            if (s.now - st.screenOffAt >= SCREEN_OFF_RELEASE_MS) return release(st, s, "release_screen_off");
            return Action.NONE;
        }
        st.screenOffAt = 0;
        if (s.fg == null || PASS_THROUGH.contains(pkgOf(s.fg))) return Action.NONE; // unknown, or a prompt over the bank
        if (s.now - st.lastBankAt < GRACE_MS) return Action.NONE;
        return release(st, s, "release");
    }

    /** Let go now (the owner's "turn debugging back on" button, a boot, or the guard being switched off). */
    public static Action release(State st, Seen s, String event) {
        if (!st.held) return Action.NONE;
        Integer usb = st.restoreUsb && !s.usbOn ? 1 : null;
        Integer wifi = null;
        boolean leftOff = false;
        if (st.restoreWifi && !s.wifiOn) {
            if (s.onWifi && st.network != 0 && s.network == st.network) wifi = 1;
            else leftOff = true;
        }
        st.held = false;
        st.heldPkg = null;
        st.screenOffAt = 0;
        st.wifiLeftOff = leftOff;
        return new Action(usb, wifi, leftOff ? event + "_wifi_left_off" : event);
    }

    /** What the HoldProvider answers: held, and refreshed recently enough that the guard is still alive. */
    public static boolean holdIsLive(boolean held, long refreshedAt, long now) {
        return held && now - refreshedAt >= 0 && now - refreshedAt < HOLD_STALE_MS;
    }
}
