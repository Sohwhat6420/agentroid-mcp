package dev.agentroid.bankguard;

import android.app.AlarmManager;
import android.app.AppOpsManager;
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
import android.content.pm.ServiceInfo;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
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

/**
 * Polls the foreground app (UsageStatsManager, Usage Access; never an AccessibilityService) about every 400 ms while the
 * screen is on, and hands each reading to {@link GuardCore#tick}. Stops polling with the screen off; a held guard is
 * then woken once after {@link GuardLogic#SCREEN_OFF_RELEASE_MS} by an alarm, so a bank left open under a locked screen
 * does not keep debugging off all night.
 */
public final class GuardService extends Service {
    static final String ACTION_RELEASE = "dev.agentroid.bankguard.RELEASE";   // notification: debugging back on now
    static final String ACTION_PAUSE = "dev.agentroid.bankguard.PAUSE";       // notification: pause 1 h / resume
    static final String ACTION_WIFI_ON = "dev.agentroid.bankguard.WIFI_ON";   // notification: owner's consent
    static final String ACTION_TICK = "dev.agentroid.bankguard.TICK";         // alarm after screen-off
    static final String EXTRA_BOOT = "boot";
    static final long POLL_MS = 400;
    private static final int NOTE_ID = 1, NOTE_WIFI = 2, NOTE_REOPEN = 3;
    private static final String CH_STATUS = "status", CH_WIFI = "wifi"; // CH_WIFI: things the owner may act on

    private GuardCore core;
    private HandlerThread thread;
    private Handler h;
    private UsageStatsManager usm;
    private ConnectivityManager cm;
    private volatile boolean screenOn = true;
    private volatile boolean onWifi;
    private volatile long network;
    private String fg;
    private long fgSince, lastResumeTs, queriedTo;
    private boolean polling;
    private String shownText;
    private long ticks, tickNanos;

    private final Runnable poll = new Runnable() { public void run() { pollOnce(); } };

    private final BroadcastReceiver screen = new BroadcastReceiver() {
        @Override public void onReceive(Context c, Intent i) {
            screenOn = Intent.ACTION_SCREEN_ON.equals(i.getAction()) || Intent.ACTION_USER_PRESENT.equals(i.getAction());
            h.post(new Runnable() { public void run() { onScreen(); } });
        }
    };

