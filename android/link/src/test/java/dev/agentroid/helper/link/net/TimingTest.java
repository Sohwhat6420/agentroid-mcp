package dev.agentroid.helper.link.net;

import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Random;
import java.util.Set;

import dev.agentroid.helper.core.Backoff;

/** Backoff never above 30 s; the heartbeat period switches between 15 s and 60 s; a silent hub is dropped. */
public class TimingTest {

    public void testBackoffNeverAbove30sAndIsJittered() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 1);
        hub.refuseConnections = true;
        Phone p = new Phone(clock, hub, 21);
        p.start();
        clock.runFor(2 * 3600 * 1000L); // two hours of a hub that is down
        List<Long> d = hub.dialTimes;
        A.ok(d.size() > 200, "kept redialling: " + d.size());
        Set<Long> distinct = new HashSet<Long>();
        long max = 0;
        for (int i = 1; i < d.size(); i++) {
            long gap = d.get(i) - d.get(i - 1);
            max = Math.max(max, gap);
            A.ok(gap <= Backoff.MAX_MS + 2 * hub.latencyMs, "gap " + i + " is " + gap + " ms");
            A.ok(gap >= 500, "gap " + i + " is at least 0.5 s: " + gap);
            if (i > 8) A.ok(gap >= Backoff.MAX_MS / 2, "after the ramp the gap is 15..30 s: " + gap);
            distinct.add(gap);
        }
        A.ok(distinct.size() > 100, "jittered (distinct gaps: " + distinct.size() + ")");
        A.ok(p.client.lastRedialDelayMs() <= Backoff.MAX_MS, "last delay");
        // and the function itself, for every attempt count up to 10,000
        Random r = new Random(5);
        for (int a = 0; a < 10000; a++) A.ok(Backoff.delayMs(a, r) <= Backoff.MAX_MS, "delay at attempt " + a);
        System.out.println("  backoff: " + d.size() + " dials in 2 h, max gap " + max + " ms, " + distinct.size() + " distinct gaps");
    }

    public void testBackoffResetsAfterReady() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 1);
        hub.refuseConnections = true;
        final Phone p = new Phone(clock, hub, 21);
        p.start();
        clock.runFor(10 * 60 * 1000L);
        hub.refuseConnections = false;
        clock.runUntil(new FakeClock.Until() { public boolean done() { return p.client.phase() == LinkClient.Phase.READY; } }, 31000, "ready within one capped backoff");
        hub.drop();
        clock.runFor(10);
        A.ok(p.client.lastRedialDelayMs() <= 1000, "first redial after a good session is quick: " + p.client.lastRedialDelayMs());
    }

    private static long lastGap(FakeHub hub) {
        List<Long> t = hub.heartbeatTimes;
        return t.get(t.size() - 1) - t.get(t.size() - 2);
    }

    private static Object lastPeriod(FakeHub hub) { return hub.heartbeats.get(hub.heartbeats.size() - 1).get("period_s"); }

    public void testHeartbeatPeriodSwitchesBetween15And60Seconds() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 1);
        Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        clock.runFor(5 * 60000L);
        A.eq(60000L, lastGap(hub), "screen off, not charging: 60 s");
        A.eq(Long.valueOf(60), lastPeriod(hub), "period_s 60");

        p.screenOn = true;
        p.client.onPowerChanged();
        clock.runFor(2 * 60000L);
        A.eq(15000L, lastGap(hub), "screen on: 15 s");
        A.eq(Long.valueOf(15), lastPeriod(hub), "period_s 15");

        p.screenOn = false;
        p.charging = true;
        p.client.onPowerChanged();
        clock.runFor(2 * 60000L);
        A.eq(15000L, lastGap(hub), "charging: 15 s");

        p.charging = false;
        p.client.onPowerChanged();
        clock.runFor(5 * 60000L);
        A.eq(60000L, lastGap(hub), "back to idle: 60 s");
        A.eq(Long.valueOf(60), lastPeriod(hub), "period_s 60 again");
    }

    public void testHeartbeatCarriesTheBeaconFields() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 1);
        Phone p = new Phone(clock, hub, 21);
        p.beacon.put("adb_port", Long.valueOf(37123));
        p.beacon.put("adb_port_source", "nsd");
        p.beacon.put("wifi_adb", Boolean.TRUE);
        p.beacon.put("keeper", "untrusted_bssid");
        p.beacon.put("bssid_trusted", Boolean.FALSE);
        LinkTest.readyUp(clock, p);
        clock.runFor(100);
        Map<String, Object> h = hub.heartbeats.get(0);
        A.eq(Long.valueOf(37123), h.get("adb_port"), "adb_port");
        A.eq("nsd", h.get("adb_port_source"), "source");
        A.eq("untrusted_bssid", h.get("keeper"), "keeper state");
        A.eq(Long.valueOf(80), h.get("battery_pct"), "battery");
        A.ok(h.containsKey("a11y_enabled") && h.get("a11y_enabled") == null, "absent fields are null, never missing");
    }

    public void testSilentHubIsDroppedAndRedialled() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 1);
        final Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        hub.silent = true; // stops acking heartbeats: a half-dead hub
        final int dials = hub.dialTimes.size();
        long t0 = clock.nowMs();
        clock.runUntil(new FakeClock.Until() { public boolean done() { return hub.dialTimes.size() > dials; } }, 10 * 60000L, "redial");
        long waited = clock.nowMs() - t0;
        A.ok(waited >= 2 * 60000L && waited <= 2 * 60000L + 5000 + 60000 + 31000, "dropped after 2 x 60 s + 5 s of silence (waited " + waited + ")");
    }
}
