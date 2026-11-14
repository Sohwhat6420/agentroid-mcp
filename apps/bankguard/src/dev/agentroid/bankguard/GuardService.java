package dev.agentroid.bankguard;

import android.app.AlarmManager;
import android.app.AppOpsManager;
import android.app.KeyguardManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.app.usage.UsageEvents;
import android.app.usage.UsageStatsManager;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.content.pm.ServiceInfo;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.wifi.WifiInfo;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.os.PowerManager;
import android.os.Process;
import android.os.SystemClock;
import android.os.UserManager;

import java.io.FileDescriptor;
import java.io.PrintWriter;
import java.util.HashMap;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Set;

/**
 * Polls UsageEvents (Usage Access; never an AccessibilityService) about every 400 ms while the screen is on, keeps the
 * set of VISIBLE activities (so a bank in split screen, or under a dialog, still counts), and hands each reading to
 * {@link GuardCore#tick}. A locked screen counts as off. Stops polling with the screen off; a held guard is then woken
 * once after {@link GuardLogic#SCREEN_OFF_RELEASE_MS} by an alarm, so a bank left open under a locked screen does not
 * keep debugging off all night.
 */
public final class GuardService extends Service {
    static final String ACTION_RELEASE = "dev.agentroid.bankguard.RELEASE";   // notification: debugging back on now
    static final String ACTION_PAUSE = "dev.agentroid.bankguard.PAUSE";       // notification: pause 1 h / resume
    static final String ACTION_WIFI_ON = "dev.agentroid.bankguard.WIFI_ON";   // notification: owner's consent
    static final String ACTION_TICK = "dev.agentroid.bankguard.TICK";         // alarm after screen-off
    static final String EXTRA_BOOT = "boot";
    static final long POLL_MS = 400;
    private static final long OVERLAP_MS = 5_000; // re-read this much of the last query: late events still count
    private static final int NOTE_ID = 1, NOTE_WIFI = 2, NOTE_REOPEN = 3;
    private static final String CH_STATUS = "status", CH_WIFI = "wifi"; // CH_WIFI: things the owner may act on

    private GuardCore core;
    private HandlerThread thread;
    private Handler h;
    private UsageStatsManager usm;
    private ConnectivityManager cm;
    private KeyguardManager km;
    private boolean interactive = true;
    // Wi-Fi networks up now (handles in this boot), from a TRANSPORT_WIFI request that does not ask for internet:
    // an adb LAN with no internet still counts. All on the poll thread.
    private final Set<Long> wifiUp = new LinkedHashSet<Long>();
    private long network;
    private final GuardLogic.Visible visible = new GuardLogic.Visible();
    private long queriedTo, bankSinceWall;
    private String home;
    private boolean polling;
    private String shownText;
    private long ticks, tickNanos, clockDraws;

    private final Runnable poll = new Runnable() { public void run() { pollOnce(); } };

    private final BroadcastReceiver screen = new BroadcastReceiver() {
        @Override public void onReceive(Context c, final Intent i) {
            h.post(new Runnable() { public void run() {
                interactive = !Intent.ACTION_SCREEN_OFF.equals(i.getAction());
                onScreen();
                if (Intent.ACTION_SCREEN_ON.equals(i.getAction())) drawClocks();
            } });
        }
    };

    /**
     * The dot clocks' minute: the system's own ACTION_TIME_TICK (a non-wakeup broadcast the system sends anyway, and
     * only to a receiver registered at run time). Ignored while the screen is off, and redrawn once at screen-on, so
     * a clock costs nothing in the pocket. No alarm of this app's.
     */
    private final BroadcastReceiver clock = new BroadcastReceiver() {
        @Override public void onReceive(Context c, final Intent i) {
            h.post(new Runnable() { public void run() {
                if (interactive || !Intent.ACTION_TIME_TICK.equals(i.getAction())) drawClocks();
            } });
        }
    };

    private void drawClocks() {
        clockDraws++;
        ClockWidget.renderAll(this);
    }

