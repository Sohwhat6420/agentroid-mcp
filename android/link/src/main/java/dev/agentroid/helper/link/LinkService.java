/*
 * Foreground start, sticky restart and the default-network watch are adapted from true-phone-link companion-phase0
 * SpikeService.java (lines 99-118 onCreate, 149 respawn, 554 watchNetworks) via the S4 spike's S4Link.java.
 * No mobilerun-portal code was read or copied (AGPL; clean-room, see HELPER-REPORT.md).
 */
package dev.agentroid.helper.link;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.net.ConnectivityManager;
import android.net.Network;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.util.Log;

import java.io.FileDescriptor;
import java.io.PrintWriter;
import java.security.SecureRandom;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Random;

import dev.agentroid.helper.core.Dispatcher;
import dev.agentroid.helper.core.HelperMethods;
import dev.agentroid.helper.core.HubSpec;
import dev.agentroid.helper.core.LinkSession;
import dev.agentroid.helper.core.NotificationText;
import dev.agentroid.helper.core.PortRegistry;
import dev.agentroid.helper.link.net.DumpText;
import dev.agentroid.helper.link.net.ExecutorClock;
import dev.agentroid.helper.link.net.LinkChallenge;
import dev.agentroid.helper.link.net.LinkProto;
import dev.agentroid.helper.link.net.LinkClient;
import dev.agentroid.helper.link.net.LinkEnv;
import dev.agentroid.helper.link.net.OkHttpTransport;
import dev.agentroid.helper.link.net.TailnetGuard;

/**
 * The link. ADR 0001: the phone DIALS OUT to the hub (ws:// on the tailnet) and keeps no listening socket, so the
 * "endpoint the daemon connects to" is the hub's, and this is the client end of it.
 *
 * It is a connectedDevice foreground service whose notification is ongoing, names the controlling laptop and carries a
 * Stop action. If the user swipes the notification away (Android 14+ allows that for most foreground services) the
 * delete intent re-posts it at once. If notifications are switched off for the app, the session refuses to act.
 *
 * 2.12: the connection is {@link LinkClient} (protocol v1, schemas/link.json) over {@link OkHttpTransport}, on one
 * {@link ExecutorClock} thread; identity is the AndroidKeyStore key ({@link KeystoreIdentity}), whose PUBLIC key
 * {@link #dump} prints for `adb shell dumpsys activity service dev.agentroid.helper/.link.LinkService`. The 2.5 stub
 * challenge is gone: {@link LinkChallenge} only mirrors the link-layer state. A module such as :keeper rides along as
 * a {@link LinkExtension}, found by class name.
 *
 * Logging is limited to fixed strings: no address, no laptop name, no typed text, no screen content.
 */
public class LinkService extends Service {
    private static final String TAG = "AgentroidLink";
    private static final String CHANNEL = "agentroid_link";
    private static final int NOTIFICATION_ID = 4201;
    private static final String ACTION_STOP = "dev.agentroid.helper.link.STOP_SERVICE";
    private static final String ACTION_REPOST = "dev.agentroid.helper.link.REPOST";

    private final Handler main = new Handler(Looper.getMainLooper());
    private volatile LinkClient client;
    private final LinkSession session = new LinkSession(new LinkChallenge(new LinkChallenge.ReadyCheck() {
        public boolean linkReady() {
            LinkClient c = client;
            return c != null && c.phase() == LinkClient.Phase.READY;
        }
    }));
    private Dispatcher dispatcher;
    private ExecutorClock clock;
    private KeystoreIdentity identity;
    private AndroidPower power;
    private LinkExtension extension;
    private HubStore store;
    private ConnectivityManager.NetworkCallback netCallback;

    private volatile boolean stopped;
    private volatile HubSpec hub;

    /** Start (or nudge) the link as a foreground service. */
    public static void start(Context c, String src) {
        try {
            c.startForegroundService(new Intent(c, LinkService.class).putExtra("src", src));
        } catch (RuntimeException e) {
            Log.w(TAG, "foreground start was refused");
        }
    }

    /** Ask a running link to stop. A link that is not running has nothing to stop. */
    public static void stop(Context c) {
        try {
            c.startService(new Intent(c, LinkService.class).setAction(ACTION_STOP));
        } catch (RuntimeException e) {
            Log.i(TAG, "stop requested while not running");
        }
    }

