package dev.agentroid.helper.runner;

/**
 * The local human_touch flag (task 2.15 supplies it: the alpha-0 accessibility overlay writes it on a qualifying
 * touchscreen ACTION_OUTSIDE, with adb down too). 2.14 only reads it.
 *
 * Contract: lastTouchMs is in the runner Clock's nowMs() domain (SystemClock.elapsedRealtime on the phone); the
 * listener, if the flag can push, may be called on any thread and the runner re-posts it to its Clock thread. The
 * runner polls as well, so a flag without a listener still stops a segment within the poll period.
 */
public interface HumanTouch {
    long NEVER = Long.MIN_VALUE;

    /** Time of the latest human touch, or NEVER. */
    long lastTouchMs();

    /** Register (or clear with null) a push hook fired on each touch. */
    void setListener(Runnable onTouch);
}
