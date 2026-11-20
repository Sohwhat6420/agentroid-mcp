package dev.agentroid.bankguard;

import android.Manifest;
import android.app.Activity;
import android.app.PendingIntent;
import android.appwidget.AppWidgetManager;
import android.appwidget.AppWidgetProvider;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothClass;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothManager;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.RectF;
import android.os.BatteryManager;
import android.os.Bundle;
import android.provider.Settings;
import android.widget.RemoteViews;

import java.util.Calendar;
import java.util.Set;

/**
 * The year, battery and earbuds widgets: efficient copies of the owner's NewThing Widgets Pro originals
 * (YearsDaysCountR, BatteryWidgetEmojiR, EarBudsBattery2R). The day greeting is ClockWidget.GoodDay.
 *
 * What redraws them, and what it costs. None sets an alarm and none has a timer (updatePeriodMillis 0); `dumpsys
 * alarm` shows nothing for this app. GuardService, already running for the bank guard, owns the listening:
 * - Year: on the system's minute tick while the screen is on it compares the day (one Calendar read) and redraws only
 *   when the day has changed; also on a date, time or timezone change and at screen-on. The original redraws every
 *   minute and wakes the phone every 30 minutes.
 * - Battery: the system's BATTERY_CHANGED, registered only while a battery widget is placed. It redraws only when the
 *   percent or the plug changes, and while the screen is off it only notes the change and redraws at screen-on. The
 *   original also wakes the phone every 15 minutes.
 * - Earbuds: Bluetooth's own broadcasts (on/off, a device connecting or leaving, a headset's battery), registered
 *   only while an earbuds widget is placed. No polling.
 * A widget placed while the service is not running draws once and is kept current from the next start (boot, or
 * opening the app).
 */
public final class InfoWidgets {
    private InfoWidgets() { }

    static final int CARD = 0xFF1D1D1D, INK = 0xFFF1F1F1, RED = 0xFFBC312B, GREY = 0xFF3B393E, GREY_TEXT = 0xFF6C696D;

    static float dp(Context c) { return c.getResources().getDisplayMetrics().density; }

    static int[] ids(Context c, Class<?> k) {
        try {
            int[] ids = AppWidgetManager.getInstance(c).getAppWidgetIds(new ComponentName(c, k));
            return ids == null ? new int[0] : ids;
        } catch (Throwable t) { return new int[0]; }
    }

    static boolean placed(Context c, Class<?> k) { return ids(c, k).length > 0; }

