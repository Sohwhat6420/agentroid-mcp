package dev.agentroid.helper.overlay.logic;

import static dev.agentroid.helper.overlay.logic.OverlayTestRunner.eq;
import static dev.agentroid.helper.overlay.logic.OverlayTestRunner.ok;

public class DotTimerTest {
    public void testShowsForExactlyTenSeconds() {
        DotTimer d = new DotTimer();
        ok(!d.visible(0), "hidden before start");
        long until = d.start(50_000);
        eq(60_000L, until, "runs out 10 s later");
        ok(d.visible(50_000), "visible at start");
        ok(d.visible(59_999), "visible 1 ms before the end");
        eq(1L, d.remainingMs(59_999), "remaining");
        ok(!d.visible(60_000), "hidden at 10 s");
        ok(d.expired(60_000), "expired at 10 s");
        ok(!d.expired(59_999), "not expired before");
    }

    public void testATouchCancelsTheWarningTenOfTen() {
        for (int i = 0; i < 10; i++) {
            DotTimer d = new DotTimer();
            d.start(1_000);
            long touch = 1_000 + 500 + i * 950L;
            ok(d.onHumanTouch(touch), "run " + i + " cancelled");
            ok(!d.visible(touch), "hidden");
            ok(!d.expired(20_000), "a cancelled warning never expires into a reclaim");
            ok(d.cancelledByTouch(), "marked");
        }
    }

    public void testATouchOutsideTheWarningChangesNothing() {
        DotTimer d = new DotTimer();
        ok(!d.onHumanTouch(5), "no warning running");
        d.start(0);
        ok(!d.onHumanTouch(10_000), "after it ran out");
        ok(d.expired(10_000), "still expired");
    }

    public void testHubHideAndRestart() {
        DotTimer d = new DotTimer();
        d.start(0);
        d.hide();
        ok(!d.visible(1), "hidden by the hub");
        ok(!d.expired(20_000), "a hidden warning does not expire");
        d.start(30_000);
        ok(d.visible(39_000) && !d.visible(40_000), "restart gives a fresh 10 s");
        ok(!d.cancelledByTouch(), "restart clears the mark");
    }
}
