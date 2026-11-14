package dev.agentroid.bankguard;

import android.app.Activity;
import android.content.Intent;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.os.Bundle;

/**
 * The home-screen shortcut "Open UOB TMRW safely": debugging goes off FIRST, then the bank opens. A bank that checks
 * in its first screen's onCreate never sees debugging on, which the poll alone cannot promise.
 * Not exported: only this app's own pinned shortcuts (started by the launcher on this app's behalf) reach it, and
 * the package must be on the bank list.
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
            ConnectivityManager cm = getSystemService(ConnectivityManager.class);
            Network n = cm.getActiveNetwork();
            NetworkCapabilities caps = n == null ? null : cm.getNetworkCapabilities(n);
            boolean wifi = caps != null && caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI);
            core.holdNow(pkg, wifi, wifi ? n.getNetworkHandle() : 0);
            startForegroundService(new Intent(this, GuardService.class));
            startActivity(launch);
        }
        finish();
    }
}
