/*
 * Apache-2.0 section 4(b) notice: windows() (multi-window enumeration sorted by layer, hash de-duplication, active-window
 * fallback) is derived from google/artemis HierarchyDumper.java (packages/artemis-accessibility-helper, com.artemis.helper,
 * lines 463-513; Copyright Google LLC, Apache License 2.0; upstream has no NOTICE file), via the S4 spike's S4Access.java.
 * It has been modified by the Agentroid authors (rewritten against the UiWindow/UiNode interfaces; the "not important"
 * filter is not carried over). The licence text is in the LICENSE file at the repository root.
 */
package dev.agentroid.helper.a11y;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.GestureDescription;
import android.graphics.Bitmap;
import android.graphics.ColorSpace;
import android.graphics.Path;
import android.graphics.Rect;
import android.hardware.HardwareBuffer;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.Display;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

import dev.agentroid.helper.core.Ports;
import dev.agentroid.helper.core.UiWindow;

/**
 * The accessibility side of the helper: tree dump, tap/swipe gestures, set_value through ACTION_SET_PROGRESS, screenshot.
 * Every method blocks until the platform answers and must be called off the main thread (the link's worker does).
 * Written for the product from the S4 spike's proven calls (S4Access.java); not yet run on a phone.
 */
final class A11yBridge implements Ports.A11y {
    private static final int MAX_SEARCH_NODES = 50000;

    private final AccessibilityService svc;
    private final Handler main = new Handler(Looper.getMainLooper());

    /** Told just before each dispatchGesture with its duration (the overlay's own-gesture window). */
    interface GestureNote { void own(long durationMs); }

    private volatile GestureNote note;

    A11yBridge(AccessibilityService svc) { this.svc = svc; }

    void setGestureNote(GestureNote n) { note = n; }

    // ------------------------------------------------------------------ tree

    @Override
    public List<? extends UiWindow> windows() throws Ports.PortException {
        List<UiWindow> out = new ArrayList<UiWindow>();
        Set<Integer> seen = new HashSet<Integer>();
        boolean hasApp = false;
        try {
            List<AccessibilityWindowInfo> windows = svc.getWindows();
            if (windows != null && !windows.isEmpty()) {
                List<AccessibilityWindowInfo> sorted = new ArrayList<AccessibilityWindowInfo>(windows);
                Collections.sort(sorted, new Comparator<AccessibilityWindowInfo>() {
                    @Override public int compare(AccessibilityWindowInfo a, AccessibilityWindowInfo b) {
                        return Integer.compare(b.getLayer(), a.getLayer());
                    }
                });
                for (AccessibilityWindowInfo w : sorted) {
                    try {
                        AccessibilityNodeInfo root = w.getRoot();
                        if (root == null) continue;
                        if (!seen.add(root.hashCode())) { AndroidNode.recycle(root); continue; }
                        if (w.getType() == AccessibilityWindowInfo.TYPE_APPLICATION) hasApp = true;
                        Rect b = new Rect();
                        w.getBoundsInScreen(b);
                        out.add(new AndroidWindow(w.getId(), windowType(w.getType()), w.getLayer(), w.isActive(), w.isFocused(),
                                b.isEmpty() ? null : new int[] {b.left, b.top, b.right, b.bottom}, new AndroidNode(root)));
                    } catch (RuntimeException ignored) {
                        // one broken window must not hide the others
                    }
                }
            }
        } catch (RuntimeException ignored) {
            // fall through to the active-window fallback
        }
        if (!hasApp) {
            try {
                AccessibilityNodeInfo active = svc.getRootInActiveWindow();
                if (active != null && seen.add(active.hashCode())) {
                    out.add(new AndroidWindow(active.getWindowId(), "ACTIVE_FALLBACK", 0, true, true, null, new AndroidNode(active)));
                }
            } catch (RuntimeException ignored) {
                // nothing more to try
            }
        }
        if (out.isEmpty()) throw new Ports.PortException("no_windows", "the accessibility service sees no windows");
        return out;
    }

    private static String windowType(int t) {
        switch (t) {
            case AccessibilityWindowInfo.TYPE_APPLICATION: return "APPLICATION";
            case AccessibilityWindowInfo.TYPE_INPUT_METHOD: return "INPUT_METHOD";
            case AccessibilityWindowInfo.TYPE_SYSTEM: return "SYSTEM";
            case AccessibilityWindowInfo.TYPE_ACCESSIBILITY_OVERLAY: return "ACCESSIBILITY_OVERLAY";
            case AccessibilityWindowInfo.TYPE_SPLIT_SCREEN_DIVIDER: return "SPLIT_SCREEN_DIVIDER";
            default: return "OTHER_" + t;
        }
    }

