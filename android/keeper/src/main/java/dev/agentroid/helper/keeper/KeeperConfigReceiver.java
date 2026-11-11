package dev.agentroid.helper.keeper;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

import java.util.Arrays;
import java.util.List;

import dev.agentroid.helper.keeper.logic.Bssids;
import dev.agentroid.helper.keeper.logic.Keeper;

/**
 * The hub sets the trusted BSSID list over adb (the link method keeper.trust_bssids does the same over the tailnet,
 * which is the path that works while wireless debugging is off on an untrusted network):
 *   adb shell am broadcast -n dev.agentroid.helper/dev.agentroid.helper.keeper.KeeperConfigReceiver \
 *       -a dev.agentroid.helper.KEEPER_TRUST --es bssids aa:bb:cc:dd:ee:01,aa:bb:cc:dd:ee:02
 * Protected by android.permission.DUMP (see the manifest). The list replaces the old one.
 */
public class KeeperConfigReceiver extends BroadcastReceiver {
    public static final String ACTION_TRUST = "dev.agentroid.helper.KEEPER_TRUST";

    @Override
    public void onReceive(Context c, Intent intent) {
        if (!ACTION_TRUST.equals(intent.getAction())) return;
        String csv = intent.getStringExtra("bssids");
        List<String> list = csv == null ? null : Arrays.asList(csv.split(","));
        try {
            Keeper live = KeeperExtension.live;
            if (live != null) live.setTrusted(list);
            else new AndroidKeeperParts.TrustPrefs(c).set(Bssids.parseTrusted(list));
            if (isOrderedBroadcast()) setResult(Activity.RESULT_OK, "trusted " + list.size(), null);
        } catch (Bssids.Invalid e) {
            if (isOrderedBroadcast()) setResult(Activity.RESULT_CANCELED, "refused: " + e.getMessage(), null);
        }
    }
}
