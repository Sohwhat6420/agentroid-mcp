package dev.agentroid.helper.runner;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

/** Post-conditions, unique selectors, expiry and epoch changes mid-run, and one segment at a time. */
public final class RunnerBehaviourTest {
    public void testAStepWaitsForItsPostConditionAndReportsIt() throws Exception {
        Rig r = new Rig(true, false, 1_200);
        r.run(r.signed("seg", "R1", Arrays.asList(TestHub.read("s1"), TestHub.tap("s2"), TestHub.toggle("s3", "accelerometer_rotation", "0"))), 60_000);
        A.eq("completed", r.done.outcome, "outcome");
        List<Map<String, Object>> steps = r.reportSteps();
        A.eq("text of {\"text\":\"Battery s1\"}", steps.get(0).get("value"), "the read value travels in the report");
        long ms = ((Long) steps.get(1).get("ms")).longValue();
        A.ok(ms >= 1_200 && ms <= 1_250, "the tap waited for its post-condition: " + ms + " ms");
        A.eq(1, r.transport.received.size(), "report sent at once while the link is up");
        A.ok(r.journal.count("started") == 4 && r.journal.count("done") == 3, "journal: segment start + 3 step starts, 3 dones: " + r.journal.records);
    }

    public void testAPostConditionThatNeverHoldsFailsTheSegment() throws Exception {
        Rig r = new Rig(true, false, 10_000);
        r.run(r.signed("seg", "R1", Arrays.asList(TestHub.tap("s1"), TestHub.tap("s2"))), 60_000);
        A.eq("failed", r.done.outcome, "outcome");
        A.eq(1, r.exec.acts.size(), "the second tap never ran");
        A.eq(Boolean.FALSE, r.reportSteps().get(0).get("post_verified"), "not verified");
    }

    public void testATapThatMatchesTwoNodesIsNotRun() throws Exception {
        Rig r = new Rig(true, false, 0);
        r.exec.counts.put("{\"id\":\"com.android.settings:id/item_s1\"}", 2);
        r.run(r.signed("seg", "R1", Arrays.asList(TestHub.tap("s1"))), 60_000);
        A.eq("failed", r.done.outcome, "not unique");
        A.eq(0, r.exec.acts.size(), "never tapped");
    }

    public void testASegmentThatExpiresMidRunStops() throws Exception {
        Rig r = new Rig(true, false, 0);
        List<Map<String, Object>> steps = new ArrayList<Map<String, Object>>();
        for (int i = 1; i <= 10; i++) steps.add(TestHub.waitMs("s" + i, 60_000));
        r.run(r.hub.sign(r.hub.segment("seg", "R0", Rig.NOW + 5 * 60_000L, false, steps)), 20 * 60_000L);
        A.eq("expired", r.done.outcome, "outcome");
        List<Map<String, Object>> rep = r.reportSteps();
        A.eq(5, rep.size(), "4 waits done, the 5th reaches the expiry");
        A.eq("done", rep.get(3).get("status"), "4th done");
        A.eq("interrupted", rep.get(4).get("status"), "5th interrupted at the expiry, never reported done after it");
        A.eq("expired", rep.get(4).get("reason"), "reason");
    }

    public void testANewerHubEpochStopsARunningSegment() throws Exception {
        final Rig r = new Rig(true, false, 0);
        r.runner.submit(r.signed("seg", "R0", Arrays.asList(TestHub.waitMs("s1", 10_000), TestHub.waitMs("s2", 10_000), TestHub.waitMs("s3", 10_000))));
        r.clock.schedule(15_000, new Runnable() { public void run() { r.trust.epoch = Rig.EPOCH + 1; } });
        r.clock.runIdle(60_000);
        A.eq("stale_epoch", r.done.outcome, "fenced by the new epoch");
        List<Map<String, Object>> rep = r.reportSteps();
        A.eq(2, rep.size(), "s1 done, s2 interrupted");
        A.eq("done", rep.get(0).get("status"), "s1");
        A.eq("interrupted", rep.get(1).get("status"), "s2 stopped mid-wait");
        A.ok(r.done.at - 15_000 <= SegmentRunner.TOUCH_POLL_MS, "fenced within one poll: " + (r.done.at - 15_000));
    }

    public void testOneSegmentAtATime() throws Exception {
        Rig r = new Rig(true, false, 0);
        r.runner.submit(r.signed("seg-a", "R0", Arrays.asList(TestHub.waitMs("s1", 10_000))));
        A.ok(r.runner.busy(), "running");
        Fakes.Done first = r.done;
        r.runner.submit(r.signed("seg-b", "R0", Arrays.asList(TestHub.read("s1"))));
        A.eq("busy", first.outcome, "second refused while busy");
        r.clock.runIdle(60_000);
        A.eq("completed", r.done.outcome, "the first one still completes");
    }
}
