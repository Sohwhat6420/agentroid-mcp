package dev.agentroid.helper.a11y;

import android.accessibilityservice.AccessibilityService;
import android.content.Intent;
import android.os.SystemClock;
import android.util.Log;
import android.view.accessibility.AccessibilityEvent;

import dev.agentroid.helper.core.PortRegistry;
import dev.agentroid.helper.overlay.HumanTouchOverlay;
import dev.agentroid.helper.overlay.logic.HumanTouchLog;

/**
 * The helper's accessibility service. Config is in res/xml/helper_access.xml (flagIncludeNotImportantViews |
 * flagReportViewIds | flagRetrieveInteractiveWindows, canTakeScreenshot, canPerformGestures). It does nothing on its
 * own: it publishes {@link A11yBridge} to the {@link PortRegistry} while bound, and the link calls through that.
 * It does not start the link: the link starts from boot, the config receiver or the app, and honours the owner's Stop.
 *
 * <p>It also hosts the human-touch overlay (one app since 11 Nov 2026): every human touch is handed to
 * {@link PortRegistry#humanTouched}, where the app's runner flag picks it up, and each gesture the bridge dispatches is
 * marked first so it is not counted as human. The {@code human_touch} frame for the hub is not sent yet (the hub reads
 * human touches from getevent today). If the overlay cannot be added, the service carries on without it.
 */
public class HelperAccessibilityService extends AccessibilityService {
    private static final String TAG = "AgentroidA11y";
    private A11yBridge bridge;
    private HumanTouchOverlay overlay;

    @Override
    protected void onServiceConnected() {
        super.onServiceConnected();
        bridge = new A11yBridge(this);
        PortRegistry.shared().setA11y(bridge);
        try {
            final HumanTouchOverlay o = new HumanTouchOverlay(this, new HumanTouchLog.Sink() {
                @Override public void send(String frame) {
                    PortRegistry.shared().humanTouched(SystemClock.elapsedRealtime());
                }
            });
            o.attach();
            overlay = o;
            bridge.setGestureNote(new A11yBridge.GestureNote() {
                @Override public void own(long durationMs) { o.noteOwnGesture(durationMs); }
            });
        } catch (RuntimeException e) {
            Log.w(TAG, "human-touch overlay not attached");
            overlay = null;
        }
    }

    @Override public void onAccessibilityEvent(AccessibilityEvent event) { }

    @Override public void onInterrupt() { }

    @Override
    public boolean onUnbind(Intent intent) {
        release();
        return super.onUnbind(intent);
    }

    @Override
    public void onDestroy() {
        release();
        super.onDestroy();
    }

    private void release() {
        if (overlay != null) {
            try {
                overlay.detach();
            } catch (RuntimeException e) {
                Log.w(TAG, "human-touch overlay detach failed");
            }
            overlay = null;
        }
        if (bridge != null) {
            bridge.setGestureNote(null);
            PortRegistry.shared().clearA11y(bridge);
            bridge = null;
        }
    }
}
