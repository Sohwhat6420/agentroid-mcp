package dev.agentroid.keeper;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.os.SystemClock;

/**
 * The 15-minute backstop alarm. Scheduled from three independent places (the service, the alarm receiver itself and
 * the boot receiver), so a refused foreground-service start never breaks the chain: the receiver books the next
 * alarm before it tries to start anything.
 */
final class Alarms {
    static final long ALARM_MS = 15 * 60_000L;

    private Alarms() { }

    static void schedule(Context c) {
        AlarmManager am = c.getSystemService(AlarmManager.class);
        Intent i = new Intent(c, AlarmReceiver.class).setAction(KeeperService.ACTION_ALARM);
        PendingIntent pi = PendingIntent.getBroadcast(c, 0, i,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        long when = SystemClock.elapsedRealtime() + ALARM_MS; // elapsed clock: a wall-clock jump cannot delay it
        try {
            if (am.canScheduleExactAlarms()) {
                am.setExactAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, when, pi);
                L.i("scheduled exact alarm +15m");
            } else {
                am.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, when, pi);
                L.i("SCHEDULE_EXACT_ALARM not held; used inexact alarm (grant appop at setup); a background service start from it may be refused");
            }
        } catch (Throwable t) { L.e("scheduleAlarm", t); }
    }
}
