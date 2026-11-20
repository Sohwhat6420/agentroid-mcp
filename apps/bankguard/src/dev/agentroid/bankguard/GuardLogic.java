package dev.agentroid.bankguard;

import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;

/**
 * The bank guard's decisions, with no Android imports, so they are tested on a plain JVM (test/GuardLogicTest.java).
 *
 * <p>One {@link #tick} per poll (about every 400 ms while the screen is on). While an app on the bank list is visible
 * and the phone is unlocked, it holds: USB debugging (adb_enabled) and Wireless debugging (adb_wifi_enabled) are 0.
 * When the owner leaves the bank it releases after a grace and puts back <b>only what it switched off itself</b>.
 * Wireless debugging is put back only on the same Wi-Fi network, in the same boot, that it was switched off on:
 * elsewhere Android would ask the owner to allow the new network, so the guard leaves it off and says so instead.
 *
 * <p>All times here are the phone's elapsedRealtime (milliseconds since boot): a wall clock that moves back cannot
 * stretch a hold. A hold from an earlier boot is released at boot; its times mean nothing in this boot.
 *
 * <p>The guard never switches debugging ON unless it switched it off; it only ever writes those two keys. The one
 * exception is home Wi-Fi mode (below), which the owner turns on: then Wireless debugging is on only on the home
 * network, and only while no bank is open.
 */
public final class GuardLogic {
    /** Left the bank for the home screen or Recents: debugging comes back after this. */
    public static final long HOME_GRACE_MS = 2_000;
    /** Left the bank for another app (Messages for an SMS code, a browser tab, the camera, the dialer): the bank is
     *  likely to come back and check again when it resumes, so the hold stays this long. */
    public static final long OTHER_APP_GRACE_MS = 60_000;
    /** The "(safe)" shortcut switched debugging off before the bank's first screen: hold this long while it starts. */
    public static final long LAUNCH_MS = 15_000;
    public static final long SCREEN_OFF_RELEASE_MS = 5 * 60_000L; // a bank left open under a locked screen
    public static final long HOLD_STALE_MS = 10 * 60_000L; // a hold nobody refreshed for this long is void (HoldProvider)

    /** Packages that come in front of a bank without the owner leaving it: the shade, the keyboard, biometric,
     *  credential and permission prompts. While one of these is in front the hold stays and the grace does not run. */
    public static final Set<String> PASS_THROUGH = Collections.unmodifiableSet(new LinkedHashSet<String>(java.util.Arrays.asList(
            "com.android.systemui", "com.android.permissioncontroller", "com.google.android.permissioncontroller",
            "com.google.android.gms", "com.samsung.android.biometrics.app.setting", "com.samsung.android.samsungpass",
            "com.samsung.android.authfw", "com.android.intentresolver", "android", "com.android.credentialmanager",
            "com.lbe.security.miui", "com.miui.securitycenter")));

    // UsageEvents.Event types (public constants on Android 10+, copied so this class stays Android-free)
    public static final int EV_RESUMED = 1, EV_PAUSED = 2, EV_STOPPED = 23, EV_DESTROYED = 24;

    private static final Pattern PKG = Pattern.compile("[A-Za-z][A-Za-z0-9_]*(\\.[A-Za-z][A-Za-z0-9_]*)+");
    private static final Pattern CLS = Pattern.compile("[A-Za-z_][A-Za-z0-9_$]*(\\.[A-Za-z_][A-Za-z0-9_$]*)*");

    /** What the phone looks like this tick. */
    public static final class Seen {
        /** The latest activity to resume ("package/Class"), null when unknown. Decides which grace applies. */
        public final String fg;
        /** A bank on the list that is visible now (resumed, or paused under a dialog or in split screen), else null. */
        public final String bank;
        public final long now;              // elapsedRealtime
        public final boolean usbOn, wifiOn, onWifi;
        /** Screen on AND unlocked. A locked screen counts as off: a bank behind the keyguard is not in front. */
        public final boolean screenOn;
        public final boolean fgIsHome;      // fg is the home app (home screen or Recents)
        public final long network;          // the current Wi-Fi network's handle in this boot, 0 when not on Wi-Fi
        public Seen(String fg, String bank, long now, boolean usbOn, boolean wifiOn, boolean onWifi, long network,
                    boolean screenOn, boolean fgIsHome) {
            this.fg = fg; this.bank = bank; this.now = now; this.usbOn = usbOn; this.wifiOn = wifiOn; this.onWifi = onWifi;
            this.network = network; this.screenOn = screenOn; this.fgIsHome = fgIsHome;
        }
    }

