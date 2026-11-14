package dev.agentroid.bankguard;

import android.content.Context;
import android.content.SharedPreferences;
import android.os.SystemClock;
import android.provider.Settings;

import java.text.SimpleDateFormat;
import java.util.ArrayDeque;
import java.util.Date;
import java.util.Locale;
import java.util.Set;

/**
 * The one guard per process, shared by GuardService (the poll), OpenSafely (the shortcut) and HoldProvider (the
 * keeper's question). It owns the persisted {@link GuardLogic.State}, the bank list and the only two writes this app
 * ever makes: Settings.Global adb_enabled and adb_wifi_enabled.
 *
 * <p>Write-ahead: a hold is saved to disk (synchronously) BEFORE debugging is switched off, so a process killed in
 * between still knows what to put back. Preferences live in device-protected storage, so a boot can put debugging
 * back before the first unlock. Times are elapsedRealtime, tagged with the boot count they belong to.
 */
final class GuardCore {
    static final String PREFS = "bankguard";
    static final String ADB_USB = "adb_enabled";       // Settings.Global.ADB_ENABLED
    static final String ADB_WIFI = "adb_wifi_enabled"; // not a public constant; the same key the keeper writes
    static final String KEY_BANKS = "banks";
    static final String KEY_PAUSE_UNTIL = "pause_until_wall"; // wall clock: a pause is the owner's, and may span a boot
    /** Home Wi-Fi mode: the home network's name, and its access points (GuardLogic.parseBssids). Empty: mode off. */
    static final String KEY_HOME_SSID = "home_ssid", KEY_HOME_BSSIDS = "home_bssids";
    /** Singapore banking and Singpass apps (DBS digibank, PayLah!, POSB, OCBC, UOB TMRW, Singpass). The owner's list
     *  replaces it (MainActivity, ConfigReceiver); setup-bankguard.sh prints the bank apps it finds on the phone. */
    static final String DEFAULT_BANKS = "com.dbs.sg.dbsmbanking,com.dbs.dbspaylah,com.dbs.sg.posbmbanking,"
            + "com.ocbc.mobile,com.uob.mighty.app,sg.ndi.sp";

    private static GuardCore one;

    static synchronized GuardCore get(Context c) {
        if (one == null) one = new GuardCore(c.getApplicationContext());
        return one;
    }

    private final Context ctx;
    private final SharedPreferences p;
    private final GuardLogic.State st;
    private final int boot;              // Settings.Global.BOOT_COUNT now, or -1 when unreadable
    private boolean holdFromEarlierBoot; // the saved hold was made before this boot
    private long refreshedAt;            // elapsedRealtime of the last tick that saw the hold
    private long lastSaved;
    private boolean onTrusted; // the phone is on the home network now (GuardService's network callback)
    // measurements, shown by `adb shell dumpsys activity service dev.agentroid.bankguard/.GuardService`
    long holds, releases, reasserts, homeWrites, writeFailures, lastHoldLatencyMs = -1, maxHoldLatencyMs = -1;
    private final ArrayDeque<String> events = new ArrayDeque<String>();

    private GuardCore(Context app) {
        ctx = app;
        Context dp = app.createDeviceProtectedStorageContext();
        p = dp.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
        boot = bootCount();
        st = new GuardLogic.State();
        st.held = p.getBoolean("held", false);
        st.heldPkg = p.getString("held_pkg", null);
        st.restoreUsb = p.getBoolean("restore_usb", false);
        st.restoreWifi = p.getBoolean("restore_wifi", false);
        st.heldAt = p.getLong("held_at", 0);
        st.lastBankAt = p.getLong("last_bank_at", 0);
        st.network = p.getLong("network", 0);
        st.screenOffAt = p.getLong("screen_off_at", 0);
        st.launchUntil = p.getLong("launch_until", 0);
        st.wifiLeftOff = p.getBoolean("wifi_left_off", false);
        refreshedAt = p.getLong("refreshed_at", 0);
        int savedBoot = p.getInt("boot", -2);
        // Unknown boot count on either side: assume another boot (the safe way round: release, and never Wi-Fi).
        holdFromEarlierBoot = boot < 0 || savedBoot != boot;
        if (holdFromEarlierBoot) { st.forgetBootTimes(); refreshedAt = 0; }
    }

