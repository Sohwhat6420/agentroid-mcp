package dev.agentroid.helper.overlay.logic;

/**
 * Decides whether one touch the overlay saw was a human on the physical touchscreen.
 *
 * <p>What the platform exposes, and what each signal distinguishes:
 * <ul>
 * <li>{@code FLAG_IS_ACCESSIBILITY_EVENT} (0x800, API 30+; hidden constant, readable through getFlags()): set on events
 *     an accessibility service injected, so every {@code dispatchGesture} (ours, and the 2.14 on-phone runner's).</li>
 * <li>{@code FLAG_IS_GENERATED_GESTURE} (0x8, API 29+; hidden constant): set by the accessibility gesture injector.</li>
 * <li>{@code getDeviceId()}: 0 for events injected without a device (scrcpy's injectInputEvent path, older
 *     {@code input}), -1 for the virtual device; a uhid/uinput device has its own positive id, which is never in the
 *     {@link TouchscreenRegistry}.</li>
 * <li>{@code isFromSource(SOURCE_TOUCHSCREEN)}: rules out mouse, keyboard and joystick events.</li>
 * </ul>
 * <b>Not distinguishable on the phone:</b> {@code input tap}/{@code input swipe} on builds whose shell picks the first
 * device supporting the source, which is the real touchscreen's id. Those events look exactly like a finger. They need
 * adb, so with adb down they cannot happen; with adb up the hub knows when it sent them and Detector 1 (getevent)
 * never sees them. On API 29 an accessibility gesture may carry neither flag; then events inside our own gesture's
 * time window ({@link #noteOwnGesture}) are taken as ours, at the cost of missing a human touch in that window.
 */
public final class OverlayClassifier {
    public static final int ACTION_DOWN = 0;
    public static final int ACTION_OUTSIDE = 4;
    public static final int SOURCE_TOUCHSCREEN = TouchscreenRegistry.SOURCE_TOUCHSCREEN;
    public static final int FLAG_IS_GENERATED_GESTURE = 0x8;
    public static final int FLAG_IS_ACCESSIBILITY_EVENT = 0x800;
    /** Slack after our own gesture ends, for the API 29 window. */
    public static final long OWN_GESTURE_SLACK_MS = 50;

    public enum Verdict {
        HUMAN,
        NOT_A_DOWN,
        NOT_TOUCHSCREEN_SOURCE,
        ACCESSIBILITY_INJECTED,
        GENERATED_GESTURE,
        NO_DEVICE,
        NOT_THE_TOUCHSCREEN,
        OWN_GESTURE_WINDOW
    }

    private final TouchscreenRegistry registry;
    private final int sdk;
    private long ownStart = -1, ownEnd = -1;

    public OverlayClassifier(TouchscreenRegistry registry, int sdk) {
        this.registry = registry;
        this.sdk = sdk;
    }

    /** The helper is about to dispatch a gesture of this duration (uptime ms). */
    public synchronized void noteOwnGesture(long startMs, long durationMs) {
        ownStart = startMs;
        ownEnd = startMs + Math.max(0, durationMs) + OWN_GESTURE_SLACK_MS;
    }

    public synchronized Verdict classify(TouchSample s) {
        if (s.action != ACTION_OUTSIDE && s.action != ACTION_DOWN) return Verdict.NOT_A_DOWN;
        if ((s.source & SOURCE_TOUCHSCREEN) != SOURCE_TOUCHSCREEN) return Verdict.NOT_TOUCHSCREEN_SOURCE;
        if ((s.flags & FLAG_IS_ACCESSIBILITY_EVENT) != 0) return Verdict.ACCESSIBILITY_INJECTED;
        if ((s.flags & FLAG_IS_GENERATED_GESTURE) != 0) return Verdict.GENERATED_GESTURE;
        if (s.deviceId <= 0) return Verdict.NO_DEVICE;
        if (!registry.qualifies(s.deviceId)) return Verdict.NOT_THE_TOUCHSCREEN;
        if (sdk < 30 && ownStart >= 0 && s.eventTimeMs >= ownStart && s.eventTimeMs <= ownEnd) return Verdict.OWN_GESTURE_WINDOW;
        return Verdict.HUMAN;
    }
}