    static PendingIntent activity(Context c, int code, Intent i) {
        return PendingIntent.getActivity(c, code, i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    /** Base for the three: draws on placement and tells the service to (re)arm its listeners. */
    abstract static class Base extends AppWidgetProvider {
        abstract void render(Context c, AppWidgetManager m, int id);

        void renderAll(Context c) {
            AppWidgetManager m = AppWidgetManager.getInstance(c);
            for (int id : ids(c, getClass())) {
                try { render(c, m, id); } catch (Throwable ignored) { }
            }
        }

        @Override public void onUpdate(Context c, AppWidgetManager m, int[] ids) {
            for (int id : ids) {
                try { render(c, m, id); } catch (Throwable ignored) { }
            }
            GuardService.widgetsChanged(c);
            try { c.startForegroundService(new Intent(c, GuardService.class)); } catch (Throwable ignored) { }
        }

        @Override public void onAppWidgetOptionsChanged(Context c, AppWidgetManager m, int id, Bundle o) {
            try { render(c, m, id); } catch (Throwable ignored) { }
        }

        @Override public void onEnabled(Context c) { GuardService.widgetsChanged(c); }

        @Override public void onDisabled(Context c) { GuardService.widgetsChanged(c); }
    }

    // ------------------------------------------------------------------------------------------------------------
    /** YEAR / 41D REMAINING over a dot a day: past white, today red, the rest grey. A tap opens the calendar. */
    public static final class Year extends Base {
        private static volatile int drawnDay = -1;

        static int dayKey(Calendar k) { return k.get(Calendar.YEAR) * 1000 + k.get(Calendar.DAY_OF_YEAR); }

        /** On the minute tick: redraw only when the day has changed since the last draw (or force). */
        static void tick(Context c, boolean force) {
            if (!force && drawnDay == dayKey(Calendar.getInstance())) return;
            if (!placed(c, Year.class)) { drawnDay = dayKey(Calendar.getInstance()); return; }
            new Year().renderAll(c);
        }

        @Override void render(Context c, AppWidgetManager m, int id) {
            Calendar k = Calendar.getInstance();
            int day = k.get(Calendar.DAY_OF_YEAR), days = WidgetLogic.daysIn(k.get(Calendar.YEAR));
            Bundle o = m.getAppWidgetOptions(id);
            int wdp = o == null ? 0 : o.getInt(AppWidgetManager.OPTION_APPWIDGET_MIN_WIDTH);
            int hdp = o == null ? 0 : o.getInt(AppWidgetManager.OPTION_APPWIDGET_MAX_HEIGHT);
            if (wdp < 200) wdp = 401; // the S24's 4x2 cell
            if (hdp < 100) hdp = 212;
            RemoteViews rv = new RemoteViews(c.getPackageName(), R.layout.widget_year);
            // Caps 15.6 dp in the original: DotoClock caps are 0.69 em, so 22.6 sp.
            rv.setImageViewBitmap(R.id.w_year, ClockRender.line(c, 22.6f, "YEAR", RED, false));
            int left = WidgetLogic.remaining(day, days);
            rv.setImageViewBitmap(R.id.w_left, ClockRender.line(c, 22.6f, left + "D", INK, false));
            rv.setImageViewBitmap(R.id.w_grid, grid(c, wdp, hdp, day, days));
            rv.setContentDescription(R.id.w_left, left + " days left in the year");
            rv.setOnClickPendingIntent(R.id.w_root, activity(c, 300 + id % 1000, calendarApp()));
            m.updateAppWidget(id, rv);
            drawnDay = dayKey(k);
        }

        /** The original's tap: the phone's default calendar app. */
        static Intent calendarApp() {
            return new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_APP_CALENDAR);
        }

        /**
         * The dots, in the lower two thirds of the card as the original's GridView: 37 columns, the first dot's centre
         * 17.7 dp from the card's left edge and the last 19.3 dp from its right; rows 11.33 dp apart, the first centre
         * 10.5 dp below the top of the lower two thirds; dots 7 dp across (measured on the original beside this one).
         */
        static Bitmap grid(Context c, int wdp, int hdp, int day, int days) {
            float d = dp(c);
            float w = wdp * d, h = hdp * 2f / 3f * d;
            float pitchX = (wdp - 37f) / (WidgetLogic.YEAR_COLUMNS - 1) * d, pitchY = 11.33f * d, r = 3.5f * d;
            Bitmap b = Bitmap.createBitmap(Math.max(1, (int) w), Math.max(1, (int) h), Bitmap.Config.ARGB_8888);
            Canvas cv = new Canvas(b);
            Paint p = new Paint(Paint.ANTI_ALIAS_FLAG);
            int[] colour = {INK, RED, GREY};
            for (int i = 0; i < days; i++) {
                int row = i / WidgetLogic.YEAR_COLUMNS, col = i % WidgetLogic.YEAR_COLUMNS;
                p.setColor(colour[WidgetLogic.dot(i, day)]);
                cv.drawCircle(17.7f * d + col * pitchX, 10.5f * d + row * pitchY, r, p);
            }
            return b;
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    /** 30% / WEAK, BUT STILL MOVING / a battery with a face, red to its level. A tap opens battery usage. */
    public static final class Battery extends Base {
        static volatile int level = -1;
        static volatile boolean plugged;
        private static volatile boolean dirty;
        private static volatile String drawn;

        /** From the service's BATTERY_CHANGED receiver: note it; redraw only on a visible change, screen on. */
        static void changed(Context c, Intent i, boolean interactive) {
            int pct = WidgetLogic.percent(i.getIntExtra(BatteryManager.EXTRA_LEVEL, -1),
                    i.getIntExtra(BatteryManager.EXTRA_SCALE, -1));
            boolean plug = i.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0;
            if (pct < 0) return;
            level = pct;
            plugged = plug;
            String now = pct + "/" + plug;
            if (now.equals(drawn)) return;
            if (interactive) new Battery().renderAll(c);
            else dirty = true;
        }

        static void screenOn(Context c) {
            if (dirty) { dirty = false; new Battery().renderAll(c); }
        }

        @Override void render(Context c, AppWidgetManager m, int id) {
            if (level < 0) {
                Intent s = c.registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
                if (s != null) {
                    level = WidgetLogic.percent(s.getIntExtra(BatteryManager.EXTRA_LEVEL, -1),
                            s.getIntExtra(BatteryManager.EXTRA_SCALE, -1));
                    plugged = s.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0;
                }
            }
            int pct = Math.max(0, level);
            RemoteViews rv = new RemoteViews(c.getPackageName(), R.layout.widget_battery);
            // Caps 20.5 dp, as the original drawn beside it: 30 sp of DotoClock.
            rv.setImageViewBitmap(R.id.w_pct, ClockRender.line(c, 30f, pct + "%", INK, false));
            rv.setTextViewText(R.id.w_phrase, WidgetLogic.phrase(pct, plugged));
            rv.setImageViewBitmap(R.id.w_face, face(c, WidgetLogic.face(pct)));
            rv.setContentDescription(R.id.w_pct, "Battery " + pct + " percent" + (plugged ? ", charging" : ""));
            rv.setOnClickPendingIntent(R.id.w_root, activity(c, 400 + id % 1000,
                    new Intent(Intent.ACTION_POWER_USAGE_SUMMARY)));
            m.updateAppWidget(id, rv);
            drawn = pct + "/" + plugged;
        }

        /**
         * The battery character, drawn here (not the original's art): a grey body 74 dp wide with a cap, red up to its
         * band (as the original: four steps, not the exact percent), two eyes and a mouth that changes with the band.
         * 74 x 108 dp (drawn on an 81 x 118 grid scaled by 0.917, to the original's size); its bottom sits on the card's
         * bottom edge.
         */
        static Bitmap face(Context c, int band) {
            float d = dp(c) * 0.917f;
            int w = Math.round(81 * d), h = Math.round(117.8f * d);
            Bitmap b = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888);
            Canvas cv = new Canvas(b);
            Paint p = new Paint(Paint.ANTI_ALIAS_FLAG);
            Path body = new Path();
            body.addRoundRect(new RectF(22 * d, 0, 59 * d, 14 * d), 5 * d, 5 * d, Path.Direction.CW);
            body.addRoundRect(new RectF(0, 10 * d, w, h + 20 * d), 9 * d, 9 * d, Path.Direction.CW);
            p.setColor(GREY);
            cv.drawPath(body, p);
            // The red level. Tops measured on the original's four faces (dp below the cap's top).
            float top = band == WidgetLogic.FACE_FULL ? 0 : band == WidgetLogic.FACE_MOST ? 45.6f
                    : band == WidgetLogic.FACE_HALF ? 68f : 93.6f;
            cv.save();
            cv.clipPath(body);
            p.setColor(RED);
            if (band == WidgetLogic.FACE_FULL) cv.drawPath(body, p);
            else cv.drawRoundRect(new RectF(0, top * d, w, h + 20 * d), 9 * d, 9 * d, p);
            cv.restore();
            // Eyes: dark, with a small light glint (red when the body behind is red).
            int behind = band == WidgetLogic.FACE_FULL ? RED : GREY;
            for (float ex : new float[] {25f, 56f}) {
                p.setColor(CARD);
                cv.drawCircle(ex * d, 36 * d, 5.8f * d, p);
                p.setColor(behind);
                cv.drawCircle((ex + 1.6f) * d, 34.4f * d, 1.4f * d, p);
            }
            // Mouth.
            p.setColor(CARD);
            float cx = 40.5f * d;
            if (band == WidgetLogic.FACE_FULL) {
                cv.drawArc(new RectF(cx - 12 * d, 43 * d, cx + 12 * d, 67 * d), 0, 180, true, p); // open grin
            } else if (band == WidgetLogic.FACE_MOST) {
                p.setStyle(Paint.Style.STROKE);
                p.setStrokeWidth(2.6f * d);
                p.setStrokeCap(Paint.Cap.ROUND);
                cv.drawArc(new RectF(cx - 11 * d, 42 * d, cx + 11 * d, 60 * d), 20, 140, false, p); // smile
            } else if (band == WidgetLogic.FACE_HALF) {
                p.setStrokeWidth(3 * d);
                p.setStrokeCap(Paint.Cap.ROUND);
                cv.drawLine(cx - 10.5f * d, 55.3f * d, cx + 10.5f * d, 55.3f * d, p); // flat
            } else {
                p.setStyle(Paint.Style.STROKE);
                p.setStrokeWidth(2.6f * d);
                p.setStrokeCap(Paint.Cap.ROUND);
                cv.drawArc(new RectF(cx - 11 * d, 52 * d, cx + 11 * d, 70 * d), 200, 140, false, p); // frown
            }
            return b;
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    /** Earbuds: the battery of the connected pair over their picture and their name; else CONNECT. */
    public static final class Buds extends Base {
        static volatile boolean on;
        static volatile String name;
        static volatile String address;
        static volatile int level = -1;
        static volatile boolean anyPaired;
        private static volatile boolean dirty;

        static boolean permitted(Context c) {
            return c.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED;
        }

        /** Reads the adapter and the paired devices once (placement, service start, Bluetooth on/off, a connection). */
        static void refresh(Context c) {
            name = null; address = null; level = -1; anyPaired = false;
            if (!permitted(c)) return;
            try {
                BluetoothManager bm = c.getSystemService(BluetoothManager.class);
                BluetoothAdapter a = bm == null ? null : bm.getAdapter();
                on = a != null && a.isEnabled();
                if (!on) return;
                Set<BluetoothDevice> paired = a.getBondedDevices();
                if (paired == null) return;
                for (BluetoothDevice dev : paired) {
                    BluetoothClass k = dev.getBluetoothClass();
                    if (!WidgetLogic.looksLikeBuds(dev.getName(), k == null ? 0 : k.getMajorDeviceClass())) continue;
                    anyPaired = true;
                    if (!hidden(dev, "isConnected", false)) continue;
                    name = dev.getName();
                    address = dev.getAddress();
                    level = hiddenInt(dev, "getBatteryLevel");
                    break;
                }
            } catch (Throwable ignored) { }
        }

        static boolean hidden(BluetoothDevice dev, String m, boolean fallback) {
            try { return (Boolean) BluetoothDevice.class.getMethod(m).invoke(dev); } catch (Throwable t) { return fallback; }
        }

        static int hiddenInt(BluetoothDevice dev, String m) {
            try { return (Integer) BluetoothDevice.class.getMethod(m).invoke(dev); } catch (Throwable t) { return -1; }
        }

        static final String ACTION_BATTERY_LEVEL = "android.bluetooth.device.action.BATTERY_LEVEL_CHANGED";
        static final String EXTRA_BATTERY_LEVEL = "android.bluetooth.device.extra.BATTERY_LEVEL";

        static IntentFilter filter() {
            IntentFilter f = new IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED);
            f.addAction(BluetoothDevice.ACTION_ACL_CONNECTED);
            f.addAction(BluetoothDevice.ACTION_ACL_DISCONNECTED);
            f.addAction(ACTION_BATTERY_LEVEL);
            return f;
        }

        /** From the service's Bluetooth receiver. */
        static void changed(Context c, Intent i, boolean interactive) {
            if (ACTION_BATTERY_LEVEL.equals(i.getAction())) {
                BluetoothDevice dev = i.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE);
                int lv = i.getIntExtra(EXTRA_BATTERY_LEVEL, -1);
                if (dev != null && dev.getAddress().equals(address)) level = lv;
                else refresh(c);
            } else {
                refresh(c);
            }
            if (interactive) new Buds().renderAll(c);
            else dirty = true;
        }

        static void screenOn(Context c) {
            if (dirty) { dirty = false; new Buds().renderAll(c); }
        }

        @Override void render(Context c, AppWidgetManager m, int id) {
            boolean ok = permitted(c);
            String[] lines = WidgetLogic.budsLines(ok, on, anyPaired, name, level);
            RemoteViews rv = new RemoteViews(c.getPackageName(), R.layout.widget_buds);
            rv.setTextViewText(R.id.w_status, lines[0]);
            String label = ClockRender.drawable(lines[1]);
            // Caps 12.2 dp in the original: 17.7 sp of DotoClock, its dots fattened (stroke 0.04 em) to the original's.
            rv.setImageViewBitmap(R.id.w_label, ClockRender.line(c, 17.7f,
                    new String[] {label.isEmpty() ? "EARBUDS" : label}, new int[] {INK}, 0, false, 0.04f));
            rv.setContentDescription(R.id.w_label, lines[1] + ", " + lines[0]);
            // The original: Bluetooth settings; without the permission, its own screen that asks for it.
            Intent tap = ok ? new Intent(Settings.ACTION_BLUETOOTH_SETTINGS) : new Intent(c, Grant.class);
            rv.setOnClickPendingIntent(R.id.w_root, activity(c, 500 + id % 1000, tap));
            m.updateAppWidget(id, rv);
        }

        @Override public void onUpdate(Context c, AppWidgetManager m, int[] ids) {
            refresh(c);
            super.onUpdate(c, m, ids);
        }
    }

    /** Asks for Bluetooth access (Nearby devices) for the earbuds widget, then redraws it and closes. */
    public static final class Grant extends Activity {
        @Override protected void onCreate(Bundle b) {
            super.onCreate(b);
            if (Buds.permitted(this)) { done(); return; }
            requestPermissions(new String[] {Manifest.permission.BLUETOOTH_CONNECT}, 1);
        }

        @Override public void onRequestPermissionsResult(int code, String[] perms, int[] results) { done(); }

        private void done() {
            Buds.refresh(this);
            new Buds().renderAll(this);
            GuardService.widgetsChanged(this);
            finish();
        }
    }
}