    // ------------------------------------------------------------------ lifecycle

    @Override
    public void onCreate() {
        super.onCreate();
        store = new HubStore(this);
        NotificationChannel ch = new NotificationChannel(CHANNEL, "Agentroid helper status", NotificationManager.IMPORTANCE_LOW);
        ch.setShowBadge(false);
        getSystemService(NotificationManager.class).createNotificationChannel(ch);
        identity = new KeystoreIdentity();
        clock = new ExecutorClock("agentroid-link");
        dispatcher = new Dispatcher(session);
        HelperMethods.register(dispatcher, PortRegistry.shared(), new AndroidEnv(this));
        extension = loadExtension();
        PortRegistry.shared().addHumanTouchListener(touchToHub);
        power = new AndroidPower(this, new Runnable() {
            public void run() {
                LinkClient c = client;
                if (c != null) c.onPowerChanged();
            }
        });
        client = new LinkClient(clock, new OkHttpTransport(), identity, store, session, new Env(),
                new SecureRandom(), new Random(), AndroidEnv.VERSION, Build.VERSION.SDK_INT);
        watchNetwork();
    }

    /** The overlay's human touches, to the hub as human_touch frames (sequenced; held while the link is down). */
    private final PortRegistry.HumanTouchListener touchToHub = new PortRegistry.HumanTouchListener() {
        @Override public void touched(long elapsedRealtimeMs) {
            LinkClient c = client;
            if (c == null) return;
            long now = android.os.SystemClock.elapsedRealtime();
            c.sendEvent(LinkProto.humanTouch(Math.min(elapsedRealtimeMs, now), now));
        }
    };

    /** :keeper's extension when this flavour carries it (full), else null. */
    private LinkExtension loadExtension() {
        try {
            LinkExtension e = (LinkExtension) Class.forName(LinkExtension.KEEPER_CLASS).getDeclaredConstructor().newInstance();
            e.attach(this, dispatcher, new LinkExtension.Host() {
                public void event(Map<String, Object> body) {
                    LinkClient c = client;
                    if (c != null) c.sendEvent(body);
                }

                public String serial() { return store.serial(); }
            });
            return e;
        } catch (ClassNotFoundException e) {
            return null; // helper flavour: no keeper
        } catch (ReflectiveOperationException | RuntimeException e) {
            Log.w(TAG, "keeper extension failed to start");
            return null;
        }
    }

    /** The phone side of LinkClient: power, beacon, and running a request through the 2.5 Dispatcher. */
    private final class Env implements LinkEnv {
        @Override public boolean charging() { return power.charging(); }

        @Override public boolean screenOn() { return power.screenOn(); }

        @Override
        public Map<String, Object> beacon() {
            Map<String, Object> b = new LinkedHashMap<String, Object>();
            b.put("battery_pct", power.batteryPct());
            LinkExtension e = extension;
            if (e != null) {
                try {
                    b.putAll(e.beacon());
                } catch (RuntimeException ex) {
                    b.put("keeper", "absent");
                }
            }
            return b;
        }

        @Override
        public byte[] execute(byte[] rpcFrame) {
            session.setNotificationVisible(notificationVisible());
            byte[] out = dispatcher.handleFrame(rpcFrame);
            refreshNotification();
            return out;
        }

        @Override public void onStateChanged() { refreshNotification(); }
    }

    /**
     * `adb shell dumpsys activity service dev.agentroid.helper/.link.LinkService` (the caller needs
     * android.permission.DUMP; the adb shell has it). Prints the PUBLIC key only: see DumpText.
     */
    @Override
    protected void dump(FileDescriptor fd, PrintWriter pw, String[] args) {
        pw.print(DumpText.of(identity != null ? identity : new KeystoreIdentity()));
        pw.flush();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? null : intent.getAction();
        if (ACTION_STOP.equals(action)) {
            store.setEnabled(false);
            // startForeground first if we were started cold, so Android does not kill us for skipping it.
            goForeground();
            shutdown();
            return START_NOT_STICKY;
        }
        goForeground(); // within 5 s of startForegroundService, whatever happens next
        if (ACTION_REPOST.equals(action)) return START_STICKY;
        hub = store.load();
        if (hub == null || !store.enabled()) {
            shutdown();
            return START_NOT_STICKY;
        }
        stopped = false;
        refreshNotification();
        try {
            client.start(hub.host, hub.port);
        } catch (TailnetGuard.Refused e) {
            Log.w(TAG, "stored hub is not a tailnet address; link not started");
            shutdown();
            return START_NOT_STICKY;
        }
        return START_STICKY;
    }