    // Home Wi-Fi mode: the name and access point each Wi-Fi network up now was last seen with (poll thread only).
    private final Map<Long, String[]> wifiIds = new HashMap<Long, String[]>();
    private Boolean lastTrusted; // null until the first check

    /**
     * Wi-Fi networks, from the system's own callbacks: no polling, and nothing is woken for it. On Android 12+ the
     * callback asks for location info, so the network's name and access point arrive with its capabilities (when the
     * app holds location access and location is on; otherwise they arrive redacted, and the network is not trusted).
     */
    private final class Net extends ConnectivityManager.NetworkCallback {
        Net() { super(); }
        Net(int flags) { super(flags); }

        @Override public void onAvailable(Network n) {
            wifiUp.add(n.getNetworkHandle());
            if (network == 0) network = n.getNetworkHandle();
        }
        @Override public void onCapabilitiesChanged(Network n, NetworkCapabilities nc) {
            Object ti = nc.getTransportInfo();
            if (!(ti instanceof WifiInfo)) return;
            WifiInfo w = (WifiInfo) ti;
            String[] id = {w.getSSID(), w.getBSSID()};
            String[] was = wifiIds.put(n.getNetworkHandle(), id);
            // Signal-strength updates arrive here too: re-decide only when the network or access point changed.
            if (was == null || !eq(was[0], id[0]) || !eq(was[1], id[1])) checkHome();
        }
        @Override public void onLost(Network n) {
            long gone = n.getNetworkHandle();
            wifiUp.remove(gone);
            wifiIds.remove(gone);
            if (gone == network) {
                network = wifiUp.isEmpty() ? 0 : wifiUp.iterator().next();
                core.wifiGone();
            }
            checkHome();
        }
    }

    private static boolean eq(String a, String b) { return a == null ? b == null : a.equals(b); }

    private ConnectivityManager.NetworkCallback net;
    private NetworkRequest wifiRequest;

    /**
     * (Re-)register the Wi-Fi callback. Registering makes the system report every Wi-Fi network up now, with its
     * name and access point as this app may see them NOW. So it is re-armed whenever the home setting changes: a
     * callback registered before location access was granted holds redacted names, and deciding from those would
     * switch Wireless debugging off under a working connection.
     */
    private void armNet() {
        try { if (net != null) cm.unregisterNetworkCallback(net); } catch (Throwable ignored) { }
        wifiIds.clear();
        net = Build.VERSION.SDK_INT >= 31 ? new Net(ConnectivityManager.NetworkCallback.FLAG_INCLUDE_LOCATION_INFO) : new Net();
        try { cm.registerNetworkCallback(wifiRequest, net, h); } catch (Throwable t) { core.log("network callback: " + t); }
    }

    /** Decide once after a moment if no callback has decided yet (no Wi-Fi at all calls no callback). The moment lets
     *  the networks already up arrive first, so a working connection is never switched off and on again. */
    private void decideSoon() {
        h.postDelayed(new Runnable() { public void run() { if (lastTrusted == null || homeDirty) checkHome(); } }, 3_000);
    }

    /** On the home network now? Decided from the last-seen identity of every Wi-Fi network up (poll thread). */
    private void checkHome() {
        boolean t = false;
        for (String[] id : wifiIds.values()) if (core.trusted(id[0], id[1])) { t = true; break; }
        if (lastTrusted != null && lastTrusted == t && !homeDirty) return;
        lastTrusted = t;
        homeDirty = false;
        core.setOnTrusted(t);
    }

    private boolean homeDirty; // the home network setting changed: decide again even if "trusted" did not

    @Override public IBinder onBind(Intent i) { return null; }

