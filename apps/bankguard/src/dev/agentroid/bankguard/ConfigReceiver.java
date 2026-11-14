package dev.agentroid.bankguard;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

import java.util.Set;

/**
 * Set the bank list from a computer:
 * <pre>
 *   adb shell am broadcast -n dev.agentroid.bankguard/.ConfigReceiver -a dev.agentroid.bankguard.SET --es banks "a.b.c,d.e.f"
 *   adb shell am broadcast -n dev.agentroid.bankguard/.ConfigReceiver -a dev.agentroid.bankguard.SET --es pause_min 0
 * </pre>
 * Exported, but the manifest lets only a sender holding android.permission.DUMP reach it: the adb shell has it,
 * ordinary apps cannot. It carries package names only, each validated; nothing here can switch debugging on.
 */
public final class ConfigReceiver extends BroadcastReceiver {
    static final String ACTION_SET = "dev.agentroid.bankguard.SET";

    @Override
    public void onReceive(Context c, Intent i) {
        if (!ACTION_SET.equals(i.getAction())) return;
        GuardCore core = GuardCore.get(c);
        String banks = i.getStringExtra("banks");
        if (banks != null) {
            Set<String> list = GuardLogic.parseList(banks);
            core.setBanks(list);
            setResultData("banks=" + list.size());
        }
        String pause = i.getStringExtra("pause_min");
        if (pause != null) {
            try {
                long min = Math.max(0, Math.min(12 * 60, Long.parseLong(pause.trim())));
                core.pauseFor(min * 60_000L);
                setResultData("pause_min=" + min);
            } catch (NumberFormatException e) {
                setResultData("pause_min not a number");
            }
        }
    }
}
