package dev.agentroid.helper.keeper;

import android.content.ContentResolver;
import android.content.Context;
import android.content.SharedPreferences;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.TransportInfo;
import android.net.wifi.WifiInfo;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.Settings;

import java.util.Collections;
import java.util.HashSet;
import java.util.Set;

import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.keeper.logic.Keeper;

/** The Android implementations of the keeper's seams. Each is small on purpose: the logic is in keeper.logic.Keeper. */
final class AndroidKeeperParts {
    private AndroidKeeperParts() { }

    static final String PREFS = "keeper";

    /** Our accessibility service, by name (:keeper does not depend on :a11y). */
    static String a11yComponent(Context c) { return c.getPackageName() + "/dev.agentroid.helper.a11y.HelperAccessibilityService"; }

    /** Settings.Global / Settings.Secure. Writes need WRITE_SECURE_SETTINGS (granted with `pm grant`), else SecurityException. */
    static final class AndroidSettings implements Keeper.Settings {
        private final ContentResolver cr;
        AndroidSettings(Context c) { cr = c.getContentResolver(); }
        @Override public String getGlobal(String k) { return Settings.Global.getString(cr, k); }
        @Override public void putGlobal(String k, String v) { Settings.Global.putString(cr, k, v); }
        @Override public String getSecure(String k) { return Settings.Secure.getString(cr, k); }
        @Override public void putSecure(String k, String v) { Settings.Secure.putString(cr, k, v); }
    }

    /**
     * Current Wi-Fi and its BSSID, from a ConnectivityManager callback (ACCESS_NETWORK_STATE, one of the five). On API 31+
     * the callback asks for location info, but Android still redacts the BSSID to 02:00:00:00:00:00 unless the app
     * holds ACCESS_FINE_LOCATION and location is on. :keeper may not ask for that permission (exactly five), so on a
     * phone where nothing else grants it the BSSID is unreadable and the keeper reports bssid_unreadable and never
     * re-asserts wireless debugging. That is the safe failure; see LINK-REPORT.md, "only the phone can measure".
     */
    static final class AndroidWifi implements Keeper.Wifi {
        private volatile boolean on;
        private volatile String bssid;
        private final ConnectivityManager cm;
        private ConnectivityManager.NetworkCallback cb;

        AndroidWifi(Context c) { cm = c.getSystemService(ConnectivityManager.class); }

        void watch(final Runnable onChange) {
            NetworkRequest req = new NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build();
            cb = Build.VERSION.SDK_INT >= 31 ? new ConnectivityManager.NetworkCallback(ConnectivityManager.NetworkCallback.FLAG_INCLUDE_LOCATION_INFO) {
                @Override public void onCapabilitiesChanged(Network n, NetworkCapabilities caps) { update(caps, onChange); }
                @Override public void onLost(Network n) { lost(onChange); }
            } : new ConnectivityManager.NetworkCallback() {
                @Override public void onCapabilitiesChanged(Network n, NetworkCapabilities caps) { update(caps, onChange); }
                @Override public void onLost(Network n) { lost(onChange); }
            };
            cm.registerNetworkCallback(req, cb, new Handler(Looper.getMainLooper()));
        }

        private void update(NetworkCapabilities caps, Runnable onChange) {
            String b = null;
            if (Build.VERSION.SDK_INT >= 31) {
                TransportInfo ti = caps.getTransportInfo();
                if (ti instanceof WifiInfo) b = ((WifiInfo) ti).getBSSID();
            }
            boolean changed = !on || (b == null ? bssid != null : !b.equals(bssid));
            on = true;
            bssid = b;
            if (changed) onChange.run();
        }

        private void lost(Runnable onChange) {
            on = false;
            bssid = null;
            onChange.run();
        }

        void stop() {
            if (cb != null) {
                try {
                    cm.unregisterNetworkCallback(cb);
                } catch (RuntimeException ignored) {
                    // already gone
                }
            }
        }

        @Override public boolean onWifi() { return on; }
        @Override public String bssid() { return bssid; }
    }

    /** The hub-set trusted list, in device-protected MODE_PRIVATE prefs (BSSIDs are not secrets). */
    static final class TrustPrefs implements Keeper.TrustList {
        private final SharedPreferences p;
        TrustPrefs(Context c) {
            Context de = c.createDeviceProtectedStorageContext();
            p = de.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
        }
        @Override public Set<String> get() { return Collections.unmodifiableSet(new HashSet<String>(p.getStringSet("trusted_bssids", new HashSet<String>()))); }
        @Override public void set(Set<String> b) { p.edit().putStringSet("trusted_bssids", new HashSet<String>(b)).commit(); }
    }

    /** Main-looper Clock for the keeper's 15-minute tick while the link service runs (AlarmManager covers the rest). */
    static final class HandlerClock implements Clock {
        private final Handler h = new Handler(Looper.getMainLooper());
        @Override public long nowMs() { return SystemClock.elapsedRealtime(); }
        @Override public long wallMs() { return System.currentTimeMillis(); }
        @Override public Task schedule(long delayMs, final Runnable r) {
            h.postDelayed(r, Math.max(0, delayMs));
            return new Task() { public void cancel() { h.removeCallbacks(r); } };
        }
    }
}
