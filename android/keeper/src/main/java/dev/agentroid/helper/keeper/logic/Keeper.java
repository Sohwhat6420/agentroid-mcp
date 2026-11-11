package dev.agentroid.helper.keeper.logic;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

import dev.agentroid.helper.core.Clock;

/**
 * The keeper (task 2.12, emptied in from 5.3). Needs WRITE_SECURE_SETTINGS, which only `pm grant` over adb gives.
 *
 * <p>Each pass (on boot, on a settings change seen by a ContentObserver, on a network change, and every 15 minutes):
 * <ul>
 *   <li><b>Wireless debugging</b>: re-assert Settings.Global adb_wifi_enabled=1 ONLY when the current Wi-Fi BSSID is on
 *       the hub-set trusted list. Any other BSSID, an empty list, or a BSSID the phone cannot read reports
 *       untrusted_bssid (once per distinct BSSID and reason) and writes nothing.</li>
 *   <li><b>Accessibility</b>: make sure this app's accessibility service is in enabled_accessibility_services (the other
 *       entries are kept, in order) and accessibility_enabled=1. The design words this part without the BSSID
 *       condition ("and re-adds the accessibility service on boot, on change and every 15 min"), so it is NOT gated by
 *       the BSSID: see {@link #A11Y_FOLLOWS_BSSID} and LINK-REPORT.md, assumptions.</li>
 * </ul>
 * Every write is skipped when the value is already right, so a pass triggered by our own write does nothing.
 */
public final class Keeper {
    public static final long TICK_MS = 15L * 60 * 1000;
    public static final String ADB_WIFI = "adb_wifi_enabled";
    public static final String A11Y_LIST = "enabled_accessibility_services";
    public static final String A11Y_ON = "accessibility_enabled";
    /** false = the plan's wording (accessibility re-added on every network); true = the brief's (only on trusted BSSIDs). */
    public static final boolean A11Y_FOLLOWS_BSSID = false;

    public interface Settings {
        String getGlobal(String key);
        void putGlobal(String key, String value);
        String getSecure(String key);
        void putSecure(String key, String value);
    }

    public interface Wifi {
        boolean onWifi();
        /** The raw BSSID as Android reports it; null or 02:00:00:00:00:00 when it cannot be read. */
        String bssid();
    }

    public interface TrustList {
        Set<String> get();
        void set(Set<String> bssids);
    }

    public interface Reporter {
        /** bssid is null when reason is bssid_unreadable. */
        void untrustedBssid(String bssid, String reason);
    }

    /** What one pass saw and did. */
    public static final class Result {
        public final String state;          // ok, untrusted_bssid, no_wifi, no_permission
        public final Boolean bssidTrusted;  // null off Wi-Fi
        public final String reason;         // untrusted_bssid reason, else null
        public final boolean wroteAdbWifi, wroteA11y;
        Result(String state, Boolean bssidTrusted, String reason, boolean wroteAdbWifi, boolean wroteA11y) {
            this.state = state;
            this.bssidTrusted = bssidTrusted;
            this.reason = reason;
            this.wroteAdbWifi = wroteAdbWifi;
            this.wroteA11y = wroteA11y;
        }
    }

    private final Clock clock;
    private final Settings settings;
    private final Wifi wifi;
    private final TrustList trust;
    private final Reporter reporter;
    private final String a11yComponent;
    private Clock.Task tick;
    private boolean running;
    private Result last;
    private String lastReported;
    private Integer adbPort;
    private int passes;
    private final List<String> triggers = new ArrayList<String>();

    /** @param a11yComponent flattened ComponentName of our accessibility service, "pkg/full.Class" */
    public Keeper(Clock clock, Settings settings, Wifi wifi, TrustList trust, Reporter reporter, String a11yComponent) {
        this.clock = clock;
        this.settings = settings;
        this.wifi = wifi;
        this.trust = trust;
        this.reporter = reporter;
        this.a11yComponent = a11yComponent;
    }

    /** Boot pass now, then every 15 minutes until stop(). */
    public synchronized void start() {
        if (running) return;
        running = true;
        pass("boot");
        scheduleTick();
    }

    public synchronized void stop() {
        running = false;
        if (tick != null) tick.cancel();
        tick = null;
    }

    private void scheduleTick() {
        tick = clock.schedule(TICK_MS, new Runnable() { public void run() {
            synchronized (Keeper.this) {
                if (!running) return;
                pass("tick");
                scheduleTick();
            }
        } });
    }

    /** A ContentObserver saw adb_wifi_enabled or the accessibility settings change. */
    public synchronized Result onSettingChanged() { return pass("change"); }

