package dev.agentroid.helper.keeper.logic;

import java.util.PriorityQueue;

import dev.agentroid.helper.core.Clock;

/** Virtual time for the keeper tests (kept inside :keeper so its tests compile on their own under Gradle too). */
final class TickClock implements Clock {
    private static final class T implements Task, Comparable<T> {
        final long due, order;
        final Runnable r;
        boolean cancelled;
        T(long due, long order, Runnable r) { this.due = due; this.order = order; this.r = r; }
        public void cancel() { cancelled = true; }
        public int compareTo(T o) { return due != o.due ? Long.compare(due, o.due) : Long.compare(order, o.order); }
    }

    private final PriorityQueue<T> q = new PriorityQueue<T>();
    private long now, order;

    public long nowMs() { return now; }
    public long wallMs() { return 1760000000000L + now; }

    public Task schedule(long delayMs, Runnable r) {
        T t = new T(now + Math.max(0, delayMs), order++, r);
        q.add(t);
        return t;
    }

    void runFor(long ms) {
        long end = now + ms;
        while (!q.isEmpty() && q.peek().due <= end) {
            T t = q.poll();
            if (t.cancelled) continue;
            now = t.due;
            t.r.run();
        }
        now = end;
    }
}
