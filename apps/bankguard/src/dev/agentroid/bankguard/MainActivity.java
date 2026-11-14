package dev.agentroid.bankguard;

import android.app.Activity;
import android.app.AppOpsManager;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.content.pm.ShortcutInfo;
import android.content.pm.ShortcutManager;
import android.graphics.drawable.Icon;
import android.os.Bundle;
import android.os.Process;
import android.provider.Settings;
import android.view.View;
import android.widget.AdapterView;
import android.widget.ArrayAdapter;
import android.widget.Button;
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
 * The one screen: what the guard is doing, the two grants it needs, and the list of apps to guard (tap to tick).
 * Long-press an app for a home-screen shortcut that switches debugging off before the app opens.
 */
public final class MainActivity extends Activity {
    private TextView status;
    private ListView list;
    private final List<String[]> apps = new ArrayList<String[]>(); // {label, package}

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(32, 48, 32, 16);
        status = new TextView(this);
        status.setTextIsSelectable(true);
        root.addView(status);
        Button usage = new Button(this);
        usage.setText("Usage access settings");
        usage.setOnClickListener(new View.OnClickListener() { public void onClick(View v) {
            startActivity(new Intent(Settings.ACTION_USAGE_ACCESS_SETTINGS));
        } });
        root.addView(usage);
        TextView hint = new TextView(this);
        hint.setText("Ticked apps switch USB and Wireless debugging off while they are open. Long-press one for a "
                + "home-screen shortcut that switches debugging off before it opens.");
        hint.setPadding(0, 24, 0, 8);
        root.addView(hint);
        list = new ListView(this);
        list.setChoiceMode(ListView.CHOICE_MODE_MULTIPLE);
        root.addView(list, new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f));
        setContentView(root);
        list.setOnItemClickListener(new AdapterView.OnItemClickListener() {
            public void onItemClick(AdapterView<?> p, View v, int pos, long id) { saveTicks(); }
        });
        list.setOnItemLongClickListener(new AdapterView.OnItemLongClickListener() {
            public boolean onItemLongClick(AdapterView<?> p, View v, int pos, long id) { pin(apps.get(pos)); return true; }
        });
    }

    @Override
    protected void onResume() {
        super.onResume();
        startForegroundService(new Intent(this, GuardService.class));
        loadApps();
        refresh();
    }

    private void refresh() {
        boolean wss = checkSelfPermission("android.permission.WRITE_SECURE_SETTINGS") == PackageManager.PERMISSION_GRANTED;
        AppOpsManager ao = getSystemService(AppOpsManager.class);
        boolean usage = ao.unsafeCheckOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, Process.myUid(), getPackageName())
                == AppOpsManager.MODE_ALLOWED;
        GuardLogic.State st = GuardCore.get(this).state();
        status.setText("Agentroid Bank Guard\n"
                + (wss ? "✓" : "✗") + " WRITE_SECURE_SETTINGS" + (wss ? "" : " (grant it once over adb: see the README)") + "\n"
                + (usage ? "✓" : "✗") + " Usage access" + (usage ? "" : " (tap the button below, or grant it over adb)") + "\n"
                + (st.held ? "Holding: debugging is off while " + st.heldPkg + " is open" : "Watching; debugging is left as it is"));
    }

    private void loadApps() {
        apps.clear();
        Intent main = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
        PackageManager pm = getPackageManager();
        Set<String> seen = new LinkedHashSet<String>();
        for (ResolveInfo r : pm.queryIntentActivities(main, 0)) {
            String pkg = r.activityInfo.packageName;
            if (pkg.equals(getPackageName()) || !seen.add(pkg)) continue;
            apps.add(new String[] {String.valueOf(r.loadLabel(pm)), pkg});
        }
        final Set<String> banks = GuardCore.get(this).banks();
        Collections.sort(apps, new Comparator<String[]>() {
            public int compare(String[] a, String[] b) {
                boolean ba = banks.contains(a[1]), bb = banks.contains(b[1]);
                if (ba != bb) return ba ? -1 : 1;
                return a[0].compareToIgnoreCase(b[0]);
            }
        });
        List<String> rows = new ArrayList<String>();
        for (String[] a : apps) rows.add(a[0] + "\n" + a[1]);
        list.setAdapter(new ArrayAdapter<String>(this, android.R.layout.simple_list_item_multiple_choice, rows));
        for (int i = 0; i < apps.size(); i++) list.setItemChecked(i, banks.contains(apps.get(i)[1]));
    }

    /** Keep every list entry that is not an installed launcher app (another phone's bank, the test bank), then the ticks. */
    private void saveTicks() {
        GuardCore core = GuardCore.get(this);
        Set<String> keep = new LinkedHashSet<String>(core.banks());
        for (String[] a : apps) keep.remove(a[1]);
        for (int i = 0; i < apps.size(); i++) if (list.isItemChecked(i)) keep.add(apps.get(i)[1]);
        core.setBanks(keep);
        refresh();
    }

    private void pin(String[] app) {
        if (!GuardCore.get(this).banks().contains(app[1])) {
            Toast.makeText(this, "Tick " + app[0] + " first", Toast.LENGTH_SHORT).show();
            return;
        }
        ShortcutManager sm = getSystemService(ShortcutManager.class);
        if (sm == null || !sm.isRequestPinShortcutSupported()) {
            Toast.makeText(this, "This launcher does not take shortcuts", Toast.LENGTH_SHORT).show();
            return;
        }
        Intent open = new Intent(this, OpenSafely.class).setAction(Intent.ACTION_VIEW).putExtra(OpenSafely.EXTRA_PKG, app[1]);
        ShortcutInfo s = new ShortcutInfo.Builder(this, "safe:" + app[1])
                .setShortLabel(app[0] + " (safe)")
                .setLongLabel("Open " + app[0] + " with debugging off")
                .setIcon(Icon.createWithResource(this, android.R.drawable.ic_lock_lock))
                .setIntent(open)
                .build();
        sm.requestPinShortcut(s, null);
    }
}