    private int bootCount() {
        try { return Settings.Global.getInt(ctx.getContentResolver(), Settings.Global.BOOT_COUNT, -1); }
        catch (Throwable t) { return -1; }
    }

    private static long now() { return SystemClock.elapsedRealtime(); }

    SharedPreferences prefs() { return p; }

    synchronized Set<String> banks() { return GuardLogic.parseList(p.getString(KEY_BANKS, DEFAULT_BANKS)); }

    synchronized void setBanks(Set<String> banks) {
        StringBuilder b = new StringBuilder();
        for (String s : banks) { if (b.length() > 0) b.append(','); b.append(s); }
        p.edit().putString(KEY_BANKS, b.toString()).apply();
        log("banks set: " + banks.size());
    }

    synchronized boolean paused() { return System.currentTimeMillis() < p.getLong(KEY_PAUSE_UNTIL, 0); }

    synchronized void pauseFor(long ms) {
        p.edit().putLong(KEY_PAUSE_UNTIL, ms <= 0 ? 0 : System.currentTimeMillis() + ms).apply();
        log(ms <= 0 ? "resumed" : "paused " + (ms / 60_000L) + " min");
    }

    synchronized GuardLogic.State state() { return st.copy(); }

    /** For the HoldProvider: should the keeper leave Wireless debugging off right now? */
    synchronized boolean keeperShouldWait() {
        if (homeMode()) return GuardLogic.keeperShouldWaitHome(st, onTrusted);
        return GuardLogic.keeperShouldWait(st, refreshedAt, now(), !holdFromEarlierBoot);
    }

    // ------------------------------------------------------------------------------------------- home Wi-Fi mode

    synchronized String homeSsid() { return p.getString(KEY_HOME_SSID, ""); }

    synchronized Set<String> homeBssids() { return GuardLogic.parseBssids(p.getString(KEY_HOME_BSSIDS, "")); }

    /** On only with both a name and at least one access point. */
    synchronized boolean homeMode() { return !homeSsid().isEmpty() && !homeBssids().isEmpty(); }

    synchronized boolean trusted(String ssid, String bssid) {
        return GuardLogic.trusted(ssid, bssid, homeSsid(), homeBssids());
    }

    /** Set (or, with an empty name, clear) the home network. Nothing is written to the settings here: the service's
     *  callback re-checks the current network when the preference changes. */
    synchronized void setHome(String ssid, Set<String> bssids) {
        StringBuilder b = new StringBuilder();
        for (String s : bssids) { if (b.length() > 0) b.append(','); b.append(s); }
        p.edit().putString(KEY_HOME_SSID, ssid == null ? "" : ssid).putString(KEY_HOME_BSSIDS, b.toString()).commit();
        log(ssid == null || ssid.isEmpty() ? "home wifi mode off" : "home wifi set: " + bssids.size() + " access point(s)");
    }

    /** The network callback: the phone joined or left the home network (or the home network was changed). */
    synchronized GuardLogic.Action setOnTrusted(boolean t) {
        boolean changed = t != onTrusted;
        onTrusted = t;
        if (!homeMode()) return GuardLogic.Action.NONE;
        if (changed) log(t ? "home wifi: joined" : "home wifi: left");
        GuardLogic.Seen s = new GuardLogic.Seen(null, null, now(), read(ADB_USB), read(ADB_WIFI), false, 0, true, false);
        GuardLogic.Action a = GuardLogic.withHome(GuardLogic.Action.NONE, st, s, t);
        commitThenApply(a, 0);
        return a;
    }

    /** Home mode over an action GuardLogic has just decided for the state in s. */
    private GuardLogic.Action home(GuardLogic.Action a, GuardLogic.Seen s) {
        return homeMode() ? GuardLogic.withHome(a, st, s, onTrusted) : a;
    }

