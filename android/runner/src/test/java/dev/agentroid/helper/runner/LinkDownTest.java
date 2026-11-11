package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Json;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** A laptop sleep: the segment runs to completion with the link down and the report goes out when the link resumes. */
public final class LinkDownTest {
    /** 50 R0/R1 steps: reads, unique taps, 20 s waits, scrolls, back and leased toggles (about 4 min of virtual time). */
    static List<Map<String, Object>> fiftySteps() {
        List<Map<String, Object>> steps = new ArrayList<Map<String, Object>>();
        for (int i = 1; i <= 50; i++) {
            String id = "s" + i;
            switch (i % 5) {
                case 0: steps.add(TestHub.read(id)); break;
                case 1: steps.add(i % 10 == 1 ? TestHub.toggle(id, "key_" + i, i % 20 == 1 ? "1" : "0") : TestHub.tap(id)); break;
                case 2: steps.add(TestHub.waitMs(id, 20_000)); break;
                case 3: steps.add(TestHub.scroll(id)); break;
                default: steps.add(TestHub.back(id)); break;
            }
        }
        return steps;
    }

    @SuppressWarnings("unchecked")
    public void testFiftyStepsCompleteWithTheLinkDownForFiveMinutesAndTheReportFollows() throws Exception {
        Rig r = new Rig(false, false, 1500); // link down from the start; posts hold 1.5 s after each action
        String json = r.signed("seg-sleep", "R1", fiftySteps());
        r.runner.submit(json);
        r.clock.advanceTo(5 * 60_000L - 1); // the laptop sleeps for 5 minutes

        A.eq("completed", r.done.outcome, "finished while the link was down");
        A.ok(r.done.at < 5 * 60_000L, "finished at " + r.done.at + " ms, inside the outage");
        List<Map<String, Object>> steps = (List<Map<String, Object>>) r.report().get("steps");
        A.eq(50, steps.size(), "every step reported");
        for (Map<String, Object> s : steps) {
            A.eq("done", s.get("status"), "status of " + s.get("id"));
            A.eq(Boolean.TRUE, s.get("post_verified"), "post-condition of " + s.get("id"));
        }
        A.eq(0, r.transport.received.size(), "nothing sent while down");
        A.eq(1, r.outbox.pending(), "the report waits");

        // The laptop wakes and the link reaches ready again.
        r.transport.up = true;
        r.clock.schedule(5 * 60_000L - r.clock.nowMs(), new Runnable() { public void run() { r.outbox.onLinkUp(); } });
        r.clock.advanceTo(5 * 60_000L);
        A.eq(1, r.transport.received.size(), "delivered after resume");
        A.eq(r.done.report, r.transport.received.get(0), "the queued report, unchanged");
        A.eq(0, r.outbox.pending(), "queue empty");
        r.outbox.onLinkUp();
        A.eq(1, r.transport.received.size(), "delivered exactly once");

        System.out.println("  link-down: 50 steps done at virtual " + r.done.at / 1000.0 + " s with the link down; report delivered at 300 s");

        // For the Rust supervisor cross-check (crates/segments test reads it when present).
        Map<String, Object> dump = new LinkedHashMap<String, Object>();
        dump.put("hub_spki", Base64.getEncoder().encodeToString(r.hub.spki()));
        dump.put("device", Rig.SERIAL);
        dump.put("epoch", Long.valueOf(Rig.EPOCH));
        dump.put("now_s", Long.valueOf(Rig.NOW / 1000));
        dump.put("signed", json);
        dump.put("report", r.done.report);
        Files.write(A.out().resolve("runner-sample.json"), Json.write(dump).getBytes(StandardCharsets.UTF_8));
    }

    public void testALinkThatFlapsDuringTheRunChangesNothing() throws Exception {
        final Rig r = new Rig(true, false, 1500);
        r.runner.submit(r.signed("seg-flap", "R1", fiftySteps()));
        for (int t = 10_000; t < 240_000; t += 20_000) {
            final boolean up = (t / 20_000) % 2 == 0;
            r.clock.schedule(t - r.clock.nowMs(), new Runnable() { public void run() { r.transport.up = up; if (up) r.outbox.onLinkUp(); } });
        }
        r.clock.runIdle(10 * 60_000L);
        A.eq("completed", r.done.outcome, "flapping link");
        r.transport.up = true;
        r.outbox.onLinkUp();
        A.eq(1, r.transport.received.size(), "one report");
    }
}
