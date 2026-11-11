package dev.agentroid.keeper;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.ServiceInfo;
import android.database.ContentObserver;
import android.graphics.drawable.Icon;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.net.wifi.WifiInfo;
import android.net.wifi.WifiManager;
import android.os.BatteryManager;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.os.SystemClock;
import android.provider.Settings;

import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;

/**
 * The keeper: a connectedDevice foreground service with no listening socket.
 * It keeps Settings.Global.adb_wifi_enabled = 1 across Wi-Fi changes, boots and the 15-min backstop,
 * discovers the adb-tls port via NsdManager, and posts an outbound heartbeat to the hub (if one is set).
 * The decision logic lives in the android-free KeeperLogic.
 */
public class KeeperService extends Service {
    static final String CH = "keeper";
    static final int NID = 7311;
    static final String PREFS = "keeper";
    static final String KEY_HUB = "hub";
    static final String KEY_SALT = "ssid_salt";
    static final String KEY_SERIAL = "hw_serial";
    static final String KEY_PAUSE_UNTIL = "pause_until"; // v0.2: epoch ms; `--es pause 1` pauses for PAUSE_DEFAULT_MS (or `--es pause_min N`, max 12 h), `--es pause 0` resumes
    static final long PAUSE_DEFAULT_MS = 2 * 60 * 60_000L;
    static final long PAUSE_MAX_MS = 12 * 60 * 60_000L;
    static final String ADB_WIFI = "adb_wifi_enabled"; // Settings.Global key

    static final long HEARTBEAT_MS = 60_000L;
    static final int READBACK_TOTAL_MS = 5_000;
    static final int READBACK_STEP_MS = 250;

    static final String ACTION_ALARM = "dev.agentroid.keeper.ALARM";

    private HandlerThread worker;
    private Handler bg;                 // keeper loop + heartbeat run here (off the main thread)
    private final Handler main = new Handler(Looper.getMainLooper());

    private volatile String hub;
    private volatile int lastPort = -1;
    private volatile int lastAdbWifi = -1;

    private ContentObserver adbWifiObserver;
    private ConnectivityManager.NetworkCallback netCb;

    // While the network is gone, briefly wake the screen: on HyperOS a screen-off phone can take minutes (up to 30 in
    // the 31 Oct runs) to rejoin Wi-Fi, against 12 s with the screen on.
    private volatile boolean netGone;
    private int nudgesSent;
    private PowerManager.WakeLock goneLock;   // keeps the CPU awake while the network is gone, so the nudge timer can run

