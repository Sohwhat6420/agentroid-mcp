package dev.agentroid.helper.overlay.logic;

/**
 * The parts of an android.view.MotionEvent the classifier reads, copied out so the logic runs on a plain JVM.
 * Every field is something the platform exposes to the overlay's window for an ACTION_OUTSIDE (or ACTION_DOWN on the
 * dot itself). Coordinates are not used: for touches on another app's window ACTION_OUTSIDE carries (0, 0).
 */
public final class TouchSample {
    /** MotionEvent.getActionMasked(). */
    public final int action;
    /** MotionEvent.getSource(). */
    public final int source;
    /** MotionEvent.getDeviceId(). */
    public final int deviceId;
    /** MotionEvent.getFlags(). */
    public final int flags;
    /** MotionEvent.getToolType(0). */
    public final int toolType;
    /** MotionEvent.getEventTime(), uptime milliseconds. */
    public final long eventTimeMs;

    public TouchSample(int action, int source, int deviceId, int flags, int toolType, long eventTimeMs) {
        this.action = action;
        this.source = source;
        this.deviceId = deviceId;
        this.flags = flags;
        this.toolType = toolType;
        this.eventTimeMs = eventTimeMs;
    }

    @Override public String toString() {
        return "TouchSample{action=" + action + ", source=0x" + Integer.toHexString(source) + ", deviceId=" + deviceId
                + ", flags=0x" + Integer.toHexString(flags) + ", toolType=" + toolType + ", t=" + eventTimeMs + "}";
    }
}
