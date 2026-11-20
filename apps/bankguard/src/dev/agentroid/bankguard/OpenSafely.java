package dev.agentroid.bankguard;

import android.app.Activity;
import android.content.Intent;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.os.Bundle;

/**
 * The home-screen shortcut "Open UOB TMRW safely": debugging goes off FIRST, then the bank opens, and the guard holds
 * for {@link GuardLogic#LAUNCH_MS} while the bank starts (a bank's start-up checks can take seconds before its first
 * screen shows). Not exported: only this app's own pinned shortcuts and widgets (started on this app's behalf) reach
 * it, and the package must be on the bank list.
 */
public final class OpenSafely extends Activity {
    static final String EXTRA_PKG = "pkg";

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        String pkg = getIntent().getStringExtra(EXTRA_PKG);
        GuardCore core = GuardCore.get(this);
        Intent launch = pkg == null ? null : getPackageManager().getLaunchIntentForPackage(pkg);
        if (pkg != null && GuardLogic.isBank(core.banks(), pkg) && launch != null) {
            long wifi = wifiHandle();
            core.holdNow(pkg, wifi != 0, wifi);
            startForegroundService(new Intent(this, GuardService.class));
            startActivity(launch);
        } else if (launch != null) {
            startActivity(launch); // not a bank (any more): just open it
        }
        finish();
    }

    /** The Wi-Fi network's handle, internet or not (an adb LAN may have none), or 0. */
    @SuppressWarnings("deprecation")
    private long wifiHandle() {
        try {
            ConnectivityManager cm = getSystemService(ConnectivityManager.class);
            for (Network n : cm.getAllNetworks()) {
                NetworkCapabilities caps = cm.getNetworkCapabilities(n);
                if (caps != null && caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) return n.getNetworkHandle();
            }
        } catch (Throwable ignored) { }
        return 0;
    }
}
