package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Clock;

import java.util.PriorityQueue;

/** Virtual time with the core Clock contract: one thread, due-time order, FIFO for equal times. */
final class FakeClock implements Clock {
    private static final class T implements Task, Comparable<T> {
        final long due, seq;
        final Runnable r;
        boolean cancelled;
        T(long due, long seq, Runnable r) { this.due = due; this.seq = seq; this.r = r; }
        public void cancel() { cancelled = true; }
        public int compareTo(T o) { return due != o.due ? Long.compare(due, o.due) : Long.compare(seq, o.seq); }
    }

    private final PriorityQueue<T> q = new PriorityQueue<T>();
    private final long wallBase;
    private long now;
    private long seq;
    long tasksRun;

    FakeClock(long wallBaseMs) { this.wallBase = wallBaseMs; }

    public long nowMs() { return now; }

    public long wallMs() { return wallBase + now; }

    public Task schedule(long delayMs, Runnable r) {
        T t = new T(now + Math.max(0, delayMs), seq++, r);
        q.add(t);
        return t;
    }

    /** Run every task due up to and including t, then set the time to t. */
    void advanceTo(long t) {
        while (!q.isEmpty() && q.peek().due <= t) {
            T x = q.poll();
            now = Math.max(now, x.due);
            if (!x.cancelled) { tasksRun++; x.r.run(); }
        }
        now = Math.max(now, t);
    }

    void advance(long ms) { advanceTo(now + ms); }

    /** Run until nothing is queued (bounded). */
    void runIdle(long maxMs) {
        long end = now + maxMs;
        while (!q.isEmpty() && q.peek().due <= end) advanceTo(q.peek().due);
    }

    int queued() { return q.size(); }
}
