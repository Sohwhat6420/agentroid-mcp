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
 *
 * The heads-up check (14 Nov 2026, the protected phone's screenshot guard):
 *   CLASSPATH=/data/local/tmp/agentroid-dump.dex:/system/framework/uiautomator.jar \
 *     app_process /system/bin dev.agentroid.dump.Dump --heads-up
 * A heads-up notification is drawn in a System UI window, not the active window `uiautomator dump` reads, so
 * this mode asks for every window on screen (FLAG_RETRIEVE_INTERACTIVE_WINDOWS on this connection only; no
 * phone setting is changed) and walks System UI's visible nodes for a notification row. It prints exactly one
 * line to stdout, "agentroid-hun: yes", "agentroid-hun: no" or "agentroid-hun: unknown", and nothing of what the
 * notification says. It reads nothing else and writes nothing.
 */
package dev.agentroid.dump;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.UiAutomation;
import android.graphics.Point;
import android.view.Display;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import java.io.File;
import java.util.ArrayDeque;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.TimeoutException;

public final class Dump {
    private Dump() {}

    /**
     * Resource-id fragments of a visible notification row (a subset of core's preview marks: the empty shade's own
     * containers are left out, so a collapsed shade is not a "yes").
     */
    private static final String[] HEADS_UP_MARKS = {
        "expandablenotificationrow",
        "notification_row",
        "notification_content",
        "notification_main_column",
        "heads_up",
        "headsup",
    };

    /** At most this many nodes are walked per check (a heads-up sits near the top of its window). */
    private static final int MAX_NODES = 4000;

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && "--heads-up".equals(args[0])) {
            headsUp();
            return;
        }
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

    /** Prints "agentroid-hun: yes|no|unknown" and nothing else to stdout. */
    private static void headsUp() {
        String answer = "unknown";
        Object wrap = null;
        Class<?> wrapC = null;
        try {
            wrapC = Class.forName("com.android.uiautomator.core.UiAutomationShellWrapper");
            wrap = wrapC.getConstructor().newInstance();
            wrapC.getMethod("connect").invoke(wrap);
            UiAutomation ui = (UiAutomation) wrapC.getMethod("getUiAutomation").invoke(wrap);
            AccessibilityServiceInfo info = ui.getServiceInfo();
            info.flags |= AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS;
            ui.setServiceInfo(info);
            List<AccessibilityWindowInfo> windows = null;
            for (int i = 0; i < 10; i++) {
                windows = ui.getWindows();
                if (windows != null && !windows.isEmpty()) {
                    break;
                }
                Thread.sleep(100);
            }
            if (windows != null && !windows.isEmpty()) {
                answer = anyHeadsUp(windows) ? "yes" : "no";
            }
        } catch (Throwable t) {
            answer = "unknown";
        } finally {
            if (wrap != null) {
                try {
                    wrapC.getMethod("disconnect").invoke(wrap);
                } catch (Throwable ignored) {
                    // the answer stands; the connection ends with the process
                }
            }
        }
        System.out.println("agentroid-hun: " + answer);
        System.out.flush();
    }

    private static boolean anyHeadsUp(List<AccessibilityWindowInfo> windows) {
        int walked = 0;
        for (AccessibilityWindowInfo w : windows) {
            AccessibilityNodeInfo root = w.getRoot();
            if (root == null) {
                continue;
            }
            ArrayDeque<AccessibilityNodeInfo> todo = new ArrayDeque<>();
            todo.add(root);
            while (!todo.isEmpty()) {
                if (++walked > MAX_NODES) {
                    // too much to walk in time: not a "no"
                    throw new IllegalStateException("too many nodes");
                }
                AccessibilityNodeInfo n = todo.poll();
                CharSequence pkg = n.getPackageName();
                String id = n.getViewIdResourceName();
                if (pkg != null && "com.android.systemui".contentEquals(pkg) && id != null && n.isVisibleToUser()) {
                    String lower = id.toLowerCase(Locale.ROOT);
                    for (String m : HEADS_UP_MARKS) {
                        if (lower.contains(m)) {
                            return true;
                        }
                    }
                }
                for (int i = 0; i < n.getChildCount(); i++) {
                    AccessibilityNodeInfo c = n.getChild(i);
                    if (c != null) {
                        todo.add(c);
                    }
                }
            }
        }
        return false;
    }
}
