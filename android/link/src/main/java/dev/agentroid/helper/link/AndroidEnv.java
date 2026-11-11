package dev.agentroid.helper.link;

import android.content.ComponentName;
import android.content.Context;
import android.os.Build;
import android.provider.Settings;
import android.view.inputmethod.InputMethodInfo;
import android.view.inputmethod.InputMethodManager;

import java.util.List;

import dev.agentroid.helper.core.HelperEnv;

/**
 * Facts for the status report. The IME is found by class NAME, not by a compile-time reference, because :link ships in
 * every flavour and :ime does not.
 */
final class AndroidEnv implements HelperEnv {
    static final String VERSION = "0.1.0-v0";
    static final String IME_CLASS = "dev.agentroid.helper.ime.HelperInputMethodService";

    private final Context c;

    AndroidEnv(Context c) { this.c = c.getApplicationContext(); }

    @Override public String helperVersion() { return VERSION; }

    @Override public int sdkInt() { return Build.VERSION.SDK_INT; }

    @Override
    public boolean imeEnabled() {
        try {
            InputMethodManager imm = (InputMethodManager) c.getSystemService(Context.INPUT_METHOD_SERVICE);
            List<InputMethodInfo> list = imm.getEnabledInputMethodList();
            for (InputMethodInfo i : list) {
                if (i.getPackageName().equals(c.getPackageName()) && IME_CLASS.equals(i.getServiceName())) return true;
            }
        } catch (RuntimeException ignored) {
            // report "not enabled" rather than fail the status call
        }
        return false;
    }

    @Override
    public boolean imeSelected() {
        try {
            String cur = Settings.Secure.getString(c.getContentResolver(), Settings.Secure.DEFAULT_INPUT_METHOD);
            if (cur == null) return false;
            ComponentName me = new ComponentName(c.getPackageName(), IME_CLASS);
            return cur.equals(me.flattenToString()) || cur.equals(me.flattenToShortString());
        } catch (RuntimeException e) {
            return false;
        }
    }
}
