package dev.agentroid.helper.link.net;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.Random;

import dev.agentroid.helper.core.ErrorCodes;

/** The 2.12 measured row, run in-process: 10,000 requests with 20 forced drops, 0 lost and 0 duplicated. */
public class LinkTest {
    static final String SERIAL = "POCOX6TEST01";

    static FakeHub hub(FakeClock clock, long epoch) { return new FakeHub(clock, 11, epoch, SERIAL); }

    static void readyUp(FakeClock clock, final Phone p) {
        p.start();
        clock.runUntil(new FakeClock.Until() { public boolean done() { return p.client.phase() == LinkClient.Phase.READY; } }, 60000, "ready");
    }

    public void testTenThousandRequestsTwentyDropsZeroLostZeroDuplicated() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        hub.recordFrames = false;
        final Phone p = new Phone(clock, hub, 21);
        final int n = 10000, drops = 20;
        for (int i = 0; i < n; i++) hub.enqueue("req-" + i, "act.count", "{\"n\":" + i + "}");

        // 20 drop points, spread at random over the hub's received frames (so some land mid-handshake, some mid-burst)
        Random r = new Random(2012);
        final List<Integer> at = new ArrayList<Integer>();
        while (at.size() < drops) {
            int x = 50 + r.nextInt(9500); // the hub handles about 10,000 responses plus handshakes and heartbeats
            if (!at.contains(x)) at.add(x);
        }
        Collections.sort(at);
        final int[] frames = {0};
        hub.afterReceive = new Runnable() { public void run() {
            frames[0]++;
            if (hub.drops < drops && frames[0] >= at.get(hub.drops)) hub.drop();
        } };

        long t0 = System.nanoTime();
        p.start();
        clock.runUntil(new FakeClock.Until() { public boolean done() { return hub.responses.size() == n; } }, 3600000L, "10,000 responses");
        long wallMs = (System.nanoTime() - t0) / 1000000L;

