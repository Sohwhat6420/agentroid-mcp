package dev.agentroid.bankguard;

import android.app.Activity;
import android.appwidget.AppWidgetManager;
import android.appwidget.AppWidgetProviderInfo;
import android.content.ComponentName;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.os.Bundle;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.BaseAdapter;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.TextView;
import android.widget.Toast;

import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/**
 * Choose the dock's apps: tap apps in the order they should sit (up to six), pick Mono or Colour, Save. The pill at
 * the top is a live preview drawn by the same code as the widget. Opened by the launcher when the dock is placed or
 * reconfigured, by an empty dock's tap, and from the app's own screen.
 */
public final class DockConfigActivity extends Activity {
    private static final int BG = 0xFF0E0E10, SURFACE = 0xFF1B1B1D, RED = 0xFFCD3733, INK = 0xFFF4F4F5, DIM = 0xFF9A9AA0;

    private int id = AppWidgetManager.INVALID_APPWIDGET_ID;
    private final List<String> chosen = new ArrayList<String>();
    private final List<String[]> apps = new ArrayList<String[]>(); // {label, package}
    private boolean mono = true;
    private Typeface dots;
    private LinearLayout preview;
    private TextView monoBtn, colourBtn, count;
    private BaseAdapter adapter;

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        Intent in = getIntent();
        id = in == null ? AppWidgetManager.INVALID_APPWIDGET_ID
                : in.getIntExtra(AppWidgetManager.EXTRA_APPWIDGET_ID, AppWidgetManager.INVALID_APPWIDGET_ID);
        setResult(RESULT_CANCELED, new Intent().putExtra(AppWidgetManager.EXTRA_APPWIDGET_ID, id));
        AppWidgetProviderInfo info = id == AppWidgetManager.INVALID_APPWIDGET_ID ? null
                : AppWidgetManager.getInstance(this).getAppWidgetInfo(id);
        if (info == null || !new ComponentName(this, DockWidget.class).equals(info.provider)) { finish(); return; }