    @Override
    public void onCreate() {
        super.onCreate();
        core = GuardCore.get(this);
        channels();
        startInForeground("Starting");
        thread = new HandlerThread("bankguard", Process.THREAD_PRIORITY_BACKGROUND);
        thread.start();
        h = new Handler(thread.getLooper());
        cm = getSystemService(ConnectivityManager.class);
        km = getSystemService(KeyguardManager.class);
        wifiRequest = new NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
                .removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .build();
        armNet();
        decideSoon();
        IntentFilter f = new IntentFilter();
        f.addAction(Intent.ACTION_SCREEN_ON);
        f.addAction(Intent.ACTION_SCREEN_OFF);
        f.addAction(Intent.ACTION_USER_PRESENT);
        registerReceiver(screen, f);
        IntentFilter t = new IntentFilter();
        t.addAction(Intent.ACTION_TIME_TICK);
        t.addAction(Intent.ACTION_TIME_CHANGED);
        t.addAction(Intent.ACTION_TIMEZONE_CHANGED);
        t.addAction(Intent.ACTION_DATE_CHANGED);
        registerReceiver(clock, t);
        PowerManager pm = getSystemService(PowerManager.class);
        interactive = pm == null || pm.isInteractive();
        h.post(new Runnable() { public void run() { drawClocks(); } });
        core.prefs().registerOnSharedPreferenceChangeListener(prefsChanged);
    }

    private final SharedPreferences.OnSharedPreferenceChangeListener prefsChanged =
            new SharedPreferences.OnSharedPreferenceChangeListener() {
                public void onSharedPreferenceChanged(SharedPreferences sp, String key) {
                    if (GuardCore.KEY_HOME_SSID.equals(key) || GuardCore.KEY_HOME_BSSIDS.equals(key)) {
                        h.post(new Runnable() { public void run() { homeDirty = true; armNet(); decideSoon(); } });
                        return;
                    }
                    if (!GuardCore.KEY_BANKS.equals(key) && !GuardCore.KEY_PAUSE_UNTIL.equals(key)) return;
                    h.post(new Runnable() { public void run() {
                        if (!polling) return;
                        h.removeCallbacks(poll); // one poll chain, never two
                        pollOnce();
                    } });
                }
            };

    @Override
    public int onStartCommand(Intent i, int flags, int startId) {
        final String action = i == null ? null : i.getAction();
        final boolean boot = i != null && i.getBooleanExtra(EXTRA_BOOT, false);
        // Every start may come from startForegroundService (boot, the alarm, the shortcut): be in the foreground again.
        startInForeground(shownText == null ? "Watching" : shownText);
        h.post(new Runnable() { public void run() {
            if (boot) leftOffNote(core.bootRelease());
            if (ACTION_RELEASE.equals(action)) {
                core.pauseFor(2 * 60_000L); // or the bank still in front would hold again at once
                leftOffNote(core.releaseNow("release_owner", network != 0, network));
                shownText = null;
            } else if (ACTION_PAUSE.equals(action)) {
                boolean paused = core.paused();
                core.pauseFor(paused ? 0 : 60 * 60_000L);
                if (!paused) leftOffNote(core.releaseNow("release_paused", network != 0, network));
                shownText = null;
            } else if (ACTION_WIFI_ON.equals(action)) {
                if (core.wifiOnByOwner()) cancelWifiNote();
            }
            onScreen();
        } });
        return START_STICKY;
    }

    private void onScreen() {
        if (interactive) {
            cancelAlarm();
            home = homePackage();
            if (!polling) { polling = true; h.removeCallbacks(poll); h.post(poll); }
        } else {
            polling = false;
            h.removeCallbacks(poll);
            pollOnce(); // record the screen-off moment in the state
            if (core.state().held) setAlarm(GuardLogic.SCREEN_OFF_RELEASE_MS + 5_000);
        }
    }

    private void pollOnce() {
        long t0 = System.nanoTime();
        boolean unlocked = getSystemService(UserManager.class).isUserUnlocked();
        if (!unlocked || !usageAccess()) {
            show(!unlocked ? "Waiting for the first unlock" : "Needs Usage access: open Agentroid Companion");
            again();
            return;
        }
        if (core.paused()) {
            show("Paused: debugging is left as it is");
            again();
            return;
        }
        readVisible();
        String bank = visible.bank(core.banks());
        String fg = visible.top;
        boolean locked = km != null && km.isKeyguardLocked();
        boolean screenOn = interactive && !locked; // a bank behind the keyguard is not in front
        boolean fgIsHome = fg != null && home != null && home.equals(GuardLogic.pkgOf(fg));
        GuardLogic.Action a = core.tick(fg, bank, fgIsHome, network != 0, network, screenOn, bankSinceWall);
        leftOffNote(a);
        // The poll caught a bank opened directly while debugging was on: its start-up check may already have seen it.
        if ("hold".equals(a.event) && a.writes()) reopenNote(core.state().heldPkg);
        GuardLogic.State st = core.state();
        show(st.held ? "Debugging off while " + label(st.heldPkg) + " is open" : "Watching " + core.banks().size() + " apps");
        ticks++;
        tickNanos += System.nanoTime() - t0;
        again();
    }

