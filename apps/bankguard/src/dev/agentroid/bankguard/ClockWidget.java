package dev.agentroid.bankguard;

import android.app.PendingIntent;
import android.appwidget.AppWidgetManager;
import android.appwidget.AppWidgetProvider;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.provider.AlarmClock;
import android.provider.Settings;
import android.text.format.DateFormat;
import android.widget.RemoteViews;

import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Date;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/**
 * The dot clocks. A launcher will not load this app's font inside a widget (see ClockRender), so each clock is drawn
 * here as bitmaps of DotoClock text.
 *
 * What redraws them, and what it costs:
 * - GuardService, the app's foreground service, already running for the bank guard, listens for ACTION_TIME_TICK.
 *   That is the system's own minute broadcast, sent on a non-wakeup alarm the system keeps anyway. The service
 *   redraws only while the screen is on, and once at screen-on. This app sets no alarm and never wakes the phone for
 *   the time: `dumpsys alarm` shows nothing for it.
 * - A time, timezone or locale change redraws through this receiver; placing a widget or a launcher restart through
 *   onUpdate. updatePeriodMillis is 0.
 */
public abstract class ClockWidget extends AppWidgetProvider {
    static final int INK = 0xFFF4F4F5, DIM = 0xFF9A9AA0, RED = 0xFFCD3733;
    static final Class<?>[] ALL = {Greeting.class, Row.class, Large.class};

    abstract int layout();

    abstract void fill(Context c, RemoteViews rv, Date now, boolean h24);

    @Override
    public void onUpdate(Context c, AppWidgetManager m, int[] ids) {
        forgetClockTarget();
        for (int id : ids) render(c, m, id);
        // The service is what ticks the clock. It normally runs from boot; make sure of it (Android may refuse a
        // foreground start from here, and then the boot receiver or opening the app starts it).
        try { c.startForegroundService(new Intent(c, GuardService.class)); } catch (Throwable ignored) { }
    }

    @Override
    public void onReceive(Context c, Intent i) {
        String a = i.getAction();
        if (Intent.ACTION_TIME_CHANGED.equals(a) || Intent.ACTION_TIMEZONE_CHANGED.equals(a)
                || Intent.ACTION_LOCALE_CHANGED.equals(a)) {
            forgetClockTarget();
            renderAll(c);
            return;
        }
        super.onReceive(c, i);
    }