    private void holdGoneLock() {
        try {
            if (goneLock == null) goneLock = getSystemService(PowerManager.class).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "keeper:netgone");
            goneLock.acquire(10 * 60_000L);   // times out by itself
        } catch (Throwable t) { L.e("holdGoneLock", t); }
    }

    private void releaseGoneLock() {
        try { if (goneLock != null && goneLock.isHeld()) goneLock.release(); } catch (Throwable ignored) {}
    }
    private final Runnable nudge = new Runnable() {
        @Override public void run() {
            if (!netGone) return;
            try {
                PowerManager pm = getSystemService(PowerManager.class);
                @SuppressWarnings("deprecation")
                PowerManager.WakeLock wl = pm.newWakeLock(PowerManager.SCREEN_DIM_WAKE_LOCK
                        | PowerManager.ACQUIRE_CAUSES_WAKEUP, "keeper:nudge");
                wl.acquire(8_000L);
                L.i("nudge " + (nudgesSent + 1) + ": screen woken, network still gone");
            } catch (Throwable t) { L.e("nudge", t); }
            nudgesSent++;
            long next = KeeperLogic.nudgeDelayMs(nudgesSent);
            if (next >= 0) main.postDelayed(this, next);
        }
    };

    static void start(Context c, String hub, String src) {
        Intent i = new Intent(c, KeeperService.class).putExtra("src", src);
        if (hub != null) i.putExtra("hub", hub);
        try {
            c.startForegroundService(i);
        } catch (Throwable t) {
            L.e("startForegroundService(src=" + src + ") REFUSED", t);
        }
    }

    @Override
    public void onCreate() {
        super.onCreate();
        L.init(this);
        worker = new HandlerThread("keeper-bg");
        worker.start();
        bg = new Handler(worker.getLooper());

        NotificationChannel ch = new NotificationChannel(CH, "Keeper", NotificationManager.IMPORTANCE_MIN);
        ch.setShowBadge(false);
        getSystemService(NotificationManager.class).createNotificationChannel(ch);

        migratePrefs();
        L.i("KeeperService onCreate sdk=" + Build.VERSION.SDK_INT + " exactAlarms=" + getSystemService(android.app.AlarmManager.class).canScheduleExactAlarms());
        goForeground();
        ensureSalt();
        watchNetworks();
        watchAdbWifiSetting();
        Alarms.schedule(this);
        bg.postDelayed(heartbeatTick, HEARTBEAT_MS);
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String src = intent == null ? "sticky" : String.valueOf(intent.getStringExtra("src"));
        String extra = intent == null ? null : intent.getStringExtra("hub");
        SharedPreferences p = prefs();
        if (extra != null && !extra.isEmpty()) p.edit().putString(KEY_HUB, extra.trim()).apply();
        // The hub reads ro.serialno over adb at setup and hands it over (`--es serial`), because an app
        // cannot read the hardware serial itself; the laptop watchdog matches heartbeats on it.
        String serialExtra = intent == null ? null : intent.getStringExtra("serial");
        if (serialExtra != null && !serialExtra.isEmpty()) p.edit().putString(KEY_SERIAL, serialExtra.trim()).apply();
        String pauseExtra = intent == null ? null : intent.getStringExtra("pause");
        if (pauseExtra != null) {
            String pm = intent.getStringExtra("pause_min");
            long ms = KeeperLogic.pauseMillis(pauseExtra, pm, PAUSE_DEFAULT_MS, PAUSE_MAX_MS);
            if (ms < 0) L.i("pause extra not understood: " + pauseExtra + " (use 1 or 0); ignored");
            else {
                p.edit().putLong(KEY_PAUSE_UNTIL, ms == 0 ? 0 : System.currentTimeMillis() + ms).apply();
                L.i(ms == 0 ? "pause cleared" : "paused for " + (ms / 60_000L) + " min");
            }
        }
        hub = p.getString(KEY_HUB, null);
        boolean alarm = intent != null && ACTION_ALARM.equals(intent.getAction());
        L.i("onStartCommand src=" + src + " alarm=" + alarm + " hub=" + hub);
        goForeground();
        runKeeperLoop(alarm ? "alarm" : src);
        return START_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) { return null; }

    @Override
    public void onDestroy() {
        L.i("KeeperService onDestroy");
        try { if (adbWifiObserver != null) getContentResolver().unregisterContentObserver(adbWifiObserver); } catch (Throwable ignored) {}
        try { if (netCb != null) getSystemService(ConnectivityManager.class).unregisterNetworkCallback(netCb); } catch (Throwable ignored) {}
        if (worker != null) worker.quitSafely();
        super.onDestroy();
    }

    // ---------------------------------------------------------------- foreground

    private void goForeground() {
        try {
            Notification n = new Notification.Builder(this, CH)
                    .setContentTitle("Keeper")
                    .setContentText("maintaining wireless debugging")
                    .setSmallIcon(Icon.createWithResource(this, android.R.drawable.ic_menu_info_details))
                    .setOngoing(true)
                    .build();
            startForeground(NID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE);
        } catch (Throwable t) {
            L.e("startForeground REFUSED", t);
        }
    }

    // ---------------------------------------------------------------- prefs / salt

    // Device-protected storage so the service works after a reboot before the first unlock (direct boot).
    private SharedPreferences prefs() {
        Context dp = isDeviceProtectedStorage() ? this : createDeviceProtectedStorageContext();
        return dp.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    private void migratePrefs() {
        try {
            if (!isDeviceProtectedStorage()) createDeviceProtectedStorageContext().moveSharedPreferencesFrom(this, PREFS);
        } catch (Throwable t) { L.e("migratePrefs", t); }
    }

    private void ensureSalt() {
        SharedPreferences p = prefs();
        if (p.getString(KEY_SALT, null) == null) {
            p.edit().putString(KEY_SALT, Long.toHexString(new java.security.SecureRandom().nextLong())).apply();
        }
    }

    // ---------------------------------------------------------------- the keeper loop

    private void runKeeperLoop(final String why) {
        bg.post(() -> {
            try { doKeeperLoop(why); } catch (Throwable t) { L.e("keeper loop", t); }
        });
    }

    private boolean paused() { return System.currentTimeMillis() < prefs().getLong(KEY_PAUSE_UNTIL, 0); }

    private void doKeeperLoop(String why) {
        if (KeeperLogic.shouldSkipRun(paused())) { L.i("loop why=" + why + " SKIPPED (paused)"); return; }
        int before = readAdbWifi();
        if (KeeperLogic.shouldWriteEnable(before)) writeAdbWifi(1);
        int after = before;
        long deadline = SystemClock.uptimeMillis() + READBACK_TOTAL_MS;
        while (SystemClock.uptimeMillis() < deadline) {
            after = readAdbWifi();
            if (after == 0) break;           // reverted: stop early, it is untrusted
            sleep(READBACK_STEP_MS);
        }
        KeeperLogic.Outcome o = KeeperLogic.evaluateRun(before, after);
        lastAdbWifi = after;
        L.i("loop why=" + why + " before=" + before + " after=" + after + " -> " + KeeperLogic.logLineFor(o));
        sendHeartbeat("loop:" + why);
    }

    private int readAdbWifi() {
        try {
            return Settings.Global.getInt(getContentResolver(), ADB_WIFI, 0);
        } catch (Throwable t) {
            L.e("readAdbWifi", t);
            return 0;
        }
    }

    private void writeAdbWifi(int v) {
        try {
            boolean ok = Settings.Global.putInt(getContentResolver(), ADB_WIFI, v);
            if (!ok) L.i("writeAdbWifi=" + v + " returned false (WRITE_SECURE_SETTINGS missing?)");
        } catch (SecurityException se) {
            L.e("writeAdbWifi SecurityException (grant WRITE_SECURE_SETTINGS over adb)", se);
        } catch (Throwable t) {
            L.e("writeAdbWifi", t);
        }
    }

    private static void sleep(long ms) {
        try { Thread.sleep(ms); } catch (InterruptedException e) { Thread.currentThread().interrupt(); }
    }

    // ---------------------------------------------------------------- triggers

    private void watchNetworks() {
        ConnectivityManager cm = getSystemService(ConnectivityManager.class);
        netCb = new ConnectivityManager.NetworkCallback() {
            @Override public void onAvailable(Network n) {
                L.i("net AVAILABLE " + n);
                netGone = false;
                main.removeCallbacks(nudge);
                releaseGoneLock();
                runKeeperLoop("net-available");
            }
            @Override public void onLost(Network n) {
                L.i("net LOST " + n);
                netGone = true;
                nudgesSent = 0;
                main.removeCallbacks(nudge);
                holdGoneLock();
                main.postDelayed(nudge, KeeperLogic.nudgeDelayMs(0));
            }
        };
        try { cm.registerDefaultNetworkCallback(netCb, main); } catch (Throwable t) { L.e("registerDefaultNetworkCallback", t); }
    }

    private void watchAdbWifiSetting() {
        adbWifiObserver = new ContentObserver(main) {
            @Override public void onChange(boolean selfChange) {
                L.i("observer adb_wifi_enabled changed");
                runKeeperLoop("observer");
            }
        };
        try {
            getContentResolver().registerContentObserver(Settings.Global.getUriFor(ADB_WIFI), false, adbWifiObserver);
        } catch (Throwable t) { L.e("registerContentObserver", t); }
    }

    // ---------------------------------------------------------------- port discovery (NsdManager)

    private void discoverPort() {
        final NsdManager nm = getSystemService(NsdManager.class);
        final NsdManager.DiscoveryListener[] dl = new NsdManager.DiscoveryListener[1];
        dl[0] = new NsdManager.DiscoveryListener() {
            @Override public void onStartDiscoveryFailed(String t, int e) { L.i("nsd start failed " + e); }
            @Override public void onStopDiscoveryFailed(String t, int e) { }
            @Override public void onDiscoveryStarted(String t) { }
            @Override public void onDiscoveryStopped(String t) { }
            @Override public void onServiceLost(NsdServiceInfo s) { }
            @Override public void onServiceFound(NsdServiceInfo s) {
                nm.resolveService(s, new NsdManager.ResolveListener() {
                    @Override public void onResolveFailed(NsdServiceInfo i, int e) { L.i("nsd resolve failed " + e); }
                    @Override public void onServiceResolved(NsdServiceInfo i) {
                        lastPort = i.getPort();
                        L.i("nsd resolved adb-tls port=" + lastPort + " name=" + i.getServiceName());
                        sendHeartbeat("port");
                    }
                });
            }
        };
        try {
            nm.discoverServices("_adb-tls-connect._tcp", NsdManager.PROTOCOL_DNS_SD, dl[0]);
            main.postDelayed(() -> { try { nm.stopServiceDiscovery(dl[0]); } catch (Throwable ignored) {} }, 8_000);
        } catch (Throwable t) { L.e("discoverPort", t); }
    }

    // ---------------------------------------------------------------- heartbeat (outbound only)

    private final Runnable heartbeatTick = new Runnable() {
        @Override public void run() {
            try { discoverPort(); sendHeartbeat("tick"); } catch (Throwable t) { L.e("heartbeatTick", t); }
            bg.postDelayed(this, HEARTBEAT_MS);
        }
    };

    private void sendHeartbeat(final String why) {
        final String h = hub;
        if (paused()) { L.i("paused; skip heartbeat (" + why + ")"); return; }
        if (h == null || h.isEmpty()) { L.i("no hub set; skip heartbeat (" + why + ")"); return; }
        bg.post(() -> postHeartbeat(h, why));
    }

    private void postHeartbeat(String h, String why) {
        String payload = KeeperLogic.buildHeartbeat(
                serialHint(), lastPort, readAdbWifi(), wifiSsidHash(), battery(), System.currentTimeMillis());
        String url = h.startsWith("http") ? h : ("http://" + h + "/keeper/heartbeat");
        HttpURLConnection c = null;
        try {
            c = (HttpURLConnection) new URL(url).openConnection();
            c.setConnectTimeout(5000);
            c.setReadTimeout(5000);
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setRequestProperty("Content-Type", "application/json");
            byte[] body = payload.getBytes(StandardCharsets.UTF_8);
            try (OutputStream os = c.getOutputStream()) { os.write(body); }
            int code = c.getResponseCode();
            L.i("heartbeat (" + why + ") -> " + url + " code=" + code + " " + payload);
        } catch (Throwable t) {
            // Fails quietly: one short log line, no stack trace, no notification, no retry loop. The hub is
            // optional and the keeper's real job (adb_wifi_enabled) never waits on it.
            L.i("heartbeat (" + why + ") not delivered: " + t.getClass().getSimpleName());
        } finally {
            if (c != null) c.disconnect();
        }
    }

    @SuppressWarnings("deprecation")
    private String wifiSsidHash() {
        try {
            WifiManager wm = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            WifiInfo wi = wm == null ? null : wm.getConnectionInfo();
            String ssid = wi == null ? null : wi.getSSID();
            if (ssid == null || ssid.contains("unknown")) return null;
            if (ssid.startsWith("\"") && ssid.endsWith("\"") && ssid.length() >= 2) ssid = ssid.substring(1, ssid.length() - 1);
            return KeeperLogic.ssidHash(ssid, prefs().getString(KEY_SALT, ""));
        } catch (Throwable t) {
            return null;
        }
    }

    private int battery() {
        try {
            BatteryManager bm = (BatteryManager) getSystemService(Context.BATTERY_SERVICE);
            return bm == null ? -1 : bm.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY);
        } catch (Throwable t) { return -1; }
    }

    @SuppressWarnings("HardwareIds")
    private String serialHint() {
        String hw = prefs().getString(KEY_SERIAL, null);
        if (hw != null) return hw;          // set by setup-keeper.sh from the hub's own ro.serialno read
        try {
            // ANDROID_ID is a non-privileged, per-signing-key hint (research verdict 9), not the real serial.
            return Settings.Secure.getString(getContentResolver(), Settings.Secure.ANDROID_ID);
        } catch (Throwable t) { return null; }
    }
}
