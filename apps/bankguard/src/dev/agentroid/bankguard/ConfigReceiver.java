package dev.agentroid.bankguard;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

import java.util.Set;

/**
 * Set the bank list, a pause, or the home network from a computer:
 * <pre>
 *   adb shell am broadcast -n dev.agentroid.bankguard/.ConfigReceiver -a dev.agentroid.bankguard.SET --es banks "a.b.c,d.e.f"
 *   adb shell am broadcast -n dev.agentroid.bankguard/.ConfigReceiver -a dev.agentroid.bankguard.SET --es pause_min 0
 *   adb shell am broadcast -n dev.agentroid.bankguard/.ConfigReceiver -a dev.agentroid.bankguard.SET \
 *       --es home_ssid "'My home'" --es home_bssids "aa:bb:cc:dd:ee:f0,aa:bb:cc:dd:*"
 *   adb shell am broadcast -n dev.agentroid.bankguard/.ConfigReceiver -a dev.agentroid.bankguard.SET --es home_ssid "''"
 * </pre>
 * Exported, but the manifest lets only a sender holding android.permission.DUMP reach it: the adb shell has it,
 * ordinary apps cannot. Every value is validated. Only the home network can lead to Wireless debugging being switched
 * on (on that network, with no bank open), and only a computer that already has adb access can set it.
 */
public final class ConfigReceiver extends BroadcastReceiver {
    static final String ACTION_SET = "dev.agentroid.bankguard.SET";

    @Override
    public void onReceive(Context c, Intent i) {
        if (!ACTION_SET.equals(i.getAction())) return;
        GuardCore core = GuardCore.get(c);
        StringBuilder out = new StringBuilder();
        String banks = i.getStringExtra("banks");
        if (banks != null) {
            Set<String> list = GuardLogic.parseList(banks);
            core.setBanks(list);
            out.append(" banks=").append(list.size());
        }
        String pause = i.getStringExtra("pause_min");
        if (pause != null) {
            try {
                long min = Math.max(0, Math.min(12 * 60, Long.parseLong(pause.trim())));
                core.pauseFor(min * 60_000L);
                out.append(" pause_min=").append(min);
            } catch (NumberFormatException e) {
                out.append(" pause_min_not_a_number");
            }
        }
        String ssid = i.getStringExtra("home_ssid");
        if (ssid != null) {
            if (ssid.isEmpty()) {
                core.setHome("", new java.util.LinkedHashSet<String>());
                out.append(" home_mode=off");
            } else if (!validSsid(ssid)) {
                out.append(" home_ssid_invalid");
            } else {
                Set<String> aps = GuardLogic.parseBssids(i.getStringExtra("home_bssids"));
                if (aps.isEmpty()) {
                    out.append(" home_bssids_needed"); // a name alone is easy to copy: never trusted on its own
                } else {
                    core.setHome(ssid, aps);
                    out.append(" home_mode=on access_points=").append(aps.size());
                }
            }
        }
        setResultData(out.toString().trim());
    }

    /** A Wi-Fi network name: 1 to 32 bytes of UTF-8, no control characters. */
    static boolean validSsid(String s) {
        if (s.isEmpty() || s.getBytes(java.nio.charset.StandardCharsets.UTF_8).length > 32) return false;
        for (int k = 0; k < s.length(); k++) if (Character.isISOControl(s.charAt(k))) return false;
        return true;
    }
}
