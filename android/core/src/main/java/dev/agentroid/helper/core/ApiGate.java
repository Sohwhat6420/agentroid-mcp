package dev.agentroid.helper.core;

/**
 * API-level gates as pure functions, so a fake SDK int can be tested on the JVM. The one gate today:
 * View.setAccessibilityDataSensitive exists from API 34 (Android 14); on older releases it must not be touched.
 */
public final class ApiGate {
    private ApiGate() { }

    public static final int DATA_SENSITIVE_MIN_SDK = 34;

    /** Receives the call that the real code turns into View.setAccessibilityDataSensitive(...). */
    public interface DataSensitiveSink {
        void setDataSensitive(boolean sensitive);
    }

    public static boolean dataSensitiveSupported(int sdkInt) { return sdkInt >= DATA_SENSITIVE_MIN_SDK; }

    /** Calls the sink (with true) only when sdkInt is 34 or more. Returns whether it did. */
    public static boolean applyDataSensitive(int sdkInt, DataSensitiveSink sink) {
        if (!dataSensitiveSupported(sdkInt)) return false;
        sink.setDataSensitive(true);
        return true;
    }
}