    // ------------------------------------------------------------------------------------------------ the poll

    /** One poll. fgSinceWall is the wall-clock time the bank came in front (for the latency figure), or 0. */
    synchronized GuardLogic.Action tick(String fg, String bank, boolean fgIsHome, boolean onWifi, long network,
                                        boolean screenOn, long fgSinceWall) {
        long now = now();
        GuardLogic.Seen s = new GuardLogic.Seen(fg, bank, now, read(ADB_USB), read(ADB_WIFI), onWifi, network, screenOn, fgIsHome);
        GuardLogic.Action a = home(GuardLogic.tick(st, s), s);
        if (st.held) refreshedAt = now;
        commitThenApply(a, fgSinceWall);
        if (a.event == null && st.held && now - lastSaved > 5_000) save(false);
        return a;
    }

    /** The shortcut: debugging off first, then the bank opens; held for LAUNCH_MS while the bank starts. Opening a
     *  bank from its "(safe)" shortcut is the owner asking for the guard, so it also ends a pause. */
    synchronized GuardLogic.Action holdNow(String pkg, boolean onWifi, long network) {
        if (paused()) pauseFor(0);
        long now = now();
        GuardLogic.Seen s = new GuardLogic.Seen(pkg, pkg, now, read(ADB_USB), read(ADB_WIFI), onWifi, network, true, false);
        GuardLogic.Action a = home(GuardLogic.holdForLaunch(st, s), s);
        refreshedAt = now;
        commitThenApply(a, 0);
        save(true);
        return a;
    }

    /** The owner's button or a pause. */
    synchronized GuardLogic.Action releaseNow(String event, boolean onWifi, long network) {
        GuardLogic.Seen s = new GuardLogic.Seen(null, null, now(), read(ADB_USB), read(ADB_WIFI), onWifi, network, true, false);
        GuardLogic.Action a = home(GuardLogic.release(st, s, event), s);
        commitThenApply(a, 0);
        return a;
    }

    /** At boot (LOCKED_BOOT_COMPLETED, BOOT_COMPLETED): release a hold from an earlier boot, never Wireless debugging
     *  (the network is not known yet, and a handle from the last boot means nothing). A hold made in this boot stays. */
    synchronized GuardLogic.Action bootRelease() {
        GuardLogic.Action a = GuardLogic.Action.NONE;
        if (GuardLogic.bootShouldRelease(st, holdFromEarlierBoot)) {
            GuardLogic.Seen s = new GuardLogic.Seen(null, null, now(), read(ADB_USB), read(ADB_WIFI), false, 0, true, false);
            a = home(GuardLogic.release(st, s, "release_boot"), s);
            commitThenApply(a, 0);
        } else if (st.held) {
            log("boot: a hold from this boot stays");
        }
        if (holdFromEarlierBoot) { holdFromEarlierBoot = false; save(true); }
        return a;
    }

    /** The owner tapped "turn Wireless debugging on" after it was left off. Only then, and only with no hold. */
    synchronized boolean wifiOnByOwner() {
        if (!st.wifiLeftOff || st.held) { log("wifi-on tap ignored (nothing left off, or a bank is open)"); return false; }
        write(ADB_WIFI, 1);
        st.wifiLeftOff = false;
        save(true);
        log("wifi on (owner's tap)");
        return true;
    }

    /** The Wi-Fi network it was left off on went away: the next network is the keeper's call again. */
    synchronized void wifiGone() {
        if (st.wifiLeftOff && !st.held) { st.wifiLeftOff = false; save(true); log("wifi gone: left-off cleared"); }
    }