        A.eq(drops, hub.drops, "forced drops");
        int lost = 0, duplicated = 0;
        for (int i = 0; i < n; i++) {
            Integer runs = p.runs.get(String.valueOf(i));
            if (runs == null) lost++;
            else if (runs > 1) duplicated += runs - 1;
            if (!hub.responses.containsKey("req-" + i)) lost++;
        }
        A.eq(0, lost, "lost (not run, or no response reached the hub)");
        A.eq(0, duplicated, "duplicated executions on the phone");
        A.eq(0, hub.appDuplicates, "duplicate responses handed to the hub application");
        A.eq(n, p.totalRuns(), "total executions");
        A.eq(n, p.client.executed(), "LinkClient executions");
        for (Object rpc : hub.responses.values()) {
            A.ok(((Map<?, ?>) rpc).containsKey("result"), "every response is a result: " + rpc);
        }
        // the drops really lost frames in flight, and the resume path recovered them (otherwise this test proves little)
        A.ok(hub.resentRequests > 0, "the hub had to resend requests the phone never received");
        A.ok(p.client.resent() > 0, "the phone had to resend responses the hub never received");
        System.out.println("  link: " + n + " requests, " + hub.drops + " drops, " + hub.readies + " handshakes, lost 0, duplicated 0; recovered by resume: "
                + hub.resentRequests + " requests and " + p.client.resent() + " responses resent; " + hub.wireDuplicates
                + " wire duplicates discarded by seq; virtual " + (clock.nowMs() - 1000000) / 1000 + " s, wall " + wallMs + " ms");
    }

    public void testReplayedRequestIdIsAnsweredFromCacheNeverRerun() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        final Phone p = new Phone(clock, hub, 21);
        readyUp(clock, p);
        hub.enqueue("same-id", "act.count", "{\"n\":1}");
        hub.pumpNow();
        clock.runFor(100);
        A.eq(1, p.runs.get("1").intValue(), "first delivery runs");
        // the hub re-issues the same id under a NEW seq (as a hub that lost its own state would)
        for (int k = 0; k < 5; k++) hub.enqueue("same-id", "act.count", "{\"n\":1}");
        hub.pumpNow();
        clock.runFor(100);
        A.eq(1, p.runs.get("1").intValue(), "replays are never re-run");
        A.eq(5, hub.replayedSeen, "five responses marked replayed=true");
        A.eq(5, p.client.replayed(), "client counted five replays");
    }

    public void testATransientAnswerIsNotRememberedSoTheSameIdRunsAgain() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        final Phone p = new Phone(clock, hub, 21);
        readyUp(clock, p);
        hub.enqueue("flaky-1", "act.flaky", "{\"n\":9}");
        hub.pumpNow();
        clock.runFor(100);
        A.eq(1, p.runs.get("9").intValue(), "first delivery runs and is told to try again");
        // the hub tries the same id again (the segment runner was busy, a report was not ready): it must RUN, not be
        // answered from the phone's id memory or the link's response cache
        hub.enqueue("flaky-1", "act.flaky", "{\"n\":9}");
        hub.pumpNow();
        clock.runFor(100);
        A.eq(2, p.runs.get("9").intValue(), "the second delivery ran: a transient answer pins nothing");
        A.eq(0, hub.replayedSeen, "and nothing was answered from a cache");
        List<Map<String, Object>> answers = new ArrayList<Map<String, Object>>();
        for (Map<String, Object> f : hub.phoneFrames("response")) if ("flaky-1".equals(f.get("id"))) answers.add(f);
        A.eq(2, answers.size(), "two answers for the id");
        A.eq(Long.valueOf(ErrorCodes.TRANSIENT), ((Map<?, ?>) ((Map<?, ?>) answers.get(0).get("rpc")).get("error")).get("code"), "the first was transient");
        A.ok(((Map<?, ?>) answers.get(1).get("rpc")).get("result") != null, "the second is a result");
    }

    public void testReusedIdWithDifferentCallIsRefusedNotRun() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        final Phone p = new Phone(clock, hub, 21);
        readyUp(clock, p);
        hub.enqueue("x1", "act.count", "{\"n\":1}");
        hub.enqueue("x1", "act.count", "{\"n\":2}");
        hub.pumpNow();
        clock.runFor(100);
        A.eq(null, p.runs.get("2"), "the second call under a used id did not run");
        A.eq(1, p.totalRuns(), "one execution in total");
        long dupErrors = 0;
        for (Map<String, Object> f : hub.phoneFrames("response")) {
            Object err = ((Map<?, ?>) f.get("rpc")).get("error");
            if (err != null && Long.valueOf(ErrorCodes.INVALID_REQUEST).equals(((Map<?, ?>) err).get("code"))) dupErrors++;
        }
        A.eq(1, dupErrors, "one 'id already used' error response");
    }

    public void testDuplicateDeliveryOfTheSameSeqIsOnlyAcked() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        final Phone p = new Phone(clock, hub, 21);
        readyUp(clock, p);
        String f = FakeHub.requestFrame(3, 1, "d1", "act.count", "{\"n\":7}");
        for (int k = 0; k < 4; k++) hub.raw(f);
        clock.runFor(100);
        A.eq(1, p.runs.get("7").intValue(), "same seq four times runs once");
        A.eq(1, p.client.rxAcked(), "ack is 1");
    }

    public void testSeqGapIsRefusedAndAsksForResume() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        final Phone p = new Phone(clock, hub, 21);
        readyUp(clock, p);
        hub.raw(FakeHub.requestFrame(3, 5, "g5", "act.count", "{\"n\":5}"));
        clock.runFor(100);
        A.eq(null, p.runs.get("5"), "seq 5 with ack 0 is not run");
        A.eq(1, hub.refusals.get("out_of_order").intValue(), "refused out_of_order");
        A.ok(!hub.phoneFrames("resume").isEmpty(), "phone asked for a resume");
    }

    public void testRequestsBeforeReadyAreRefusedNotRun() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        final Phone p = new Phone(clock, hub, 21);
        hub.phoneKey = null; // the hub refuses the phone: never ready
        p.start();
        clock.runFor(2); // connected, hello sent, challenge on the wire
        hub.raw(FakeHub.requestFrame(3, 1, "early", "act.count", "{\"n\":1}"));
        clock.runFor(50);
        A.eq(0, p.totalRuns(), "nothing runs before ready");
        A.eq(1, hub.refusals.get("not_ready").intValue(), "refused not_ready");
    }

    public void testOutboxIsResentAfterADropBeforeTheAck() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = hub(clock, 3);
        hub.ackEvery = 1000000; // the hub never acks on its own
        final Phone p = new Phone(clock, hub, 21);
        readyUp(clock, p);
        for (int i = 0; i < 10; i++) hub.enqueue("o" + i, "act.count", "{\"n\":" + i + "}");
        hub.pumpNow();
        clock.runFor(1); // the phone has run all ten and sent the responses, which are still on the wire
        A.eq(10, p.totalRuns(), "ten runs");
        A.eq(0, hub.responses.size(), "no response has reached the hub yet");
        A.eq(10, p.client.outboxSize(), "ten responses wait in the outbox");
        hub.drop(); // they are lost in flight
        clock.runUntil(new FakeClock.Until() { public boolean done() { return hub.readies == 2 && hub.responses.size() == 10; } }, 60000, "resent responses");
        A.eq(10, hub.responses.size(), "all ten responses arrived after resume");
        A.eq(10, p.totalRuns(), "and nothing ran twice");
        A.eq(0, hub.appDuplicates, "no duplicate reached the hub application");
    }
}