    void render(Context c, AppWidgetManager m, int id) {
        RemoteViews rv = new RemoteViews(c.getPackageName(), layout());
        fill(c, rv, new Date(), DateFormat.is24HourFormat(c));
        // The whole widget opens the clock app, as the owner's original does (see ClockLogic for why not SHOW_ALARMS).
        rv.setOnClickPendingIntent(R.id.w_root, PendingIntent.getActivity(c, 100 + id % 1000, clockIntent(c),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
        m.updateAppWidget(id, rv);
    }

    /** The clock app's launch intent, worked out once and kept: a minute's redraw asks the package manager nothing. */
    private static volatile Intent clockTarget;

    /** Forget the clock app (called at screen-on, on placement and on a time change), so an app installed or removed
     *  since is picked up at the next redraw. */
    static void forgetClockTarget() { clockTarget = null; }

    static Intent clockIntent(Context c) {
        Intent t = clockTarget;
        if (t == null) clockTarget = t = findClockIntent(c);
        return new Intent(t);
    }

    private static Intent findClockIntent(Context c) {
        PackageManager pm = c.getPackageManager();
        String alarmApp = null;
        try {
            ResolveInfo ri = pm.resolveActivity(new Intent(AlarmClock.ACTION_SHOW_ALARMS), PackageManager.MATCH_DEFAULT_ONLY);
            if (ri != null && ri.activityInfo != null) alarmApp = ri.activityInfo.packageName;
        } catch (Throwable ignored) { }
        Set<String> launchable = new HashSet<String>();
        List<String> probe = new ArrayList<String>(Arrays.asList(ClockLogic.KNOWN));
        if (alarmApp != null) probe.add(alarmApp);
        for (String p : probe) {
            try { if (pm.getLaunchIntentForPackage(p) != null) launchable.add(p); } catch (Throwable ignored) { }
        }
        String pkg = ClockLogic.choose(alarmApp, launchable);
        Intent i = pkg == null ? null : pm.getLaunchIntentForPackage(pkg);
        if (i == null) i = new Intent(Settings.ACTION_DATE_SETTINGS); // NewThing's own fallback
        // As a launcher icon starts an app: back to the app's task as it was left, not a fresh copy on top.
        return i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
    }

    /** Redraw every placed clock. Cheap when none is placed: one lookup per provider. */
    static void renderAll(Context c) {
        AppWidgetManager m = AppWidgetManager.getInstance(c);
        for (Class<?> k : ALL) {
            try {
                int[] ids = m.getAppWidgetIds(new ComponentName(c, k));
                if (ids == null || ids.length == 0) continue;
                ClockWidget w = (ClockWidget) k.getDeclaredConstructor().newInstance();
                for (int id : ids) w.render(c, m, id);
            } catch (Throwable ignored) { }
        }
    }

    static String fmt(String pattern, Date d) {
        return new SimpleDateFormat(pattern, Locale.getDefault()).format(d);
    }

    static String time(Date d, boolean h24) { return fmt(h24 ? "HH:mm" : "hh:mm", d); }

    static String caps(String s) { return s.toUpperCase(Locale.getDefault()); }

    /** HELLO! / 06:47 / SAT, 14 NOV, on the wallpaper: the owner's own clock. */
    public static final class Greeting extends ClockWidget {
        @Override int layout() { return R.layout.widget_clock_greeting; }

        @Override void fill(Context c, RemoteViews rv, Date d, boolean h24) {
            String t = time(d, h24);
            rv.setImageViewBitmap(R.id.w_greeting,
                    ClockRender.line(c, 22, c.getString(R.string.clock_greeting_text), INK, true));
            // Heavier dots on the time only: in the original, its dots nearly touch (side by side, 14 Nov 2026).
            rv.setImageViewBitmap(R.id.w_time, ClockRender.line(c, 56, new String[] {t}, new int[] {INK}, 0, true,
                    0.02f));
            // "EEE, dd MMM", as the original (TimeDateLargeR): FRI, 06 NOV, not FRI, 6 NOV.
            rv.setImageViewBitmap(R.id.w_date, ClockRender.line(c, 22, caps(fmt("EEE, dd MMM", d)), INK, true));
            rv.setContentDescription(R.id.w_time, t);
            rv.setContentDescription(R.id.w_date, fmt("EEEE d MMMM", d));
        }
    }

    /** [ 06:27 ] ( 14 ) ( SAT ) */
    public static final class Row extends ClockWidget {
        @Override int layout() { return R.layout.widget_clock_row; }

        @Override void fill(Context c, RemoteViews rv, Date d, boolean h24) {
            String t = time(d, h24);
            // Drawn large and scaled down to fit each pill (fitCenter), which keeps the dots sharp.
            rv.setImageViewBitmap(R.id.w_time, ClockRender.line(c, 64, t, INK, false));
            rv.setImageViewBitmap(R.id.w_date, ClockRender.line(c, 64, fmt("d", d), INK, false));
            rv.setImageViewBitmap(R.id.w_day, ClockRender.line(c, 64, caps(fmt("EEE", d)), INK, false));
            rv.setContentDescription(R.id.w_time, t);
            rv.setContentDescription(R.id.w_date, fmt("EEEE d MMMM", d));
        }
    }

    /** A large time over SAT 14 NOV. */
    public static final class Large extends ClockWidget {
        @Override int layout() { return R.layout.widget_clock_large; }

        @Override void fill(Context c, RemoteViews rv, Date d, boolean h24) {
            String t = time(d, h24);
            rv.setImageViewBitmap(R.id.w_time, ClockRender.line(c, 96, t, INK, false));
            rv.setImageViewBitmap(R.id.w_sub, ClockRender.line(c, 26,
                    new String[] {caps(fmt("EEE", d)), fmt("d", d), caps(fmt("MMM", d))},
                    new int[] {DIM, RED, DIM}, 0.6f, false));
            rv.setContentDescription(R.id.w_time, t);
            rv.setContentDescription(R.id.w_sub, fmt("EEEE d MMMM", d));
        }
    }
}