    @Override public IBinder onBind(Intent intent) { return null; }

    @Override
    public void onDestroy() {
        PortRegistry.shared().removeHumanTouchListener(touchToHub);
        stopped = true;
        if (netCallback != null) {
            try {
                getSystemService(ConnectivityManager.class).unregisterNetworkCallback(netCallback);
            } catch (RuntimeException ignored) {
                // already unregistered
            }
        }
        main.removeCallbacksAndMessages(null);
        if (client != null) client.stop();
        if (power != null) power.close();
        if (extension != null) {
            try {
                extension.detach();
            } catch (RuntimeException ignored) {
                // nothing more to do while going down
            }
        }
        final ExecutorClock c = clock;
        if (c != null) c.schedule(200, new Runnable() { public void run() { c.shutdown(); } }); // after the stop has run
        session.onDisconnect();
        super.onDestroy();
    }

    private void shutdown() {
        stopped = true;
        main.removeCallbacksAndMessages(null);
        if (client != null) client.stop();
        stopForeground(STOP_FOREGROUND_REMOVE);
        stopSelf();
    }

    // ------------------------------------------------------------------ notification

    private void goForeground() {
        try {
            startForeground(NOTIFICATION_ID, buildNotification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE);
        } catch (RuntimeException e) {
            Log.e(TAG, "startForeground was refused");
        }
    }

    private void refreshNotification() {
        main.post(new Runnable() {
            @Override public void run() {
                if (stopped) return;
                try {
                    getSystemService(NotificationManager.class).notify(NOTIFICATION_ID, buildNotification());
                } catch (RuntimeException e) {
                    Log.w(TAG, "could not update the notification");
                }
            }
        });
    }

    private Notification buildNotification() {
        HubSpec h = hub != null ? hub : store.load();
        String laptop = h != null ? h.laptopLabel : "an unknown laptop";
        LinkClient c = client;
        String body = NotificationText.body(session.state(), c != null && c.connected(), laptop);
        PendingIntent stop = PendingIntent.getService(this, 1,
                new Intent(this, LinkService.class).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        PendingIntent repost = PendingIntent.getService(this, 2,
                new Intent(this, LinkService.class).setAction(ACTION_REPOST), PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification.Builder b = new Notification.Builder(this, CHANNEL)
                .setContentTitle(NotificationText.TITLE)
                .setContentText(body)
                .setStyle(new Notification.BigTextStyle().bigText(body))
                .setSmallIcon(Icon.createWithResource(this, android.R.drawable.ic_menu_info_details))
                .setOngoing(true)
                .setShowWhen(false)
                .setCategory(Notification.CATEGORY_SERVICE)
                .setDeleteIntent(repost)
                .addAction(new Notification.Action.Builder(
                        Icon.createWithResource(this, android.R.drawable.ic_menu_close_clear_cancel), "Stop", stop).build());
        if (Build.VERSION.SDK_INT >= 31) b.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        return b.build();
    }

    /** True only if the user can actually see our notification: app notifications on and the channel not silenced to none. */
    private boolean notificationVisible() {
        NotificationManager nm = getSystemService(NotificationManager.class);
        if (!nm.areNotificationsEnabled()) return false;
        NotificationChannel ch = nm.getNotificationChannel(CHANNEL);
        return ch == null || ch.getImportance() != NotificationManager.IMPORTANCE_NONE;
    }

    // ------------------------------------------------------------------ connection (LinkClient does the rest)

    private void watchNetwork() {
        ConnectivityManager cm = getSystemService(ConnectivityManager.class);
        netCallback = new ConnectivityManager.NetworkCallback() {
            @Override public void onAvailable(Network n) {
                LinkClient c = client;
                if (c != null) c.onNetworkAvailable();
            }
        };
        cm.registerDefaultNetworkCallback(netCallback, main);
    }
}
