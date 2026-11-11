package dev.agentroid.helper.link.net;

import java.util.PriorityQueue;

import dev.agentroid.helper.core.Clock;

/**
 * Virtual time for the tests: tasks run in (due time, insertion order), on the caller's thread, and time jumps straight
 * to the next due task, so an hour of heartbeats and backoff runs in milliseconds. Shared by the link and keeper tests.
 */
public final class FakeClock implements Clock {
    private static final class T implements Task, Comparable<T> {
        final long due, order;
        final Runnable r;
        boolean cancelled;
        T(long due, long order, Runnable r) { this.due = due; this.order = order; this.r = r; }
        public void cancel() { cancelled = true; }
        public int compareTo(T o) { return due != o.due ? Long.compare(due, o.due) : Long.compare(order, o.order); }
    }

    public interface Until { boolean done(); }

    private final PriorityQueue<T> q = new PriorityQueue<T>();
    private long now = 1000000;
    private long order;
    private long ran;

    @Override public long nowMs() { return now; }

    @Override public long wallMs() { return 1760000000000L + now; }

    @Override
    public Task schedule(long delayMs, Runnable r) {
        T t = new T(now + Math.max(0, delayMs), order++, r);
        q.add(t);
        return t;
    }

    public long ran() { return ran; }

    /** Run everything due within the next ms milliseconds, then set the time to now + ms. */
    public void runFor(long ms) {
        long end = now + ms;
        while (true) {
            T t = q.peek();
            if (t == null || t.due > end) break;
            q.poll();
            if (t.cancelled) continue;
            now = Math.max(now, t.due);
            ran++;
            t.r.run();
        }
        now = end;
    }

    /** Run tasks until the condition holds; fails if it does not within maxMs of virtual time. */
    public void runUntil(Until u, long maxMs, String what) {
        long end = now + maxMs;
        while (!u.done()) {
            T t = q.peek();
            if (t == null) throw new AssertionError("nothing left to run, still waiting for: " + what);
            if (t.due > end) throw new AssertionError("virtual timeout (" + maxMs + " ms) waiting for: " + what);
            q.poll();
            if (t.cancelled) continue;
            now = Math.max(now, t.due);
            ran++;
            t.r.run();
        }
    }
}