    private final ConnectivityManager.NetworkCallback net = new ConnectivityManager.NetworkCallback() {
        @Override public void onCapabilitiesChanged(Network n, NetworkCapabilities caps) {
            onWifi = caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI);
            network = onWifi ? n.getNetworkHandle() : 0;
        }
        @Override public void onLost(Network n) { onWifi = false; network = 0; }
    };

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
        try { cm.registerDefaultNetworkCallback(net, h); } catch (Throwable t) { core.log("network callback: " + t); }
        IntentFilter f = new IntentFilter();
        f.addAction(Intent.ACTION_SCREEN_ON);
        f.addAction(Intent.ACTION_SCREEN_OFF);
        f.addAction(Intent.ACTION_USER_PRESENT);
        registerReceiver(screen, f);
        PowerManager pm = getSystemService(PowerManager.class);
        screenOn = pm == null || pm.isInteractive();
        core.prefs().registerOnSharedPreferenceChangeListener(prefsChanged);
    }

    private final SharedPreferences.OnSharedPreferenceChangeListener prefsChanged =
            new SharedPreferences.OnSharedPreferenceChangeListener() {
                public void onSharedPreferenceChanged(SharedPreferences sp, String key) {
                    if (GuardCore.KEY_BANKS.equals(key) || GuardCore.KEY_PAUSE_UNTIL.equals(key)) h.post(poll);
                }
            };

    @Override
    public int onStartCommand(Intent i, int flags, int startId) {
        final String action = i == null ? null : i.getAction();
        final boolean boot = i != null && i.getBooleanExtra(EXTRA_BOOT, false);
        h.post(new Runnable() { public void run() {
            if (boot) core.releaseNow("release_boot", onWifi, network); // after a reboot no bank is in front
            if (ACTION_RELEASE.equals(action)) {
                core.pauseFor(2 * 60_000L); // or the bank still in front would hold again at once
                core.releaseNow("release_owner", onWifi, network);
            } else if (ACTION_PAUSE.equals(action)) {
                boolean paused = core.paused(System.currentTimeMillis());
                core.pauseFor(paused ? 0 : 60 * 60_000L);
                if (!paused) core.releaseNow("release_paused", onWifi, network);
            } else if (ACTION_WIFI_ON.equals(action)) {
                core.wifiOnByOwner();
                cancelWifiNote();
            }
            onScreen();
        } });
        return START_STICKY;
    }

    private void onScreen() {
        if (screenOn) {
            cancelAlarm();
            if (!polling) { polling = true; queriedTo = 0; h.post(poll); }
        } else {
            polling = false;
            h.removeCallbacks(poll);
            pollOnce(); // record the screen-off moment in the state
            if (core.state().held) setAlarm(GuardLogic.SCREEN_OFF_RELEASE_MS + 5_000);
        }
    }

    private void pollOnce() {
        long t0 = System.nanoTime();
        long now = System.currentTimeMillis();
        boolean unlocked = getSystemService(UserManager.class).isUserUnlocked();
        if (!unlocked || !usageAccess()) {
            show(!unlocked ? "Waiting for the first unlock" : "Needs Usage access: open Agentroid Bank Guard");
            again();
            return;
        }
        if (core.paused(now)) {
            show("Paused: debugging is left as it is");
            again();
            return;
        }
        readForeground(now);
        GuardLogic.Action a = core.tick(fg, fgSince, onWifi, network, screenOn);
        if (a.event != null && a.event.endsWith("_wifi_left_off")) wifiNote();
        // The poll caught a bank opened directly while debugging was on: its start-up check may already have seen it.
        if ("hold".equals(a.event) && a.writes()) reopenNote(core.state().heldPkg);
        GuardLogic.State st = core.state();
        show(st.held ? "Debugging off while " + label(st.heldPkg) + " is open" : "Watching " + core.banks().size() + " apps");
        ticks++;
        tickNanos += System.nanoTime() - t0;
        again();
    }

    private void again() { if (polling && screenOn) h.postDelayed(poll, POLL_MS); }

    /** The latest app to come in front. queryEvents from where the last query stopped; on the first query, the last 6 h. */
    private void readForeground(long now) {
        long from = queriedTo == 0 ? now - 6 * 3_600_000L : queriedTo - 1_000;
        UsageEvents ev = usm().queryEvents(from, now);
        UsageEvents.Event e = new UsageEvents.Event();
        while (ev != null && ev.hasNextEvent()) {
            ev.getNextEvent(e);
            if (e.getEventType() != UsageEvents.Event.ACTIVITY_RESUMED || e.getTimeStamp() < lastResumeTs) continue;
            lastResumeTs = e.getTimeStamp();
            // fgSince is when this app came in front: a second activity of the same app does not move it
            String now2 = e.getPackageName() + "/" + e.getClassName();
            if (!GuardLogic.pkgOf(now2).equals(GuardLogic.pkgOf(fg))) fgSince = e.getTimeStamp();
            fg = now2;
        }
        queriedTo = now;
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
        return PendingIntent.getService(this, 3, new Intent(this, GuardService.class).setAction(ACTION_TICK),
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

    private PendingIntent self(String action, int code) {
        return PendingIntent.getService(this, code, new Intent(this, GuardService.class).setAction(action),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    private Notification status(String text) {
        Notification.Builder b = new Notification.Builder(this, CH_STATUS)
                .setSmallIcon(android.R.drawable.ic_lock_lock)
                .setContentTitle("Agentroid Bank Guard")
                .setContentText(text)
                .setOngoing(true)
                .setContentIntent(PendingIntent.getActivity(this, 0, new Intent(this, MainActivity.class), PendingIntent.FLAG_IMMUTABLE))
                .addAction(new Notification.Action.Builder(null, "Debugging back on", self(ACTION_RELEASE, 1)).build())
                .addAction(new Notification.Action.Builder(null,
                        core.paused(System.currentTimeMillis()) ? "Resume" : "Pause 1 h", self(ACTION_PAUSE, 2)).build());
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

    private void wifiNote() {
        Notification n = new Notification.Builder(this, CH_WIFI)
                .setSmallIcon(android.R.drawable.ic_lock_lock)
                .setContentTitle("Wireless debugging stayed off")
                .setContentText("The Wi-Fi network changed while a bank app was open. Tap to turn it back on.")
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
        w.println("screen_on=" + screenOn + " on_wifi=" + onWifi + " polling=" + polling + " fg=" + fg
                + " ticks=" + ticks + " mean_tick_us=" + (ticks == 0 ? 0 : tickNanos / ticks / 1000));
    }

    @Override
    public void onDestroy() {
        polling = false;
        try { unregisterReceiver(screen); } catch (Throwable ignored) { }
        try { cm.unregisterNetworkCallback(net); } catch (Throwable ignored) { }
        core.prefs().unregisterOnSharedPreferenceChangeListener(prefsChanged);
        if (thread != null) thread.quitSafely();
        super.onDestroy();
    }
}