    /** The persisted hold. Saved BEFORE any setting is switched off (GuardCore), so a process death can never forget
     *  what to put back. */
    public static final class State {
        public boolean held;
        public String heldPkg;
        public boolean restoreUsb, restoreWifi;
        public long heldAt, lastBankAt, network, screenOffAt, launchUntil;
        /** Wireless debugging was off by us and could not go back (another network, or a reboot): the owner is told,
         *  and the keeper leaves it off too until the owner taps, or this Wi-Fi network goes away. */
        public boolean wifiLeftOff;
        public State copy() {
            State s = new State();
            s.held = held; s.heldPkg = heldPkg; s.restoreUsb = restoreUsb; s.restoreWifi = restoreWifi;
            s.heldAt = heldAt; s.lastBankAt = lastBankAt; s.network = network; s.screenOffAt = screenOffAt;
            s.launchUntil = launchUntil; s.wifiLeftOff = wifiLeftOff;
            return s;
        }
        /** Loaded from an earlier boot: the times and the network handle mean nothing now. */
        public void forgetBootTimes() { heldAt = 0; lastBankAt = 0; network = 0; screenOffAt = 0; launchUntil = 0; }
    }

    /** What to write, and why, for one tick. A null Integer means: leave that key alone. */
    public static final class Action {
        public final Integer usb, wifi;
        public final String event; // hold, reassert, release*, *_wifi_left_off, null
        Action(Integer usb, Integer wifi, String event) { this.usb = usb; this.wifi = wifi; this.event = event; }
        public boolean writes() { return usb != null || wifi != null; }
        public boolean switchesOff() { return (usb != null && usb == 0) || (wifi != null && wifi == 0); }
        public static final Action NONE = new Action(null, null, null);
    }

    /**
     * The activities visible now, from UsageEvents in order: RESUMED adds, STOPPED or DESTROYED removes; PAUSED keeps
     * the activity (it is still visible under a dialog, or in the other half of split screen). Replaying events it
     * has already seen leaves it unchanged, so overlapping queries are safe.
     */
    public static final class Visible {
        private final Map<String, Long> on = new LinkedHashMap<String, Long>();
        public String top; // the latest to resume

        public void event(int type, String pkg, String cls) {
            String key = pkg + "/" + cls;
            if (type == EV_RESUMED) { on.remove(key); on.put(key, 0L); top = key; }
            else if (type == EV_STOPPED || type == EV_DESTROYED) { on.remove(key); if (key.equals(top)) top = null; }
        }

        /** A visible bank's package, the most recently resumed first; null if none. */
        public String bank(Set<String> banks) {
            String found = null;
            for (String k : on.keySet()) if (isBank(banks, k)) found = pkgOf(k);
            return found;
        }

        public void clear() { on.clear(); top = null; }
        public int size() { return on.size(); }
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

    /** True when the app (or that one activity of it) is on the list. */
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
     * One poll. Mutates st and returns what to write. A visible bank on an unlocked screen always holds (and
     * re-asserts if something switched debugging back on). Otherwise a hold is released once the owner has been out
     * of the bank for the grace, or after SCREEN_OFF_RELEASE_MS of screen-off or lock.
     */
    public static Action tick(State st, Seen s) {
        if (s.bank != null && s.screenOn) {
            st.lastBankAt = s.now;
            st.screenOffAt = 0;
            st.launchUntil = 0; // the bank has arrived
            if (!st.held) {
                st.held = true;
                st.heldPkg = s.bank;
                st.heldAt = s.now;
                st.restoreUsb = s.usbOn;
                st.restoreWifi = s.wifiOn;
                st.network = s.onWifi ? s.network : 0;
                st.wifiLeftOff = false;
                if (!s.usbOn && !s.wifiOn) return new Action(null, null, "hold");
                return new Action(s.usbOn ? 0 : null, s.wifiOn ? 0 : null, "hold");
            }
            st.heldPkg = s.bank;
            // Switched back on during the hold (by the owner or another app): off again, and NOT added to what is
            // restored, because the guard did not find it on.
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
        if (st.launchUntil != 0) {
            if (s.now < st.launchUntil) return Action.NONE; // the shortcut's bank is still starting
            st.launchUntil = 0;
        }
        if (s.fg == null || PASS_THROUGH.contains(pkgOf(s.fg))) {
            st.lastBankAt = s.now; // a prompt over the bank counts as the bank: the grace starts when it closes
            return Action.NONE;
        }
        long grace = s.fgIsHome ? HOME_GRACE_MS : OTHER_APP_GRACE_MS;
        if (s.now - st.lastBankAt < grace) return Action.NONE;
        return release(st, s, "release");
    }

    /** The "(safe)" shortcut: hold before the bank's first screen, and keep holding for LAUNCH_MS while it starts. */
    public static Action holdForLaunch(State st, Seen s) {
        Action a = tick(st, s);
        st.launchUntil = s.now + LAUNCH_MS;
        return a;
    }

    /** Let go now (the owner's button, a pause, or a boot). */
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
        st.launchUntil = 0;
        st.wifiLeftOff = leftOff;
        return new Action(usb, wifi, leftOff ? event + "_wifi_left_off" : event);
    }

