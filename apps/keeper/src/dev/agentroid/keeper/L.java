package dev.agentroid.keeper;

import android.content.Context;
import android.util.Log;

import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;

/** Phone-local log: files/keeper.log (append, reset if over ~1 MB). */
final class L {
    private static File file;
    private static final SimpleDateFormat FMT = new SimpleDateFormat("MM-dd HH:mm:ss.SSS", Locale.ROOT);

    static synchronized void init(Context c) {
        if (file != null) return;
        // Device-protected storage: the log (and the prefs) must work before the first unlock after a reboot.
        Context dp = c.isDeviceProtectedStorage() ? c : c.createDeviceProtectedStorageContext();
        file = new File(dp.getFilesDir(), "keeper.log");
        try {
            if (file.length() > 1_000_000) file.delete();
        } catch (Throwable ignored) { }
    }

    static synchronized void i(String msg) {
        Log.i("Keeper", msg);
        if (file == null) return;
        try (FileOutputStream o = new FileOutputStream(file, true)) {
            o.write((FMT.format(new Date()) + " " + msg + "\n").getBytes(StandardCharsets.UTF_8));
        } catch (Throwable ignored) { }
    }

    static void e(String msg, Throwable t) {
        i(msg + " :: " + t);
    }
}
