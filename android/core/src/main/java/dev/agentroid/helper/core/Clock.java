package dev.agentroid.helper.core;

/**
 * Time and scheduling behind one seam (task 2.12), so the link's heartbeat, backoff and liveness timers and the
 * keeper's 15-minute tick run on virtual time in the JVM tests. Contract: every task runs on ONE thread, in due-time
 * order (FIFO for equal times), so code driven by a Clock needs no locks of its own.
 */
public interface Clock {
    interface Task { void cancel(); }

    /** Monotonic milliseconds (not wall time). */
    long nowMs();

    /** Wall-clock Unix milliseconds, for timestamps sent to the hub only. */
    long wallMs();

    /** Run r once, after delayMs (0 = as soon as the clock's thread is free). */
    Task schedule(long delayMs, Runnable r);
}
