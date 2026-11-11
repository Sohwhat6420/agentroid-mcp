package dev.agentroid.helper.link.net;

import dev.agentroid.helper.core.LinkSession;

/** Split brain: 1,000 stale-epoch commands, 0 executed. Plus the other fencing edges. */
public class FencingTest {

    public void testThousandStaleEpochCommandsZeroExecuted() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 7);
        final Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        for (int i = 0; i < 1000; i++) {
            // an older hub (epoch 6) whose commands still reach the phone, with seqs that would otherwise be next
            hub.raw(FakeHub.requestFrame(6, 1 + i, "old-" + i, "act.count", "{\"n\":" + i + "}"));
        }
        clock.runFor(1000);
        A.eq(0, p.totalRuns(), "stale-epoch commands executed");
        A.eq(1000, hub.refusals.get("stale_epoch").intValue(), "refused stale_epoch");
        A.eq(1000, p.client.refusedStale(), "client counted the refusals");
        A.eq(0, p.client.rxAcked(), "the stream did not move");
        // the current hub's stream is undisturbed: its seq 1 still runs
        hub.enqueue("new-1", "act.count", "{\"n\":\"current\"}");
        hub.pumpNow();
        clock.runFor(100);
        A.eq(1, p.runs.get("current").intValue(), "the current epoch still runs");
        System.out.println("  fencing: 1000 stale-epoch commands, 0 executed, 1000 refused stale_epoch");
    }

    public void testOlderHubCannotTakeThePhoneBack() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 7);
        final Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        A.eq(7, p.trust.maxEpoch(), "epoch 7 remembered");
        hub.epoch = 6; // the old hub answers the next connection
        hub.drop();
        clock.runUntil(new FakeClock.Until() { public boolean done() { return !hub.repairs.isEmpty(); } }, 60000, "needs_repair");
        A.eq(LinkSession.State.NEEDS_REPAIR, p.session.state(), "needs_repair");
        A.eq("stale_epoch", p.session.failure(), "reason");
        A.eq(LinkProto.PHONE_FIX.get("stale_epoch"), p.session.fix(), "exact fix text");
        A.eq(LinkProto.PHONE_FIX.get("stale_epoch"), hub.repairs.get(0).get("fix"), "fix text sent to the hub");
        A.ok(p.client.phase() != LinkClient.Phase.READY, "not ready");
        A.eq(0, p.totalRuns(), "nothing ran");
    }

    public void testEpochAheadIsRefusedUntilReChallenged() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 7);
        final Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        hub.raw(FakeHub.requestFrame(8, 1, "ahead", "act.count", "{\"n\":1}"));
        clock.runFor(100);
        A.eq(0, p.totalRuns(), "not run");
        A.eq(1, hub.refusals.get("epoch_ahead").intValue(), "refused epoch_ahead");
    }

    public void testNewerEpochTakesOverAndStartsAFreshStream() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 7);
        final Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        hub.enqueue("a", "act.count", "{\"n\":1}");
        hub.pumpNow();
        clock.runFor(100);
        A.eq(1, p.client.rxAcked(), "epoch 7 stream at 1");
        hub.epoch = 8;
        hub.txSeq = 0;
        hub.rxAcked = 0;
        hub.outbox.clear();
        hub.drop();
        clock.runUntil(new FakeClock.Until() { public boolean done() { return p.client.epoch() == 8 && p.client.phase() == LinkClient.Phase.READY; } }, 60000, "epoch 8 ready");
        A.eq(8, p.trust.maxEpoch(), "epoch 8 remembered");
        A.eq(0, p.client.rxAcked(), "fresh stream");
        hub.raw(FakeHub.requestFrame(7, 1, "late", "act.count", "{\"n\":2}"));
        clock.runFor(100);
        A.eq(null, p.runs.get("2"), "epoch 7 is now stale");
    }
}
