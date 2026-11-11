package dev.agentroid.keeper;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/**
 * The 15-minute exact-alarm backstop. It books the NEXT alarm first (v0.2): if the foreground-service start below is
 * refused, the chain still carries on, and the service is retried 15 minutes later.
 */
public class AlarmReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context c, Intent intent) {
        L.init(c);
        L.i("AlarmReceiver fired");
        Alarms.schedule(c);
        Intent i = new Intent(c, KeeperService.class).setAction(KeeperService.ACTION_ALARM).putExtra("src", "alarm");
        try { c.startForegroundService(i); } catch (Throwable t) { L.e("alarm startForegroundService", t); }
    }
}