    /** Wi-Fi came, went, or roamed to another BSSID. */
    public synchronized Result onNetworkChanged() { return pass("network"); }

    /** keeper.trust_bssids from the hub (replaces the list), then a pass so a newly trusted AP takes effect at once. */
    public synchronized Result setTrusted(List<String> bssids) throws Bssids.Invalid {
        trust.set(Bssids.parseTrusted(bssids));
        return pass("trust");
    }

    public synchronized void setAdbPort(Integer port) { adbPort = port != null && port >= 1 && port <= 65535 ? port : null; }

    public synchronized int passes() { return passes; }

    public synchronized List<String> triggers() { return new ArrayList<String>(triggers); }

    public synchronized Result last() { return last; }

    public synchronized Result pass(String trigger) {
        passes++;
        triggers.add(trigger);
        boolean wroteA11y = false, wroteAdb = false;
        Result r;
        try {
            Boolean trusted = null;
            String reason = null;
            String bssid = null;
            String state;
            if (!wifi.onWifi()) {
                state = "no_wifi";
                lastReported = null;
            } else {
                bssid = Bssids.readable(wifi.bssid());
                Set<String> list = trust.get();
                if (bssid == null) reason = "bssid_unreadable";
                else if (list == null || list.isEmpty()) reason = "no_trusted_list";
                else if (!list.contains(bssid)) reason = "not_in_trusted_list";
                trusted = Boolean.valueOf(reason == null);
                if (reason == null) {
                    state = "ok";
                    lastReported = null;
                    if (!"1".equals(settings.getGlobal(ADB_WIFI))) {
                        settings.putGlobal(ADB_WIFI, "1");
                        wroteAdb = true;
                    }
                } else {
                    state = "untrusted_bssid";
                    String key = reason + "/" + bssid;
                    if (!key.equals(lastReported)) {
                        lastReported = key;
                        reporter.untrustedBssid(bssid, reason);
                    }
                }
            }
            if (!A11Y_FOLLOWS_BSSID || Boolean.TRUE.equals(trusted)) wroteA11y = ensureA11y();
            r = new Result(state, trusted, reason, wroteAdb, wroteA11y);
        } catch (SecurityException e) {
            r = new Result("no_permission", null, null, wroteAdb, wroteA11y); // WRITE_SECURE_SETTINGS not granted
        }
        last = r;
        return r;
    }

    private boolean ensureA11y() {
        boolean wrote = false;
        String list = settings.getSecure(A11Y_LIST);
        if (!containsComponent(list, a11yComponent)) {
            settings.putSecure(A11Y_LIST, list == null || list.isEmpty() ? a11yComponent : list + ":" + a11yComponent);
            wrote = true;
        }
        if (!"1".equals(settings.getSecure(A11Y_ON))) {
            settings.putSecure(A11Y_ON, "1");
            wrote = true;
        }
        return wrote;
    }

    /** True when list (colon-separated flattened ComponentNames) names comp, in its full or short ("pkg/.Cls") form. */
    static boolean containsComponent(String list, String comp) {
        if (list == null || list.isEmpty()) return false;
        int slash = comp.indexOf('/');
        String pkg = comp.substring(0, slash), cls = comp.substring(slash + 1);
        String shortForm = cls.startsWith(pkg + ".") ? pkg + "/" + cls.substring(pkg.length()) : comp;
        for (String e : Arrays.asList(list.split(":"))) {
            String x = e.trim();
            if (x.equalsIgnoreCase(comp) || x.equalsIgnoreCase(shortForm)) return true;
        }
        return false;
    }

    /** Beacon fields for the heartbeat (link.json $defs/heartbeat). */
    public synchronized Map<String, Object> beacon() {
        Map<String, Object> b = new LinkedHashMap<String, Object>();
        b.put("adb_port", adbPort == null ? null : Long.valueOf(adbPort));
        b.put("adb_port_source", adbPort == null ? "none" : "nsd");
        Boolean wifiAdb = null, a11y = null;
        try {
            wifiAdb = Boolean.valueOf("1".equals(settings.getGlobal(ADB_WIFI)));
            a11y = Boolean.valueOf(containsComponent(settings.getSecure(A11Y_LIST), a11yComponent) && "1".equals(settings.getSecure(A11Y_ON)));
        } catch (RuntimeException e) {
            // unreadable: leave null
        }
        b.put("wifi_adb", wifiAdb);
        b.put("a11y_enabled", a11y);
        b.put("keeper", last == null ? "no_wifi" : last.state);
        b.put("bssid_trusted", last == null ? null : last.bssidTrusted);
        return b;
    }
}
