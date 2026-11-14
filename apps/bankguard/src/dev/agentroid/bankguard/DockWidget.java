package dev.agentroid.bankguard;

import android.app.PendingIntent;
import android.appwidget.AppWidgetManager;
import android.appwidget.AppWidgetProvider;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.ColorMatrix;
import android.graphics.ColorMatrixColorFilter;
import android.graphics.Paint;
import android.graphics.PorterDuff;
import android.graphics.PorterDuffColorFilter;
import android.graphics.drawable.AdaptiveIconDrawable;
import android.graphics.drawable.Drawable;
import android.net.Uri;
import android.os.Build;
import android.view.View;
import android.widget.RemoteViews;

import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * The app dock: a pill of up to six apps the owner picked (DockConfigActivity). Drawn once per change: when it is
 * placed or reconfigured, when the launcher asks (onUpdate: a launcher restart, a boot) and when this app is updated.
 * No timer, no alarm (updatePeriodMillis 0). A bank on the bank guard's list opens through OpenSafely, so debugging
 * is off before the bank starts.
 */
public final class DockWidget extends AppWidgetProvider {
    static final int[] SLOTS = {R.id.dock_s0, R.id.dock_s1, R.id.dock_s2, R.id.dock_s3, R.id.dock_s4, R.id.dock_s5};
    static final String PREFS = "widgets";
    static final int SURFACE_SLOT = 0xFF2A2A2E, INK = 0xFFF4F4F5;

    @Override
    public void onUpdate(Context c, AppWidgetManager m, int[] ids) {
        for (int id : ids) render(c, m, id);
    }

    @Override
    public void onDeleted(Context c, int[] ids) {
        SharedPreferences.Editor e = prefs(c).edit();
        for (int id : ids) e.remove(keyApps(id)).remove(keyStyle(id));
        e.apply();
    }

    static SharedPreferences prefs(Context c) {
        return c.createDeviceProtectedStorageContext().getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    static String keyApps(int id) { return "dock." + id + ".apps"; }
    static String keyStyle(int id) { return "dock." + id + ".mono"; }

    static List<String> apps(Context c, int id) { return DockLogic.parse(prefs(c).getString(keyApps(id), null)); }
    static boolean mono(Context c, int id) { return prefs(c).getBoolean(keyStyle(id), true); }
    static boolean configured(Context c, int id) { return prefs(c).contains(keyApps(id)); }

    static void save(Context c, int id, List<String> apps, boolean mono) {
        prefs(c).edit().putString(keyApps(id), DockLogic.join(apps)).putBoolean(keyStyle(id), mono).commit();
    }

    /** Redraw every placed dock (after an app update, for instance). */
    static void renderAll(Context c) {
        AppWidgetManager m = AppWidgetManager.getInstance(c);
        for (int id : m.getAppWidgetIds(new ComponentName(c, DockWidget.class))) render(c, m, id);
    }

    static void render(Context c, AppWidgetManager m, int id) {
        RemoteViews rv = new RemoteViews(c.getPackageName(), R.layout.widget_dock);
        PackageManager pm = c.getPackageManager();
        List<String> want = apps(c, id);
        Set<String> present = new HashSet<String>();
        for (String p : want) if (pm.getLaunchIntentForPackage(p) != null) present.add(p);
        List<String> show = DockLogic.installed(want, present);
        boolean mono = mono(c, id);
        Set<String> banks = GuardCore.get(c).banks();
        int px = Math.min(192, Math.round(56 * c.getResources().getDisplayMetrics().density));
        for (int i = 0; i < SLOTS.length; i++) {
            if (i >= show.size()) { rv.setViewVisibility(SLOTS[i], View.GONE); continue; }
            String pkg = show.get(i);
            rv.setViewVisibility(SLOTS[i], View.VISIBLE);
            rv.setImageViewBitmap(SLOTS[i], icon(c, pkg, mono, px));
            rv.setContentDescription(SLOTS[i], label(pm, pkg));
            rv.setOnClickPendingIntent(SLOTS[i], PendingIntent.getActivity(c, DockLogic.requestCode(id, i),
                    launch(c, pm, pkg, banks, id, i), PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
        }
        rv.setViewVisibility(R.id.dock_empty, show.isEmpty() ? View.VISIBLE : View.GONE);
        if (show.isEmpty()) {
            Intent edit = new Intent(c, DockConfigActivity.class).putExtra(AppWidgetManager.EXTRA_APPWIDGET_ID, id)
                    .setData(Uri.parse("agentroid-dock:edit/" + id));
            rv.setOnClickPendingIntent(R.id.w_root, PendingIntent.getActivity(c, DockLogic.requestCode(id, 7), edit,
                    PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
        }
        m.updateAppWidget(id, rv);
    }

    private static Intent launch(Context c, PackageManager pm, String pkg, Set<String> banks, int id, int slot) {
        if (GuardLogic.isBank(banks, pkg)) {
            // The bank guard's "(safe)" path: debugging off first. The data URI keeps each slot's intent distinct.
            return new Intent(c, OpenSafely.class).putExtra(OpenSafely.EXTRA_PKG, pkg)
                    .setData(Uri.parse("agentroid-dock:" + id + "/" + slot));
        }
        Intent i = pm.getLaunchIntentForPackage(pkg);
        return i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
    }

    static String label(PackageManager pm, String pkg) {
        try { return String.valueOf(pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0))); }
        catch (Throwable t) { return pkg; }
    }

    /**
     * One button, drawn once. Mono: the app's themed (monochrome) icon in white on a dark circle, the Nothing look;
     * an app with no monochrome icon gets its own icon in greyscale. Colour: the app's own icon.
     */
    static Bitmap icon(Context c, String pkg, boolean mono, int px) {
        Bitmap b = Bitmap.createBitmap(px, px, Bitmap.Config.ARGB_8888);
        Canvas cv = new Canvas(b);
        Drawable d;
        try { d = c.getPackageManager().getApplicationIcon(pkg).mutate(); }
        catch (Throwable t) { d = c.getDrawable(android.R.drawable.sym_def_app_icon); }
        if (!mono) {
            d.setBounds(0, 0, px, px);
            d.draw(cv);
            return b;
        }
        Paint bg = new Paint(Paint.ANTI_ALIAS_FLAG);
        bg.setColor(SURFACE_SLOT);
        cv.drawCircle(px / 2f, px / 2f, px / 2f, bg);
        Drawable glyph = null;
        if (Build.VERSION.SDK_INT >= 33 && d instanceof AdaptiveIconDrawable) glyph = ((AdaptiveIconDrawable) d).getMonochrome();
        if (glyph != null) {
            // An adaptive layer is 108 units with its art in the middle 72: drawn 1.1x the button, the art fills ~half.
            glyph = glyph.mutate();
            int ext = Math.round(px * 0.05f);
            glyph.setBounds(-ext, -ext, px + ext, px + ext);
            glyph.setColorFilter(new PorterDuffColorFilter(INK, PorterDuff.Mode.SRC_IN));
            glyph.draw(cv);
        } else {
            ColorMatrix grey = new ColorMatrix();
            grey.setSaturation(0);
            d.setColorFilter(new ColorMatrixColorFilter(grey));
            int in = Math.round(px * 0.2f);
            d.setBounds(in, in, px - in, px - in);
            d.draw(cv);
        }
        return b;
    }
}
