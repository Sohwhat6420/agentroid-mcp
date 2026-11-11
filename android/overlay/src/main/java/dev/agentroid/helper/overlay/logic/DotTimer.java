package dev.agentroid.helper.overlay.logic;

/**
 * The corner dot's 10 s reclaim warning (round 31, "Tiny dot in a corner"). The hub starts it (ShowDot) and the dot
 * shows until the warning runs out, the hub hides it, or a human touch cancels it locally. The phone cancels on its
 * own touch at once, without waiting for the hub, and reports the touch as usual. Times are uptime milliseconds.
 */
public final class DotTimer {
    public static final long WARN_MS = 10_000;

    private long until = -1;
    private boolean cancelledByTouch;

    /** Start (or restart) the warning at {@code nowMs}; returns the time it runs out. */
    public synchronized long start(long nowMs) {
        until = nowMs + WARN_MS;
        cancelledByTouch = false;
        return until;
    }

    /** The hub hid it (reclaim done or cancelled hub-side). */
    public synchronized void hide() {
        until = -1;
    }

    /** A local human touch: cancel. Returns true when a running warning was cancelled. */
    public synchronized boolean onHumanTouch(long nowMs) {
        if (visible(nowMs)) {
            until = -1;
            cancelledByTouch = true;
            return true;
        }
        return false;
    }

    public synchronized boolean visible(long nowMs) {
        return until >= 0 && nowMs < until;
    }

    /** Ran its full 10 s without a touch or a hide. */
    public synchronized boolean expired(long nowMs) {
        return until >= 0 && nowMs >= until;
    }

    public synchronized long remainingMs(long nowMs) {
        return visible(nowMs) ? until - nowMs : 0;
    }

    public synchronized boolean cancelledByTouch() {
        return cancelledByTouch;
    }
}