    private void again() { if (polling && interactive) h.postDelayed(poll, POLL_MS); }

    /**
     * Applies new UsageEvents to the visible set, in order. Each query re-reads the last OVERLAP_MS (replaying events
     * is harmless), so events reported late still count. The first query, and any query after the wall clock moved
     * back, rebuilds the set from the last 6 h. UsageEvents carry wall-clock times; nothing here compares them with
     * each other, so a clock change cannot freeze the set.
     */
    private void readVisible() {
        long now = System.currentTimeMillis();
        long from;
        if (queriedTo == 0 || now < queriedTo - OVERLAP_MS) { visible.clear(); from = now - 6 * 3_600_000L; }
        else from = queriedTo - OVERLAP_MS;
        UsageEvents ev = usm().queryEvents(from, now);
        UsageEvents.Event e = new UsageEvents.Event();
        Set<String> banks = core.banks();
        while (ev != null && ev.hasNextEvent()) {
            ev.getNextEvent(e);
            int type = e.getEventType();
            if (type != GuardLogic.EV_RESUMED && type != GuardLogic.EV_STOPPED && type != GuardLogic.EV_DESTROYED) continue;
            visible.event(type, e.getPackageName(), e.getClassName());
            if (type == GuardLogic.EV_RESUMED && GuardLogic.isBank(banks, e.getPackageName() + "/" + e.getClassName())) {
                bankSinceWall = e.getTimeStamp();
            }
        }
        queriedTo = now;
    }