        chosen.addAll(DockWidget.apps(this, id));
        mono = DockWidget.mono(this, id);
        try { dots = getResources().getFont(R.font.doto_clock); } catch (Throwable t) { dots = Typeface.MONOSPACE; }
        loadApps();
        build();
        redraw();
    }

    private int dp(float v) { return Math.round(TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v, getResources().getDisplayMetrics())); }

    private static GradientDrawable pill(int color) {
        GradientDrawable g = new GradientDrawable();
        g.setColor(color);
        g.setCornerRadius(10_000);
        return g;
    }

    private TextView text(String s, float sp, int color, Typeface tf) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        t.setTextColor(color);
        if (tf != null) t.setTypeface(tf);
        return t;
    }

    private void build() {
        getWindow().setStatusBarColor(BG);
        getWindow().setNavigationBarColor(BG);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(BG);
        root.setPadding(dp(20), dp(28), dp(20), dp(16));

        root.addView(text("DOCK", 40, INK, dots));
        TextView sub = text("Tap apps in the order you want them, up to six.", 15, DIM, null);
        sub.setPadding(0, dp(4), 0, dp(16));
        root.addView(sub);

        preview = new LinearLayout(this);
        preview.setOrientation(LinearLayout.HORIZONTAL);
        preview.setGravity(Gravity.CENTER);
        preview.setBackground(pill(SURFACE));
        preview.setPadding(dp(8), dp(8), dp(8), dp(8));
        preview.setContentDescription("Dock preview");
        root.addView(preview, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(76)));

        LinearLayout styleRow = new LinearLayout(this);
        styleRow.setOrientation(LinearLayout.HORIZONTAL);
        styleRow.setGravity(Gravity.CENTER_VERTICAL);
        styleRow.setPadding(0, dp(16), 0, dp(8));
        monoBtn = chip("MONO");
        colourBtn = chip("COLOUR");
        monoBtn.setOnClickListener(new View.OnClickListener() { public void onClick(View v) { mono = true; redraw(); } });
        colourBtn.setOnClickListener(new View.OnClickListener() { public void onClick(View v) { mono = false; redraw(); } });
        styleRow.addView(monoBtn);
        LinearLayout.LayoutParams gap = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, dp(44));
        gap.setMarginStart(dp(8));
        styleRow.addView(colourBtn, gap);
        View spacer = new View(this);
        styleRow.addView(spacer, new LinearLayout.LayoutParams(0, 1, 1f));
        count = text("", 14, DIM, null);
        styleRow.addView(count);
        root.addView(styleRow);

        ListView list = new ListView(this);
        list.setDivider(null);
        adapter = new AppAdapter();
        list.setAdapter(adapter);
        root.addView(list, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));

        TextView save = text("SAVE", 22, INK, dots);
        save.setGravity(Gravity.CENTER);
        save.setBackground(pill(RED));
        save.setContentDescription("Save the dock");
        save.setOnClickListener(new View.OnClickListener() { public void onClick(View v) { save(); } });
        LinearLayout.LayoutParams sp = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(56));
        sp.topMargin = dp(12);
        root.addView(save, sp);

        setContentView(root);
    }

    private TextView chip(String s) {
        TextView t = text(s, 16, INK, dots);
        t.setGravity(Gravity.CENTER);
        t.setMinHeight(dp(44));
        t.setPadding(dp(18), 0, dp(18), 0);
        return t;
    }

    private void redraw() {
        monoBtn.setBackground(pill(mono ? RED : SURFACE));
        colourBtn.setBackground(pill(mono ? SURFACE : RED));
        monoBtn.setSelected(mono);
        colourBtn.setSelected(!mono);
        count.setText(chosen.size() + " of " + DockLogic.MAX_APPS);
        preview.removeAllViews();
        int px = dp(56);
        if (chosen.isEmpty()) {
            preview.addView(text("CHOOSE APPS", 18, DIM, dots));
        }
        for (String p : chosen) {
            ImageView iv = new ImageView(this);
            iv.setImageBitmap(DockWidget.icon(this, p, mono, px));
            iv.setScaleType(ImageView.ScaleType.FIT_CENTER);
            iv.setPadding(dp(3), dp(3), dp(3), dp(3));
            iv.setContentDescription(DockWidget.label(getPackageManager(), p));
            preview.addView(iv, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1f));
        }
        if (adapter != null) adapter.notifyDataSetChanged();
    }

    private void loadApps() {
        Intent main = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
        PackageManager pm = getPackageManager();
        Set<String> seen = new LinkedHashSet<String>();
        for (ResolveInfo r : pm.queryIntentActivities(main, 0)) {
            String pkg = r.activityInfo.packageName;
            if (!seen.add(pkg)) continue;
            apps.add(new String[] {String.valueOf(r.loadLabel(pm)), pkg});
        }
        Collections.sort(apps, new Comparator<String[]>() {
            public int compare(String[] a, String[] b) { return a[0].compareToIgnoreCase(b[0]); }
        });
    }

    private void save() {
        DockWidget.save(this, id, chosen, mono);
        DockWidget.render(this, AppWidgetManager.getInstance(this), id);
        setResult(RESULT_OK, new Intent().putExtra(AppWidgetManager.EXTRA_APPWIDGET_ID, id));
        finish();
    }

    /** One row per app: its icon, its name, and its place in the dock (a red number) when chosen. */
    private final class AppAdapter extends BaseAdapter {
        public int getCount() { return apps.size(); }
        public Object getItem(int i) { return apps.get(i); }
        public long getItemId(int i) { return i; }

        public View getView(int i, View old, ViewGroup parent) {
            final String[] app = apps.get(i);
            LinearLayout row = new LinearLayout(DockConfigActivity.this);
            row.setOrientation(LinearLayout.HORIZONTAL);
            row.setGravity(Gravity.CENTER_VERTICAL);
            row.setMinimumHeight(dp(56));
            row.setPadding(dp(4), dp(6), dp(4), dp(6));
            ImageView iv = new ImageView(DockConfigActivity.this);
            try { iv.setImageDrawable(getPackageManager().getApplicationIcon(app[1])); } catch (Throwable ignored) { }
            row.addView(iv, new LinearLayout.LayoutParams(dp(40), dp(40)));
            TextView name = text(app[0], 17, INK, null);
            name.setPadding(dp(14), 0, dp(8), 0);
            name.setSingleLine(true);
            row.addView(name, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
            int place = chosen.indexOf(app[1]);
            TextView badge = text(place < 0 ? "" : String.valueOf(place + 1), 16, INK, dots);
            badge.setGravity(Gravity.CENTER);
            if (place >= 0) badge.setBackground(pill(RED));
            else {
                GradientDrawable ring = pill(0x00000000);
                ring.setStroke(dp(1.5f), 0xFF3A3A3F);
                badge.setBackground(ring);
            }
            row.addView(badge, new LinearLayout.LayoutParams(dp(32), dp(32)));
            row.setContentDescription(app[0] + (place < 0 ? ", not in the dock" : ", place " + (place + 1) + " in the dock"));
            row.setOnClickListener(new View.OnClickListener() { public void onClick(View v) {
                if (!DockLogic.toggle(chosen, app[1])) {
                    Toast.makeText(DockConfigActivity.this, "The dock holds six apps: take one out first", Toast.LENGTH_SHORT).show();
                }
                redraw();
            } });
            return row;
        }
    }
}
