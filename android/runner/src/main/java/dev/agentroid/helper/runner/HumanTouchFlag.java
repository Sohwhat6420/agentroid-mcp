package dev.agentroid.helper.runner;

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

/**
 * The writable HumanTouch flag. The 2.15 accessibility overlay calls touched(now) with
 * SystemClock.elapsedRealtime() on a qualifying touchscreen ACTION_OUTSIDE; the runner reads lastTouchMs() in the
 * same domain. Thread-safe: the overlay writes on the main thread, the runner reads on its Clock thread.
 *
 * The flag only moves forward: a late or out-of-order report never hides a newer touch.
 */
public final class HumanTouchFlag implements HumanTouch {
    private final AtomicLong last = new AtomicLong(NEVER);
    private final AtomicReference<Runnable> listener = new AtomicReference<Runnable>();

    @Override
    public long lastTouchMs() {
        return last.get();
    }

    @Override
    public void setListener(Runnable onTouch) {
        listener.set(onTouch);
    }

    /** Record a human touch at nowMs (elapsedRealtime) and push it to the listener, if any. */
    public void touched(long nowMs) {
        long prev;
        do {
            prev = last.get();
            if (nowMs <= prev) break;
        } while (!last.compareAndSet(prev, nowMs));
        Runnable r = listener.get();
        if (r != null) r.run();
    }
}
