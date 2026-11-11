package dev.agentroid.keeper;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/**
 * Starts the keeper FGS on BOOT_COMPLETED and MY_PACKAGE_REPLACED (after an adb install -r).
 * The hub URL is already in SharedPreferences from setup.
 */
public class BootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context c, Intent intent) {
        L.init(c);
        String action = intent == null ? null : intent.getAction();
        L.i("BootReceiver action=" + action);
        Alarms.schedule(c);
        KeeperService.start(c, null, "boot:" + action);
    }
}
