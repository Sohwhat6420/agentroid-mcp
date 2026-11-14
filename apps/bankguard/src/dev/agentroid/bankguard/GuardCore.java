package dev.agentroid.bankguard;

import android.content.Context;
import android.content.SharedPreferences;
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
 * <p>Preferences live in device-protected storage, so a boot can put debugging back before the first unlock.
 */
final class GuardCore {
    static final String PREFS = "bankguard";
    static final String ADB_USB = "adb_enabled";       // Settings.Global.ADB_ENABLED
    static final String ADB_WIFI = "adb_wifi_enabled"; // not a public constant; the same key the keeper writes
    static final String KEY_BANKS = "banks";
    static final String KEY_PAUSE_UNTIL = "pause_until";
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
    private long refreshedAt;
    private long lastSaved;
    // measurements, shown by `adb shell dumpsys activity service dev.agentroid.bankguard/.GuardService`
    long holds, releases, reasserts, writeFailures, lastHoldLatencyMs = -1, maxHoldLatencyMs = -1, lastReleaseAt;
    private final ArrayDeque<String> events = new ArrayDeque<String>();

    private GuardCore(Context app) {
        ctx = app;
        Context dp = app.createDeviceProtectedStorageContext();
        p = dp.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
        st = new GuardLogic.State();
        st.held = p.getBoolean("held", false);
        st.heldPkg = p.getString("held_pkg", null);
        st.restoreUsb = p.getBoolean("restore_usb", false);
        st.restoreWifi = p.getBoolean("restore_wifi", false);
        st.heldAt = p.getLong("held_at", 0);
        st.lastBankAt = p.getLong("last_bank_at", 0);
        st.network = p.getLong("network", 0);
        st.wifiLeftOff = p.getBoolean("wifi_left_off", false);
        refreshedAt = p.getLong("refreshed_at", 0);
    }

    SharedPreferences prefs() { return p; }

    synchronized Set<String> banks() { return GuardLogic.parseList(p.getString(KEY_BANKS, DEFAULT_BANKS)); }

    synchronized void setBanks(Set<String> banks) {
        StringBuilder b = new StringBuilder();
        for (String s : banks) { if (b.length() > 0) b.append(','); b.append(s); }
        p.edit().putString(KEY_BANKS, b.toString()).apply();
        log("banks set: " + banks.size());
    }

    synchronized boolean paused(long now) { return now < p.getLong(KEY_PAUSE_UNTIL, 0); }

    synchronized void pauseFor(long ms) {
        p.edit().putLong(KEY_PAUSE_UNTIL, ms <= 0 ? 0 : System.currentTimeMillis() + ms).apply();
        log(ms <= 0 ? "resumed" : "paused " + (ms / 60_000L) + " min");
    }

    synchronized GuardLogic.State state() { return st.copy(); }

    /** For the HoldProvider: true only while a live guard holds (a dead guard's hold goes stale). */
    synchronized boolean holdLive() { return GuardLogic.holdIsLive(st.held, refreshedAt, System.currentTimeMillis()); }

    // ------------------------------------------------------------------------------------------------ the poll

    /** One poll. fgSince is when the foreground app came in front (for the latency figure), or 0. */
    synchronized GuardLogic.Action tick(String fg, long fgSince, boolean onWifi, long network, boolean screenOn) {
        long now = System.currentTimeMillis();
        GuardLogic.Seen s = new GuardLogic.Seen(fg, now, read(ADB_USB), read(ADB_WIFI), onWifi, network, screenOn);
        GuardLogic.Action a = GuardLogic.tick(st, s, banks());
        apply(a, now, fgSince);
        if (st.held) refreshedAt = now;
        if (a.event != null || (st.held && now - lastSaved > 5_000)) save(now);
        return a;
    }

    /** The shortcut: debugging off first, then the bank opens (no race with the bank's start-up check). */
    synchronized GuardLogic.Action holdNow(String pkg, boolean onWifi, long network) {
        long now = System.currentTimeMillis();
        GuardLogic.Seen s = new GuardLogic.Seen(pkg, now, read(ADB_USB), read(ADB_WIFI), onWifi, network, true);
        GuardLogic.Action a = GuardLogic.tick(st, s, java.util.Collections.singleton(pkg));
        apply(a, now, 0);
        refreshedAt = now;
        save(now);
        return a;
    }

    /** The owner's button, a boot, or the guard being stopped. */
    synchronized GuardLogic.Action releaseNow(String event, boolean onWifi, long network) {
        long now = System.currentTimeMillis();
        GuardLogic.Seen s = new GuardLogic.Seen(null, now, read(ADB_USB), read(ADB_WIFI), onWifi, network, true);
        GuardLogic.Action a = GuardLogic.release(st, s, event);
        apply(a, now, 0);
        save(now);
        return a;
    }

    /** The owner tapped "turn Wireless debugging on" after it was left off: that tap is the consent. */
    synchronized void wifiOnByOwner() {
        write(ADB_WIFI, 1);
        st.wifiLeftOff = false;
        save(System.currentTimeMillis());
        log("wifi on (owner's tap)");
    }

    private void apply(GuardLogic.Action a, long now, long fgSince) {
        if (a.event == null) return;
        if (a.usb != null) write(ADB_USB, a.usb);
        if (a.wifi != null) write(ADB_WIFI, a.wifi);
        long done = System.currentTimeMillis();
        String extra = "";
        if (a.event.equals("hold")) {
            holds++;
            if (fgSince > 0 && a.writes()) {
                lastHoldLatencyMs = done - fgSince;
                maxHoldLatencyMs = Math.max(maxHoldLatencyMs, lastHoldLatencyMs);
                extra = " latency=" + lastHoldLatencyMs + "ms";
            }
        } else if (a.event.equals("reassert")) {
            reasserts++;
        } else if (a.event.startsWith("release")) {
            releases++;
            lastReleaseAt = done;
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

    private void save(long now) {
        lastSaved = now;
        p.edit().putBoolean("held", st.held).putString("held_pkg", st.heldPkg)
                .putBoolean("restore_usb", st.restoreUsb).putBoolean("restore_wifi", st.restoreWifi)
                .putLong("held_at", st.heldAt).putLong("last_bank_at", st.lastBankAt).putLong("network", st.network)
                .putBoolean("wifi_left_off", st.wifiLeftOff).putLong("refreshed_at", refreshedAt).apply();
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
                .append(" restore_wifi=").append(st.restoreWifi).append(" wifi_left_off=").append(st.wifiLeftOff).append('\n');
        b.append("adb_enabled=").append(read(ADB_USB) ? 1 : 0).append(" adb_wifi_enabled=").append(read(ADB_WIFI) ? 1 : 0)
                .append(" paused=").append(paused(System.currentTimeMillis())).append(" hold_live=").append(holdLive()).append('\n');
        b.append("holds=").append(holds).append(" releases=").append(releases).append(" reasserts=").append(reasserts)
                .append(" write_failures=").append(writeFailures).append(" last_hold_latency_ms=").append(lastHoldLatencyMs)
                .append(" max_hold_latency_ms=").append(maxHoldLatencyMs).append('\n');
        b.append("banks=").append(banks()).append('\n');
        for (String e : events) b.append("  ").append(e).append('\n');
        return b.toString();
    }
}
