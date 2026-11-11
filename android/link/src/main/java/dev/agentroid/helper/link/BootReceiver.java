package dev.agentroid.helper.link;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/**
 * BOOT_COMPLETED: bring the link back, but only if the owner had it on (a Stop persists across reboots) and a valid
 * hub is stored. Shape follows the S4 spike's BootReceiver (true-phone-link companion-phase0, lines 12-26).
 * 2.12: the receiver is directBootAware and also takes LOCKED_BOOT_COMPLETED, so on a phone with a lock the link can
 * come back before the first unlock (HubStore is in device-protected storage). Both may arrive; a second start is a
 * no-op because LinkClient.start with the same address does nothing.
 */
public class BootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context c, Intent intent) {
        String a = intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(a) && !Intent.ACTION_LOCKED_BOOT_COMPLETED.equals(a)) return;
        HubStore store = new HubStore(c);
        if (store.enabled() && store.load() != null) LinkService.start(c, "boot");
    }
}
