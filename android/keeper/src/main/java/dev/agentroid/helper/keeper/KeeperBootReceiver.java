package dev.agentroid.helper.keeper;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/**
 * On boot: one keeper pass (re-add the accessibility service; wireless debugging only on a trusted, readable BSSID) and
 * arm the 15-minute alarm. The link's own BootReceiver starts LinkService, whose KeeperExtension does a full pass with
 * reporting. directBootAware, and takes LOCKED_BOOT_COMPLETED too, so it also runs before the first unlock.
 */
public class KeeperBootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context c, Intent intent) {
        String a = intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(a) && !Intent.ACTION_LOCKED_BOOT_COMPLETED.equals(a)) return;
        KeeperTickReceiver.standalone(c).pass("boot");
        KeeperTickReceiver.schedule(c);
    }
}
