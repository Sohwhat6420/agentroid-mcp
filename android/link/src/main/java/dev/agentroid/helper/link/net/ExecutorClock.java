package dev.agentroid.helper.link.net;

import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.TimeUnit;

import dev.agentroid.helper.core.Clock;

/**
 * The production Clock: one daemon thread, tasks in due-time order. The link's only thread; requests run on it too.
 * Note for the phone run: a ScheduledExecutor's delays use System.nanoTime, which (like the Handler's uptime) does
 * not advance in deep sleep, so a Doze night stretches heartbeat gaps; the hub's offline rule allows for this.
 */
public final class ExecutorClock implements Clock {
    private final ScheduledExecutorService ex;

    public ExecutorClock(final String name) {
        ScheduledThreadPoolExecutor e = new ScheduledThreadPoolExecutor(1, new ThreadFactory() {
            public Thread newThread(Runnable r) {
                Thread t = new Thread(r, name);
                t.setDaemon(true);
                return t;
            }
        });
        e.setRemoveOnCancelPolicy(true);
        ex = e;
    }

    @Override public long nowMs() { return System.nanoTime() / 1000000L; }

    @Override public long wallMs() { return System.currentTimeMillis(); }

    @Override
    public Task schedule(long delayMs, final Runnable r) {
        final ScheduledFuture<?> f;
        try {
            f = ex.schedule(new Runnable() {
                public void run() {
                    try {
                        r.run();
                    } catch (RuntimeException ignored) {
                        // one bad task must not kill the link thread
                    }
                }
            }, Math.max(0, delayMs), TimeUnit.MILLISECONDS);
        } catch (java.util.concurrent.RejectedExecutionException e) {
            return new Task() { public void cancel() { } };
        }
        return new Task() { public void cancel() { f.cancel(false); } };
    }

    public void shutdown() { ex.shutdownNow(); }
}
