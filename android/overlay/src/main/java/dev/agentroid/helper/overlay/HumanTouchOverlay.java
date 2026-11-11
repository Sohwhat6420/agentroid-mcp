package dev.agentroid.helper.overlay;

import android.accessibilityservice.AccessibilityService;
import android.content.Context;
import android.content.SharedPreferences;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.PixelFormat;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.view.Gravity;
import android.view.MotionEvent;
import android.view.View;
import android.view.WindowManager;

import dev.agentroid.helper.overlay.logic.DotTimer;
import dev.agentroid.helper.overlay.logic.HumanTouchLog;
import dev.agentroid.helper.overlay.logic.OverlayClassifier;
import dev.agentroid.helper.overlay.logic.TouchSample;

/**
 * Detector 2 (task 2.15), the thin Android half. A transparent, non-focusable {@code TYPE_ACCESSIBILITY_OVERLAY} in the
 * top-right corner of display 0, with {@code FLAG_NOT_TOUCH_MODAL | FLAG_WATCH_OUTSIDE_TOUCH}: every touch that lands
 * anywhere else on the screen arrives as {@code ACTION_OUTSIDE}, and one on the corner itself as {@code ACTION_DOWN},
 * so the whole screen is covered without the window taking a single touch from the app below. (A full-screen window
 * cannot do this: touchable, it would swallow the touches; not touchable, it would receive none.) Each event goes
 * through {@link OverlayClassifier}; a human one is written locally ({@link HumanTouchLog}, works with adb down) and
 * queued as a {@code human_touch} frame. The same window draws the reclaim warning dot ({@link DotTimer}).
 *
 * <p>Owned by the accessibility service: {@link #attach()} from onServiceConnected, {@link #detach()} from
 * onUnbind/onDestroy. Call {@link #noteOwnGesture} just before every dispatchGesture (it matters on API 29 only).
 */
public final class HumanTouchOverlay {
    private static final String PREFS = "agentroid_overlay";
    private static final String KEY_LAST = "last_human_touch";
    /** Window side in dp: the dot's box. The window draws nothing unless the dot is showing. */
    private static final int BOX_DP = 14;

    private final AccessibilityService service;
    private final WindowManager wm;
    private final Handler main = new Handler(Looper.getMainLooper());
    private final AndroidTouchscreens touchscreens;
    private final OverlayClassifier classifier;
    private final HumanTouchLog log;
    private final DotTimer dot = new DotTimer();
    private DotView view;

    public HumanTouchOverlay(AccessibilityService service, HumanTouchLog.Sink sink) {
        this.service = service;
        this.wm = service.getSystemService(WindowManager.class);
        this.touchscreens = new AndroidTouchscreens(service);
        this.classifier = new OverlayClassifier(touchscreens.registry, Build.VERSION.SDK_INT);
        final SharedPreferences prefs = service.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
        this.log = new HumanTouchLog(new HumanTouchLog.Store() {
            @Override public void putLastTouch(long wallMs) { prefs.edit().putLong(KEY_LAST, wallMs).apply(); }
            @Override public long lastTouch() { return prefs.getLong(KEY_LAST, -1); }
        }, sink);
    }

    public HumanTouchLog log() { return log; }

    public void noteOwnGesture(long durationMs) {
        classifier.noteOwnGesture(SystemClock.uptimeMillis(), durationMs);
    }

    public void attach() {
        if (view != null) return;
        touchscreens.start(main);
        int px = Math.round(BOX_DP * service.getResources().getDisplayMetrics().density);
        WindowManager.LayoutParams lp = new WindowManager.LayoutParams(px, px,
                WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
                WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                        | WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL
                        | WindowManager.LayoutParams.FLAG_WATCH_OUTSIDE_TOUCH
                        | WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
                PixelFormat.TRANSLUCENT);
        lp.gravity = Gravity.TOP | Gravity.END;
        lp.setTitle("agentroid-human-touch");
        view = new DotView(service);
        view.setOnTouchListener(new View.OnTouchListener() {
            @Override public boolean onTouch(View v, MotionEvent e) {
                onEvent(e);
                return false;
            }
        });
        wm.addView(view, lp);
    }

    public void detach() {
        main.removeCallbacksAndMessages(null);
        if (view != null) {
            wm.removeViewImmediate(view);
            view = null;
        }
        touchscreens.stop();
    }

    /** Hub's ShowDot: the 10 s warning. */
    public void showDot() {
        long until = dot.start(SystemClock.uptimeMillis());
        redraw();
        main.postAtTime(new Runnable() { @Override public void run() { redraw(); } }, until + 1);
    }

    /** Hub's HideDot. */
    public void hideDot() {
        dot.hide();
        redraw();
    }

    private void redraw() {
        if (view != null) view.invalidate();
    }

    private void onEvent(MotionEvent e) {
        TouchSample s = new TouchSample(e.getActionMasked(), e.getSource(), e.getDeviceId(), e.getFlags(),
                e.getPointerCount() > 0 ? e.getToolType(0) : 0, e.getEventTime());
        if (classifier.classify(s) != OverlayClassifier.Verdict.HUMAN) return;
        long wall = System.currentTimeMillis() - (SystemClock.uptimeMillis() - e.getEventTime());
        log.record(wall);
        if (dot.onHumanTouch(SystemClock.uptimeMillis())) redraw();
    }

    /** Draws the dot while the warning runs; transparent otherwise. */
    private final class DotView extends View {
        private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);

        DotView(Context c) {
            super(c);
            paint.setColor(0xCCE8A33D);
        }

        @Override protected void onDraw(Canvas canvas) {
            if (!dot.visible(SystemClock.uptimeMillis())) return;
            float r = Math.min(getWidth(), getHeight()) * 0.3f;
            canvas.drawCircle(getWidth() / 2f, getHeight() / 2f, r, paint);
        }
    }
}