    /** Write-ahead: a hold reaches the disk before any setting goes to 0; everything else after its writes. */
    private void commitThenApply(GuardLogic.Action a, long fgSinceWall) {
        if (a.event == null) return;
        if (a.switchesOff()) save(true);
        if (a.usb != null) write(ADB_USB, a.usb);
        if (a.wifi != null) write(ADB_WIFI, a.wifi);
        save(true);
        String extra = "";
        if (a.event.equals("hold")) {
            holds++;
            if (fgSinceWall > 0 && a.writes()) {
                lastHoldLatencyMs = System.currentTimeMillis() - fgSinceWall;
                maxHoldLatencyMs = Math.max(maxHoldLatencyMs, lastHoldLatencyMs);
                extra = " latency=" + lastHoldLatencyMs + "ms";
            }
        } else if (a.event.equals("reassert")) {
            reasserts++;
        } else if (a.event.startsWith("release")) {
            releases++;
        } else if (a.event.startsWith("home_")) {
            homeWrites++;
        }
        log(a.event + " pkg=" + st.heldPkg + " usb=" + a.usb + " wifi=" + a.wifi + extra);
    }

    private boolean read(String key) {
        try { return Settings.Global.getInt(ctx.getContentResolver(), key, 0) != 0; }
        catch (Throwable t) { return false; }
    }

    private void write(String key, int v) {
        try {
            if (!Settings.Global.putInt(ctx.getContentResolver(), key, v)) { writeFailures++; log("write " + key + "=" + v + " returned false"); }
        } catch (SecurityException e) {
            writeFailures++;
            log("write " + key + "=" + v + " refused: grant WRITE_SECURE_SETTINGS over adb");
        } catch (Throwable t) {
            writeFailures++;
            log("write " + key + "=" + v + " failed: " + t);
        }
    }

    /** sync: commit() (on disk when it returns), for every change of the hold; otherwise apply(), for refreshes. */
    private void save(boolean sync) {
        lastSaved = now();
        SharedPreferences.Editor e = p.edit().putBoolean("held", st.held).putString("held_pkg", st.heldPkg)
                .putBoolean("restore_usb", st.restoreUsb).putBoolean("restore_wifi", st.restoreWifi)
                .putLong("held_at", st.heldAt).putLong("last_bank_at", st.lastBankAt).putLong("network", st.network)
                .putLong("screen_off_at", st.screenOffAt).putLong("launch_until", st.launchUntil)
                .putBoolean("wifi_left_off", st.wifiLeftOff).putLong("refreshed_at", refreshedAt)
                .putInt("boot", holdFromEarlierBoot ? p.getInt("boot", -2) : boot);
        if (sync) { if (!e.commit()) log("save: commit failed"); } else e.apply();
    }

    synchronized void log(String line) {
        String t = new SimpleDateFormat("MM-dd HH:mm:ss.SSS", Locale.ROOT).format(new Date());
        events.addLast(t + " " + line);
        while (events.size() > 60) events.removeFirst();
        android.util.Log.i("AgentroidBankGuard", line);
    }

    synchronized String dump() {
        StringBuilder b = new StringBuilder();
        b.append("held=").append(st.held).append(" pkg=").append(st.heldPkg).append(" restore_usb=").append(st.restoreUsb)
                .append(" restore_wifi=").append(st.restoreWifi).append(" wifi_left_off=").append(st.wifiLeftOff)
                .append(" launch_until=").append(st.launchUntil).append('\n');
        b.append("adb_enabled=").append(read(ADB_USB) ? 1 : 0).append(" adb_wifi_enabled=").append(read(ADB_WIFI) ? 1 : 0)
                .append(" paused=").append(paused()).append(" keeper_should_wait=").append(keeperShouldWait())
                .append(" boot=").append(boot).append(" hold_from_earlier_boot=").append(holdFromEarlierBoot).append('\n');
        b.append("home_mode=").append(homeMode()).append(" home_access_points=").append(homeBssids().size())
                .append(" on_home_wifi=").append(onTrusted).append('\n');
        b.append("holds=").append(holds).append(" releases=").append(releases).append(" reasserts=").append(reasserts)
                .append(" home_writes=").append(homeWrites)
                .append(" write_failures=").append(writeFailures).append(" last_hold_latency_ms=").append(lastHoldLatencyMs)
                .append(" max_hold_latency_ms=").append(maxHoldLatencyMs).append('\n');
        b.append("banks=").append(banks()).append('\n');
        for (String e : events) b.append("  ").append(e).append('\n');
        return b.toString();
    }
}