    // ------------------------------------------------------------------ gestures

    @Override
    public void tap(double x, double y) throws Ports.PortException {
        gesture(x, y, x, y, 50);
    }

    @Override
    public void swipe(double x1, double y1, double x2, double y2, long durationMs) throws Ports.PortException {
        gesture(x1, y1, x2, y2, durationMs);
    }

    private void gesture(double x1, double y1, double x2, double y2, long ms) throws Ports.PortException {
        final GestureDescription g;
        try {
            Path p = new Path();
            p.moveTo((float) x1, (float) y1);
            if (x1 != x2 || y1 != y2) p.lineTo((float) x2, (float) y2);
            g = new GestureDescription.Builder().addStroke(new GestureDescription.StrokeDescription(p, 0, Math.max(1, ms))).build();
        } catch (IllegalArgumentException e) {
            throw new Ports.PortException("bad_gesture", "the platform rejected the gesture");
        }
        final CountDownLatch done = new CountDownLatch(1);
        final AtomicReference<String> how = new AtomicReference<String>("timeout");
        main.post(new Runnable() {
            @Override public void run() {
                GestureNote n = note;
                if (n != null) n.own(Math.max(1, ms));
                boolean accepted = svc.dispatchGesture(g, new AccessibilityService.GestureResultCallback() {
                    @Override public void onCompleted(GestureDescription d) { how.set("completed"); done.countDown(); }
                    @Override public void onCancelled(GestureDescription d) { how.set("cancelled"); done.countDown(); }
                }, null);
                if (!accepted) { how.set("not_accepted"); done.countDown(); }
            }
        });
        boolean finished;
        try {
            finished = done.await(ms + 5000, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new Ports.PortException("interrupted", "interrupted while waiting for the gesture");
        }
        if (!finished) throw new Ports.PortException("gesture_timeout", "the gesture did not finish in time");
        if (!"completed".equals(how.get())) throw new Ports.PortException("gesture_" + how.get(), "the gesture was " + how.get());
    }

    // ------------------------------------------------------------------ set_value

    @Override
    public Map<String, Object> setValue(String by, String selector, double value) throws Ports.PortException {
        List<AccessibilityNodeInfo> matches = find(by, selector);
        try {
            if (matches.isEmpty()) throw new Ports.PortException("node_not_found", "no node matches that " + by);
            if (matches.size() > 1) throw new Ports.PortException("ambiguous", matches.size() + " nodes match that " + by + "; use a unique one");
            AccessibilityNodeInfo node = matches.get(0);
            if (!node.getActionList().contains(AccessibilityNodeInfo.AccessibilityAction.ACTION_SET_PROGRESS)) {
                throw new Ports.PortException("not_settable", "the node does not offer ACTION_SET_PROGRESS");
            }
            AccessibilityNodeInfo.RangeInfo before = node.getRangeInfo();
            if (before != null && (value < before.getMin() || value > before.getMax())) {
                throw new Ports.PortException("out_of_range", "value is outside " + before.getMin() + ".." + before.getMax());
            }
            Bundle args = new Bundle();
            args.putFloat(AccessibilityNodeInfo.ACTION_ARGUMENT_PROGRESS_VALUE, (float) value);
            boolean ok = node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SET_PROGRESS.getId(), args);
            if (!ok) throw new Ports.PortException("set_failed", "the node refused ACTION_SET_PROGRESS");
            node.refresh();
            AccessibilityNodeInfo.RangeInfo after = node.getRangeInfo();
            Map<String, Object> out = new LinkedHashMap<String, Object>();
            out.put("requested", Double.valueOf(value));
            out.put("now", after == null ? null : Double.valueOf(after.getCurrent()));
            out.put("min", after == null ? null : Double.valueOf(after.getMin()));
            out.put("max", after == null ? null : Double.valueOf(after.getMax()));
            return out;
        } finally {
            for (AccessibilityNodeInfo m : matches) AndroidNode.recycle(m);
        }
    }

