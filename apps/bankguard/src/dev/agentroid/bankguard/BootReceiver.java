package dev.agentroid.bankguard;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** Starts the guard after a boot (before the first unlock too) or an update. A boot also releases any old hold. */
public final class BootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context c, Intent i) {
        String a = i.getAction();
        boolean boot = Intent.ACTION_LOCKED_BOOT_COMPLETED.equals(a) || Intent.ACTION_BOOT_COMPLETED.equals(a);
        if (!boot && !Intent.ACTION_MY_PACKAGE_REPLACED.equals(a)) return;
        c.startForegroundService(new Intent(c, GuardService.class).putExtra(GuardService.EXTRA_BOOT, boot));
    }
}