    /** At boot: release only a hold made in an EARLIER boot. A hold made since this boot (a bank opened before the
     *  late BOOT_COMPLETED arrived) is live and stays. */
    public static boolean bootShouldRelease(State st, boolean holdFromEarlierBoot) {
        return st.held && holdFromEarlierBoot;
    }

    /** What the HoldProvider answers to the keeper ("leave Wireless debugging off"): a live hold that switched
     *  Wireless debugging off, or Wireless debugging left off after a release. Nothing about which app is open. */
    public static boolean keeperShouldWait(State st, long refreshedAt, long now, boolean sameBoot) {
        boolean live = st.held && sameBoot && now - refreshedAt >= 0 && now - refreshedAt < HOLD_STALE_MS;
        return (live && st.restoreWifi) || st.wifiLeftOff;
    }

    // ------------------------------------------------------------------------------------------- home Wi-Fi mode

    /*
     * Home Wi-Fi mode is off until the owner names a home network (ConfigReceiver, over adb). Once it is on, the
     * guard owns Wireless debugging outright:
     *   - on while the phone is on a trusted network (the home SSID, from one of the home access points) and no bank
     *     hold is on;
     *   - off the moment the phone leaves that network, and off while a bank is open at home.
     * USB debugging keeps the bank-hold rule above (off while a bank is open, back as it was).
     * A network whose name or access point cannot be read (no location access, location off) is not trusted: the
     * unknown case fails closed, with Wireless debugging off.
     */

    private static final Pattern MAC = Pattern.compile("[0-9a-f]{2}(:[0-9a-f]{2}){5}");
    private static final Pattern MAC_PREFIX = Pattern.compile("[0-9a-f]{2}(:[0-9a-f]{2}){2,4}:\\*");

    /** The home access points: full BSSIDs ("aa:bb:cc:dd:ee:01") or a prefix of 3 to 5 octets ending in ":*" (a mesh
     *  whose nodes and bands are not all known). Lower-cased; anything else is dropped. */
    public static Set<String> parseBssids(String s) {
        Set<String> out = new LinkedHashSet<String>();
        if (s == null) return out;
        for (String part : s.toLowerCase(java.util.Locale.ROOT).split("[,\\s]+")) {
            if (MAC.matcher(part).matches() || MAC_PREFIX.matcher(part).matches()) out.add(part);
        }
        return out;
    }

    /** Android quotes a UTF-8 SSID ("\"home\""); a hidden or unreadable one is "<unknown ssid>". */
    public static String unquote(String ssid) {
        if (ssid == null) return null;
        if (ssid.length() >= 2 && ssid.startsWith("\"") && ssid.endsWith("\"")) return ssid.substring(1, ssid.length() - 1);
        return ssid;
    }

    /** Is this the home network? The SSID must match exactly, and the access point must be one of the home ones. An
     *  empty home SSID means home mode is off: nothing is trusted. An empty access-point list never trusts either,
     *  because a network name alone is easy to copy. */
    public static boolean trusted(String ssid, String bssid, String homeSsid, Set<String> homeBssids) {
        if (homeSsid == null || homeSsid.isEmpty() || homeBssids == null || homeBssids.isEmpty()) return false;
        String name = unquote(ssid);
        if (name == null || !name.equals(homeSsid) || bssid == null) return false;
        String ap = bssid.toLowerCase(java.util.Locale.ROOT);
        if (!MAC.matcher(ap).matches() || ap.equals("02:00:00:00:00:00")) return false; // Android's redacted value
        for (String h : homeBssids) {
            if (h.endsWith("*") ? ap.startsWith(h.substring(0, h.length() - 1)) : ap.equals(h)) return true;
        }
        return false;
    }

    /** What home mode wants for Wireless debugging now: on exactly when trusted and no bank hold is on. */
    public static boolean homeWantsWifi(State st, boolean onTrusted) { return onTrusted && !st.held; }

    /**
     * Home mode over a tick's action (after tick, release or holdForLaunch has updated st). The USB part is kept; the
     * Wireless part is replaced by what home mode wants. "Left off" never applies: home mode puts it back itself when
     * the phone is home again.
     */
    public static Action withHome(Action a, State st, Seen s, boolean onTrusted) {
        st.wifiLeftOff = false;
        boolean want = homeWantsWifi(st, onTrusted);
        Integer wifi = want == s.wifiOn ? null : (want ? 1 : 0);
        String event = a.event;
        if (event != null && event.endsWith("_wifi_left_off")) event = event.substring(0, event.length() - "_wifi_left_off".length());
        if (event == null && wifi != null) event = want ? "home_wifi_on" : (onTrusted ? "home_wifi_off_bank" : "home_wifi_off_away");
        if (event == null && a.usb == null) return Action.NONE;
        return new Action(a.usb, wifi, event);
    }

    /** The keeper's answer in home mode: wait (leave Wireless debugging off) whenever home mode wants it off. */
    public static boolean keeperShouldWaitHome(State st, boolean onTrusted) { return !homeWantsWifi(st, onTrusted); }
}