    /** Exact matches across every window. The caller recycles what it gets back. */
    private List<AccessibilityNodeInfo> find(String by, String selector) throws Ports.PortException {
        List<AccessibilityNodeInfo> hits = new ArrayList<AccessibilityNodeInfo>();
        int[] budget = {MAX_SEARCH_NODES};
        List<? extends UiWindow> ws = windows();
        for (UiWindow w : ws) {
            AndroidNode root = (AndroidNode) w.root();
            try {
                search(root.n, by, selector, hits, budget);
            } finally {
                root.release();
            }
        }
        return hits;
    }

    private void search(AccessibilityNodeInfo n, String by, String sel, List<AccessibilityNodeInfo> hits, int[] budget) {
        if (budget[0]-- <= 0) return;
        if (matches(n, by, sel)) hits.add(AccessibilityNodeInfo.obtain(n));
        int cc = n.getChildCount();
        for (int i = 0; i < cc; i++) {
            AccessibilityNodeInfo c = n.getChild(i);
            if (c == null) continue;
            try {
                search(c, by, sel, hits, budget);
            } finally {
                AndroidNode.recycle(c);
            }
        }
    }

    private static boolean matches(AccessibilityNodeInfo n, String by, String sel) {
        if ("id".equals(by)) {
            String id = n.getViewIdResourceName();
            if (id == null) return false;
            return id.equals(sel) || (sel.indexOf('/') < 0 && id.endsWith(":id/" + sel));
        }
        CharSequence c = "text".equals(by) ? n.getText() : n.getContentDescription();
        return c != null && sel.equals(c.toString());
    }

    // ------------------------------------------------------------------ screenshot

    @Override
    public Ports.Screenshot screenshot() throws Ports.PortException {
        if (Build.VERSION.SDK_INT < 30) throw new Ports.PortException("unsupported_api", "takeScreenshot needs Android 11 (API 30)");
        final CountDownLatch done = new CountDownLatch(1);
        final AtomicReference<Ports.Screenshot> shot = new AtomicReference<Ports.Screenshot>();
        final AtomicReference<Ports.PortException> err = new AtomicReference<Ports.PortException>();
        Executor inline = new Executor() {
            @Override public void execute(Runnable r) { r.run(); }
        };
        svc.takeScreenshot(Display.DEFAULT_DISPLAY, inline, new AccessibilityService.TakeScreenshotCallback() {
            @Override public void onSuccess(AccessibilityService.ScreenshotResult s) {
                try {
                    HardwareBuffer hb = s.getHardwareBuffer();
                    try {
                        ColorSpace cs = s.getColorSpace();
                        Bitmap hw = Bitmap.wrapHardwareBuffer(hb, cs);
                        if (hw == null) throw new Ports.PortException("screenshot_failed", "could not wrap the screen buffer");
                        Bitmap bm = hw.copy(Bitmap.Config.ARGB_8888, false);
                        ByteArrayOutputStream bo = new ByteArrayOutputStream();
                        bm.compress(Bitmap.CompressFormat.PNG, 100, bo);
                        shot.set(new Ports.Screenshot(bm.getWidth(), bm.getHeight(), bo.toByteArray()));
                    } finally {
                        hb.close();
                    }
                } catch (Ports.PortException e) {
                    err.set(e);
                } catch (RuntimeException e) {
                    err.set(new Ports.PortException("screenshot_failed", "encoding the screenshot failed"));
                } finally {
                    done.countDown();
                }
            }

            @Override public void onFailure(int code) {
                // FLAG_SECURE windows come back as a failure, never as pixels: report secure_screen, not numbers.
                if (Build.VERSION.SDK_INT >= 34 && code == AccessibilityService.ERROR_TAKE_SCREENSHOT_SECURE_WINDOW) {
                    err.set(new Ports.PortException("secure_screen", "the foreground window is marked secure"));
                } else if (code == AccessibilityService.ERROR_TAKE_SCREENSHOT_INTERVAL_TIME_SHORT) {
                    err.set(new Ports.PortException("screenshot_rate_limited", "screenshots are limited to about one a second"));
                } else {
                    err.set(new Ports.PortException("screenshot_failed", "takeScreenshot failed with code " + code));
                }
                done.countDown();
            }
        });
        try {
            if (!done.await(10, TimeUnit.SECONDS)) throw new Ports.PortException("screenshot_timeout", "no screenshot within 10 s");
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new Ports.PortException("interrupted", "interrupted while waiting for the screenshot");
        }
        if (err.get() != null) throw err.get();
        return shot.get();
    }
}
