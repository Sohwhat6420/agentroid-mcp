/*
 * Agentroid's fallback screen read: what `uiautomator dump` does, without failing when the UI never goes idle.
 *
 * Why (11 Nov 2026): `uiautomator dump` waits for the UI to be idle (500 ms of quiet, up to 10 s) and gives up
 * with "ERROR: could not get idle state." on a screen that keeps changing, such as a running stopwatch. The
 * screen guard then could not read the screen and refused the tap (deny by default). This class makes the same
 * calls as AOSP's DumpCommand (frameworks/testing, Apache-2.0), through reflection on the platform's own
 * /system/framework/uiautomator.jar, so the XML is the platform's own; it only waits briefly for idle and goes
 * on if the UI never settles. It writes nothing but the dump file and never acts on the screen.
 *
 * Run (as the shell user, over adb):
 *   CLASSPATH=/data/local/tmp/agentroid-dump.dex:/system/framework/uiautomator.jar \
 *     app_process /system/bin dev.agentroid.dump.Dump [/dev/tty]
 * stderr's last line is "agentroid-dump: idle" or "agentroid-dump: not-idle", so the caller can log which.
 */
package dev.agentroid.dump;

import android.app.UiAutomation;
import android.graphics.Point;
import android.view.Display;
import android.view.accessibility.AccessibilityNodeInfo;

import java.io.File;
import java.util.concurrent.TimeoutException;

public final class Dump {
    private Dump() {}

    public static void main(String[] args) throws Exception {
        File out = new File(args.length > 0 ? args[0] : "/dev/tty");
        Class<?> wrapC = Class.forName("com.android.uiautomator.core.UiAutomationShellWrapper");
        Object wrap = wrapC.getConstructor().newInstance();
        wrapC.getMethod("connect").invoke(wrap);
        try {
            wrapC.getMethod("setCompressedLayoutHierarchy", boolean.class).invoke(wrap, false);
            UiAutomation ui = (UiAutomation) wrapC.getMethod("getUiAutomation").invoke(wrap);
            String idle = "idle";
            try {
                ui.waitForIdle(500, 2000);
            } catch (TimeoutException e) {
                idle = "not-idle";
            }
            AccessibilityNodeInfo root = null;
            for (int i = 0; i < 10 && root == null; i++) {
                root = ui.getRootInActiveWindow();
                if (root == null) {
                    Thread.sleep(100);
                }
            }
            if (root == null) {
                System.err.println("ERROR: null root node returned by UiAutomation.");
                System.exit(2);
            }
            Class<?> dmg = Class.forName("android.hardware.display.DisplayManagerGlobal");
            Object global = dmg.getMethod("getInstance").invoke(null);
            Display display = (Display) dmg.getMethod("getRealDisplay", int.class).invoke(global, Display.DEFAULT_DISPLAY);
            Point size = new Point();
            display.getSize(size);
            Class<?> dumper = Class.forName("com.android.uiautomator.core.AccessibilityNodeInfoDumper");
            dumper.getMethod("dumpWindowToFile", AccessibilityNodeInfo.class, File.class, int.class, int.class, int.class)
                    .invoke(null, root, out, display.getRotation(), size.x, size.y);
            System.err.println("agentroid-dump: " + idle);
        } finally {
            wrapC.getMethod("disconnect").invoke(wrap);
        }
    }
}
