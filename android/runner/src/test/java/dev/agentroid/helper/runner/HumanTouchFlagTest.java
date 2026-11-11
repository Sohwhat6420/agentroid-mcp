package dev.agentroid.helper.runner;

import java.util.concurrent.atomic.AtomicInteger;

/** The writable flag behind HumanTouch. */
public final class HumanTouchFlagTest {
    public void testStartsAtNeverAndRecordsTheLatestTouch() {
        HumanTouchFlag f = new HumanTouchFlag();
        A.eq(Long.valueOf(HumanTouch.NEVER), Long.valueOf(f.lastTouchMs()), "untouched");
        f.touched(1000);
        A.eq(Long.valueOf(1000), Long.valueOf(f.lastTouchMs()), "first touch");
        f.touched(2500);
        A.eq(Long.valueOf(2500), Long.valueOf(f.lastTouchMs()), "a newer touch");
    }

    public void testALateReportNeverHidesANewerTouch() {
        HumanTouchFlag f = new HumanTouchFlag();
        f.touched(5000);
        f.touched(4000);
        A.eq(Long.valueOf(5000), Long.valueOf(f.lastTouchMs()), "stays at the newest");
    }

    public void testEveryTouchIsPushedToTheListenerUntilCleared() {
        HumanTouchFlag f = new HumanTouchFlag();
        final AtomicInteger n = new AtomicInteger();
        f.touched(1); // no listener: nothing to call, no error
        f.setListener(new Runnable() {
            public void run() { n.incrementAndGet(); }
        });
        f.touched(2);
        f.touched(2); // a repeat is still a touch the runner should hear about
        A.eq(Integer.valueOf(2), Integer.valueOf(n.get()), "pushed twice");
        f.setListener(null);
        f.touched(3);
        A.eq(Integer.valueOf(2), Integer.valueOf(n.get()), "cleared");
    }
}
