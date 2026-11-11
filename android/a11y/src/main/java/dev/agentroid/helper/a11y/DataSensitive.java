package dev.agentroid.helper.a11y;

import android.os.Build;
import android.view.View;

import dev.agentroid.helper.core.ApiGate;

/**
 * Marks a view accessibilityDataSensitive, on API 34+ only (the attribute does not exist before Android 14).
 * The decision lives in core's {@link ApiGate} so it is unit-tested with fake SDK ints. Use this for any view the
 * helper owns that shows pairing or status information (the 2.15 overlay will); the app's own MainActivity applies
 * the same gate without depending on this module.
 */
public final class DataSensitive {
    private DataSensitive() { }

    public static boolean mark(final View v) {
        return ApiGate.applyDataSensitive(Build.VERSION.SDK_INT, new ApiGate.DataSensitiveSink() {
            @Override public void setDataSensitive(boolean sensitive) {
                v.setAccessibilityDataSensitive(sensitive ? View.ACCESSIBILITY_DATA_SENSITIVE_YES : View.ACCESSIBILITY_DATA_SENSITIVE_NO);
            }
        });
    }
}