    private String homePackage() {
        try {
            ResolveInfo r = getPackageManager().resolveActivity(
                    new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_HOME), PackageManager.MATCH_DEFAULT_ONLY);
            return r == null || r.activityInfo == null ? null : r.activityInfo.packageName;
        } catch (Throwable t) { return null; }
    }

    private UsageStatsManager usm() {
        if (usm == null) usm = getSystemService(UsageStatsManager.class);
        return usm;
    }

    private boolean usageAccess() {
        AppOpsManager ao = getSystemService(AppOpsManager.class);
        int mode = ao.unsafeCheckOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, Process.myUid(), getPackageName());
        return mode == AppOpsManager.MODE_ALLOWED;
    }

    private String label(String pkg) {
        if (pkg == null) return "a bank app";
        try { return String.valueOf(getPackageManager().getApplicationLabel(getPackageManager().getApplicationInfo(pkg, 0))); }
        catch (Throwable t) { return pkg; }
    }

    // ------------------------------------------------------------------------------------------- the alarm

    private PendingIntent tickIntent() {
        return PendingIntent.getForegroundService(this, 3, new Intent(this, GuardService.class).setAction(ACTION_TICK),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    private void setAlarm(long inMs) {
        AlarmManager am = getSystemService(AlarmManager.class);
        am.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, SystemClock.elapsedRealtime() + inMs, tickIntent());
    }

    private void cancelAlarm() { getSystemService(AlarmManager.class).cancel(tickIntent()); }

    // ---------------------------------------------------------------------------------------- notifications

    private void channels() {
        NotificationManager nm = getSystemService(NotificationManager.class);
        NotificationChannel s = new NotificationChannel(CH_STATUS, "Bank guard status", NotificationManager.IMPORTANCE_MIN);
        s.setShowBadge(false);
        nm.createNotificationChannel(s);
        nm.createNotificationChannel(new NotificationChannel(CH_WIFI, "Bank guard notices", NotificationManager.IMPORTANCE_DEFAULT));
    }

    /** One-shot: a button works once, and the next status note carries a fresh one. Anything that can read
     *  notifications (a listener, a watch bridge) cannot replay it later. */
    private PendingIntent self(String action, int code) {
        return PendingIntent.getService(this, code, new Intent(this, GuardService.class).setAction(action),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_ONE_SHOT);
    }

    private Notification status(String text) {
        Notification.Builder b = new Notification.Builder(this, CH_STATUS)
                .setSmallIcon(android.R.drawable.ic_lock_lock)
                .setContentTitle("Bank guard")
                .setContentText(text)
                .setOngoing(true)
                .setContentIntent(PendingIntent.getActivity(this, 0, new Intent(this, MainActivity.class), PendingIntent.FLAG_IMMUTABLE))
                .addAction(new Notification.Action.Builder(null, "Debugging back on", self(ACTION_RELEASE, 1)).build())
                .addAction(new Notification.Action.Builder(null,
                        core.paused() ? "Resume" : "Pause 1 h", self(ACTION_PAUSE, 2)).build());
        return b.build();
    }

    private void startInForeground(String text) {
        shownText = text;
        if (Build.VERSION.SDK_INT >= 34) startForeground(NOTE_ID, status(text), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        else startForeground(NOTE_ID, status(text));
    }

    private void show(String text) {
        if (text.equals(shownText)) return;
        shownText = text;
        getSystemService(NotificationManager.class).notify(NOTE_ID, status(text));
    }

    /** Every path that releases can leave Wireless debugging off; each one tells the owner. */
    private void leftOffNote(GuardLogic.Action a) {
        if (a == null || a.event == null || !a.event.endsWith("_wifi_left_off")) return;
        Notification n = new Notification.Builder(this, CH_WIFI)
                .setSmallIcon(android.R.drawable.ic_lock_lock)
                .setContentTitle("Wireless debugging stayed off")
                .setContentText("The Wi-Fi network changed (or the phone restarted) while a bank app was open. Tap to turn it back on.")
                .setAutoCancel(true)
                .setContentIntent(self(ACTION_WIFI_ON, 4))
                .build();
        getSystemService(NotificationManager.class).notify(NOTE_WIFI, n);
    }

    private void reopenNote(String pkg) {
        Notification n = new Notification.Builder(this, CH_WIFI)
                .setSmallIcon(android.R.drawable.ic_lock_lock)
                .setContentTitle("Debugging is off for " + label(pkg))
                .setContentText("If it showed a debugging warning, close it and open it again. A \"(safe)\" shortcut avoids this.")
                .setAutoCancel(true)
                .setTimeoutAfter(60_000)
                .build();
        getSystemService(NotificationManager.class).notify(NOTE_REOPEN, n);
    }

    private void cancelWifiNote() { getSystemService(NotificationManager.class).cancel(NOTE_WIFI); }

    // ------------------------------------------------------------------------------------------------- dump

    /** `adb shell dumpsys activity service dev.agentroid.bankguard/.GuardService` (the shell holds DUMP). */
    @Override
    protected void dump(FileDescriptor fd, PrintWriter w, String[] args) {
        w.print(core.dump());
        w.println("interactive=" + interactive + " wifi_networks=" + wifiUp.size() + " polling=" + polling
                + " top=" + visible.top + " visible=" + visible.size() + " home=" + home
                + " ticks=" + ticks + " mean_tick_us=" + (ticks == 0 ? 0 : tickNanos / ticks / 1000)
                + " clock_draws=" + clockDraws);
    }

    @Override
    public void onDestroy() {
        polling = false;
        try { unregisterReceiver(screen); } catch (Throwable ignored) { }
        try { unregisterReceiver(clock); } catch (Throwable ignored) { }
        try { cm.unregisterNetworkCallback(net); } catch (Throwable ignored) { }
        core.prefs().unregisterOnSharedPreferenceChangeListener(prefsChanged);
        if (thread != null) thread.quitSafely();
        super.onDestroy();
    }
}
