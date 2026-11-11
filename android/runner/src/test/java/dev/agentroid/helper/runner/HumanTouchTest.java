package dev.agentroid.helper.runner;

import java.util.List;
import java.util.Map;
import java.util.Random;

/**
 * The 2.15 human_touch flag stops the local segment: the decision fires within 300 ms of the touch (virtual clock) and
 * no step starts after it. 10 of 10, half through the flag's push listener, half by polling alone.
 */
public final class HumanTouchTest {
    @SuppressWarnings("unchecked")
    public void testAHumanTouchStopsTheSegmentTenOfTen() throws Exception {
        Random rnd = new Random(2014);
        int stopped = 0;
        long worst = 0;
        StringBuilder lat = new StringBuilder();
        for (int trial = 0; trial < 10; trial++) {
            Rig r = new Rig(true, false, 300 + rnd.nextInt(1200));
            r.runner.submit(r.signed("seg-touch-" + trial, "R1", LinkDownTest.fiftySteps()));
            long touchAt = 1_000 + (long) rnd.nextInt(200_000);
            boolean push = trial % 2 == 0;
            r.touch.touchAt(r.clock, touchAt, push);
            r.clock.runIdle(10 * 60_000L);

            A.eq("stopped_human", r.done.outcome, "trial " + trial + " (touch at " + touchAt + ", push " + push + ")");
            long latency = r.done.at - touchAt;
            A.ok(latency >= 0 && latency <= 300, "trial " + trial + ": decision " + latency + " ms after the touch");
            for (int i = 0; i < r.exec.actTimes.size(); i++) {
                A.ok(r.exec.actTimes.get(i).longValue() <= touchAt, "trial " + trial + ": step " + r.exec.acts.get(i) + " acted at " + r.exec.actTimes.get(i) + " after the touch at " + touchAt);
            }
            List<Map<String, Object>> steps = r.reportSteps();
            A.ok(steps.size() < 50, "trial " + trial + ": the segment did not run to the end");
            for (Map<String, Object> s : steps) {
                if ("interrupted".equals(s.get("status"))) A.eq(Boolean.FALSE, s.get("post_verified"), "an interrupted step is not claimed verified");
            }
            A.ok(r.journal.count("taken_over") == 1, "trial " + trial + ": taken_over journalled");
            A.eq(null, r.touch.listener, "trial " + trial + ": listener released");
            worst = Math.max(worst, latency);
            lat.append(latency).append(push ? "p " : "q ");
            stopped++;
        }
        A.eq(10, stopped, "stopped");
        System.out.println("  human touch: 10/10 stopped; decision latency (virtual ms, p=push q=poll) " + lat.toString().trim() + "; worst " + worst + " ms");
    }

    public void testATouchBeforeTheSegmentStartsDoesNotStopIt() throws Exception {
        Rig r = new Rig(true, false, 100);
        r.touch.touchAt(r.clock, 0, false);
        r.clock.advance(5_000);
        r.run(r.signed("seg", "R1", java.util.Arrays.asList(TestHub.read("s1"), TestHub.tap("s2"))), 60_000);
        A.eq("completed", r.done.outcome, "an old touch is not a new one");
    }

    public void testATouchDuringALongWaitStopsWithinOnePollWithoutTheListener() throws Exception {
        Rig r = new Rig(true, false, 100);
        r.runner.submit(r.signed("seg", "R1", java.util.Arrays.asList(TestHub.waitMs("s1", 60_000), TestHub.tap("s2"))));
        r.touch.touchAt(r.clock, 12_345, false);
        r.clock.runIdle(120_000);
        A.eq("stopped_human", r.done.outcome, "outcome");
        A.ok(r.done.at - 12_345 <= SegmentRunner.TOUCH_POLL_MS, "polled within " + SegmentRunner.TOUCH_POLL_MS + " ms: " + (r.done.at - 12_345));
        A.eq(0, r.exec.acts.size(), "the tap after the wait never ran");
        A.eq("interrupted", r.reportSteps().get(0).get("status"), "the wait was interrupted");
    }
}
