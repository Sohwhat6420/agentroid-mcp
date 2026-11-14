package dev.agentroid.bankguard;

import android.app.PendingIntent;
import android.appwidget.AppWidgetManager;
import android.appwidget.AppWidgetProvider;
import android.content.Context;
import android.content.Intent;
import android.provider.AlarmClock;
import android.widget.RemoteViews;

/**
 * The dot clocks. Each is a layout of TextClocks, which tick inside the home screen's own process: this app sets no
 * alarm, has updatePeriodMillis 0 and is never woken for the time. onUpdate runs only when the widget is placed, the
 * launcher restarts, or the app is updated; it only attaches the tap (open the phone's clock app).
 */
public abstract class ClockWidget extends AppWidgetProvider {
    abstract int layout();

    @Override
    public void onUpdate(Context c, AppWidgetManager m, int[] ids) {
        for (int id : ids) {
            RemoteViews rv = new RemoteViews(c.getPackageName(), layout());
            Intent clock = new Intent(AlarmClock.ACTION_SHOW_ALARMS);
            if (clock.resolveActivity(c.getPackageManager()) != null) {
                rv.setOnClickPendingIntent(R.id.w_root, PendingIntent.getActivity(c, 100 + id % 1000, clock,
                        PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
            }
            m.updateAppWidget(id, rv);
        }
    }

    /** [ 06:27 ] ( 14 ) ( SAT ) */
    public static final class Row extends ClockWidget {
        @Override int layout() { return R.layout.widget_clock_row; }
    }

    /** A large time over SAT 14 NOV. */
    public static final class Large extends ClockWidget {
        @Override int layout() { return R.layout.widget_clock_large; }
    }
}
