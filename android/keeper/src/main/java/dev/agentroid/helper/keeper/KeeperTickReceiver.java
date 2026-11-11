package dev.agentroid.helper.keeper;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.TransportInfo;
import android.net.wifi.WifiInfo;
import android.os.Build;
import android.os.SystemClock;

import dev.agentroid.helper.keeper.logic.Keeper;

/**
 * The 15-minute backstop: an INEXACT repeating alarm (no permission needed), so the re-assert also happens when the
 * link service's Handler tick is not running. With the link service alive it nudges the live keeper; otherwise it runs
 * one standalone pass. Not exported: only our own PendingIntent reaches it.
 */
public class KeeperTickReceiver extends BroadcastReceiver {
    static void schedule(Context c) {
        AlarmManager am = c.getSystemService(AlarmManager.class);
        if (am == null) return;
        PendingIntent pi = PendingIntent.getBroadcast(c, 7, new Intent(c, KeeperTickReceiver.class),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        am.setInexactRepeating(AlarmManager.ELAPSED_REALTIME_WAKEUP, SystemClock.elapsedRealtime() + Keeper.TICK_MS,
                AlarmManager.INTERVAL_FIFTEEN_MINUTES, pi);
    }

    @Override
    public void onReceive(Context c, Intent intent) {
        Keeper k = KeeperExtension.live;
        if (k != null) k.pass("alarm");
        else standalone(c).pass("alarm");
    }

    /**
     * A one-off keeper without the link: no reporter (nothing to report to), and the BSSID read synchronously. Android
     * redacts the BSSID in that call, so a standalone pass will normally see it as unreadable and leave wireless
     * debugging alone; it still re-adds the accessibility service.
     */
    static Keeper standalone(Context ctx) {
        final Context c = ctx.getApplicationContext();
        Keeper.Wifi snapshot = new Keeper.Wifi() {
            private NetworkCapabilities caps() {
                ConnectivityManager cm = c.getSystemService(ConnectivityManager.class);
                Network n = cm == null ? null : cm.getActiveNetwork();
                return n == null ? null : cm.getNetworkCapabilities(n);
            }
            public boolean onWifi() {
                NetworkCapabilities nc = caps();
                return nc != null && nc.hasTransport(NetworkCapabilities.TRANSPORT_WIFI);
            }
            public String bssid() {
                NetworkCapabilities nc = caps();
                if (nc == null || Build.VERSION.SDK_INT < 31) return null;
                TransportInfo ti = nc.getTransportInfo();
                return ti instanceof WifiInfo ? ((WifiInfo) ti).getBSSID() : null;
            }
        };
        return new Keeper(new AndroidKeeperParts.HandlerClock(), new AndroidKeeperParts.AndroidSettings(c), snapshot,
                new AndroidKeeperParts.TrustPrefs(c), new Keeper.Reporter() {
                    public void untrustedBssid(String bssid, String reason) { }
                }, AndroidKeeperParts.a11yComponent(c));
    }
}
